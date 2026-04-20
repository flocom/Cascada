//! Compare installed EA / cBot binaries with the ones this build of Cascada
//! embeds. The comparison is a byte-for-byte check against the embedded
//! `include_bytes!` constants — no file timestamp or version string needed.
//!
//! Returns one `EaStatus` per discovered install location so the UI can
//! show exactly which terminal(s) are out of date.
//!
//! "Out of date" here means: the file on disk differs from what the
//! current Cascada bundle would install. If the user edited the EA by
//! hand that counts as out of date too — running the updater overwrites
//! their edit, which is the usual expectation.
use super::install_ctrader::{discover_ctrader_roots, CTRADER_BOT_ALGO};
use super::install_mt::{discover_mt_terminals, MT4_EA_BIN, MT5_EA_BIN};
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

    // MT4 — `MQL4/Experts/CascadaBridge.ex4`
    for experts in discover_mt_terminals("MQL4") {
        if let Some(s) = compare_file(
            experts.join("CascadaBridge.ex4"),
            Platform::MT4,
            MT4_EA_BIN,
        ) {
            out.push(s);
        }
    }

    // MT5 — `MQL5/Experts/CascadaBridge.ex5`
    for experts in discover_mt_terminals("MQL5") {
        if let Some(s) = compare_file(
            experts.join("CascadaBridge.ex5"),
            Platform::MT5,
            MT5_EA_BIN,
        ) {
            out.push(s);
        }
    }

    // cTrader — `cBots/CascadaBridge/CascadaBridge.algo`. Different cTrader
    // installs expose the folder under slightly different roots; the
    // installer discovery is the source of truth — walk it the same way.
    for root in discover_ctrader_roots() {
        let candidates = [
            root.join("cBots").join("CascadaBridge").join("CascadaBridge.algo"),
            root.join("Sources").join("Robots").join("CascadaBridge")
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
