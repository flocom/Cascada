//! Compare installed EA / cBot artifacts with the ones this build of
//! Cascada embeds.
//!
//! For MT4/MT5 we compare the `.mq4`/`.mq5` SOURCE, not the compiled
//! `.ex4`/`.ex5` — MetaEditor's compilation is non-deterministic
//! (timestamps, build IDs baked into the binary), so byte-comparing the
//! compiled output produced "out of date" on every Cascada release even
//! when the source hadn't changed. Source bytes are stable: same source
//! → same bytes → no spurious update prompts. Cascada writes both the
//! source and the compiled binary side-by-side on install (see
//! `install_mt.rs::write_mt_to`), so the source file is always present
//! when the user installed via Cascada.
//!
//! For cTrader we still byte-compare the `.algo` because it's a signed
//! container we ship pre-built, and Spotware's signing IS deterministic
//! enough not to flap.
//!
//! Returns one `EaStatus` per discovered install location so the UI can
//! show exactly which terminal(s) are out of date.
use super::install_ctrader::{discover_ctrader_roots, CTRADER_BOT_ALGO};
use super::install_mt::{discover_mt_terminals, MT4_EA_SRC, MT5_EA_SRC};
use crate::core::model::Platform;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize)]
pub struct EaStatus {
    pub platform: Platform,
    pub path: String,
    pub up_to_date: bool,
    pub installed_bytes: u64,
    pub bundled_bytes: u64,
}

#[tauri::command]
pub async fn check_ea_versions() -> Result<Vec<EaStatus>, String> {
    // Discovery touches the filesystem (sometimes deeply). Run on the
    // blocking pool so we don't stall the Tauri event loop when the user
    // has dozens of Wine prefixes.
    let results = tokio::task::spawn_blocking(gather_sync)
        .await
        .map_err(|e| e.to_string())?;
    Ok(results)
}

fn gather_sync() -> Vec<EaStatus> {
    let mut out: Vec<EaStatus> = Vec::new();

    // MT4 — compare source (.mq4), not the non-deterministically compiled .ex4.
    for experts in discover_mt_terminals("MQL4") {
        if let Some(s) = compare_file(
            experts.join("CascadaBridge.mq4"),
            Platform::MT4,
            MT4_EA_SRC.as_bytes(),
        ) {
            out.push(s);
        }
    }

    // MT5 — same, compare source (.mq5).
    for experts in discover_mt_terminals("MQL5") {
        if let Some(s) = compare_file(
            experts.join("CascadaBridge.mq5"),
            Platform::MT5,
            MT5_EA_SRC.as_bytes(),
        ) {
            out.push(s);
        }
    }

    // cTrader — `cBots/CascadaBridge/CascadaBridge.algo`. `discover_ctrader_roots`
    // returns `<calgo>/Sources/Robots` (that's what the installer needs for the
    // .cs fallback), so walk two levels up to land on the cAlgo documents root
    // where the deployed .algo actually lives.
    for root in discover_ctrader_roots() {
        let calgo_root = if root.ends_with("Robots")
            && root.parent().map(|p| p.ends_with("Sources")).unwrap_or(false)
        {
            root.parent().and_then(|p| p.parent()).unwrap_or(&root).to_path_buf()
        } else {
            root.clone()
        };
        let candidates = [
            calgo_root.join("cBots").join("CascadaBridge").join("CascadaBridge.algo"),
            calgo_root.join("Sources").join("Robots").join("CascadaBridge")
                .join("CascadaBridge").join("bin").join("Release").join("CascadaBridge.algo"),
        ];
        for p in candidates {
            if let Some(s) = compare_file(p, Platform::CTrader, CTRADER_BOT_ALGO) {
                out.push(s);
            }
        }
    }

    out
}

fn compare_file(path: PathBuf, platform: Platform, bundled: &[u8]) -> Option<EaStatus> {
    if !path.is_file() {
        return None;
    }
    let installed = std::fs::read(&path).ok()?;
    Some(EaStatus {
        platform,
        path: path.to_string_lossy().into_owned(),
        up_to_date: installed == bundled,
        installed_bytes: installed.len() as u64,
        bundled_bytes: bundled.len() as u64,
    })
}
