use super::util::{err, push_unique, wine_prefixes, wine_user_dirs};
use crate::core::model::Platform;
use std::path::{Path, PathBuf};

pub(crate) const MT4_EA_SRC: &str     = include_str!("../../../ea/mt4/CascadaBridge.mq4");
pub(crate) const MT5_EA_SRC: &str     = include_str!("../../../ea/mt5/CascadaBridge.mq5");
/// Pre-compiled binaries produced by the `Compile EAs` GitHub workflow.
/// Shipping them alongside the source means MT4/MT5 runs the EA on first
/// launch even on terminals without MetaEditor (headless Wine, etc).
pub(crate) const MT4_EA_BIN: &[u8]    = include_bytes!("../../../ea/mt4/compiled/CascadaBridge.ex4");
pub(crate) const MT5_EA_BIN: &[u8]    = include_bytes!("../../../ea/mt5/compiled/CascadaBridge.ex5");

struct MtAssets {
    subdir: &'static str,       // "MQL4" / "MQL5"
    src_name: &'static str,     // "CascadaBridge.mq4" / "CascadaBridge.mq5"
    src_body: &'static str,     // embedded source
    bin_name: &'static str,     // "CascadaBridge.ex4" / "CascadaBridge.ex5"
    bin_body: &'static [u8],    // embedded compiled binary
}

fn mt_assets(platform: Platform) -> Result<MtAssets, String> {
    match platform {
        Platform::MT4 => Ok(MtAssets {
            subdir: "MQL4", src_name: "CascadaBridge.mq4", src_body: MT4_EA_SRC,
            bin_name: "CascadaBridge.ex4", bin_body: MT4_EA_BIN,
        }),
        Platform::MT5 => Ok(MtAssets {
            subdir: "MQL5", src_name: "CascadaBridge.mq5", src_body: MT5_EA_SRC,
            bin_name: "CascadaBridge.ex5", bin_body: MT5_EA_BIN,
        }),
        Platform::CTrader => Err("cTrader uses install_ctrader_bot".into()),
    }
}

/// Writes both the `.mq?` source and the pre-compiled `.ex?` to the Experts
/// folder. Returns the binary path (what MT actually loads) so the UI can
/// show a useful confirmation. On terminals with MetaEditor present, the
/// source lets the user inspect / tweak; on headless terminals the `.ex?`
/// takes over and the EA runs immediately.
async fn write_mt_to(experts: &Path, a: &MtAssets) -> Result<String, String> {
    tokio::fs::create_dir_all(experts).await.map_err(err)?;
    tokio::fs::write(experts.join(a.src_name), a.src_body).await.map_err(err)?;
    let bin_path = experts.join(a.bin_name);
    tokio::fs::write(&bin_path, a.bin_body).await.map_err(err)?;
    Ok(bin_path.to_string_lossy().into_owned())
}

#[tauri::command]
pub async fn install_mt_ea_at(platform: Platform, path: String) -> Result<String, String> {
    let a = mt_assets(platform)?;
    let base = Path::new(&path);
    let experts = if base.ends_with("Experts") { base.to_path_buf() }
        else if base.join(a.subdir).join("Experts").is_dir() { base.join(a.subdir).join("Experts") }
        else if base.ends_with(a.subdir) { base.join("Experts") }
        else { base.join(a.subdir).join("Experts") };
    write_mt_to(&experts, &a).await
}

#[tauri::command]
pub async fn install_mt_ea(platform: Platform) -> Result<Vec<String>, String> {
    let a = mt_assets(platform)?;
    let subdir = a.subdir;
    let roots = tokio::task::spawn_blocking(move || discover_mt_terminals(subdir)).await.map_err(err)?;
    if roots.is_empty() {
        return Err(format!("No {} terminal found. Use 'Pick location…' instead.",
                           if subdir == "MQL4" { "MetaTrader 4" } else { "MetaTrader 5" }));
    }
    let mut written = Vec::with_capacity(roots.len());
    for experts in roots {
        let p = write_mt_to(&experts, &a).await?;
        written.push(p);
    }
    Ok(written)
}

/// All `…/MetaQuotes/Terminal` directories where MT4/MT5 may live: canonical
/// AppData paths, known Wine/CrossOver/PlayOnMac/Bottles/Lutris/Heroic/Proton
/// prefixes, broker app bundles. Used by both EA installation (`<HASH>/<MQL?>/
/// Experts` siblings) and Common Files discovery (`Common/Files` sibling).
pub(crate) fn mt_terminal_parents() -> Vec<PathBuf> {
    let mut term_parents: Vec<PathBuf> = Vec::new();
    let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    let push_terminal = |v: &mut Vec<PathBuf>, base: PathBuf| {
        push_unique(v, base.join("MetaQuotes").join("Terminal"));
    };

    for env in ["APPDATA", "LOCALAPPDATA", "ProgramData", "USERPROFILE"] {
        if let Ok(v) = std::env::var(env) { push_terminal(&mut term_parents, PathBuf::from(v)); }
    }
    if let Some(h) = &home {
        push_terminal(&mut term_parents, h.join("AppData").join("Roaming"));
        push_terminal(&mut term_parents, h.join("AppData").join("Local"));
        push_terminal(&mut term_parents, h.join("Library").join("Application Support"));
    }

    if let Some(h) = &home {
        for pfx in wine_prefixes(h) {
            for user_docs in wine_user_dirs(&pfx) {
                push_terminal(&mut term_parents, user_docs.join("AppData").join("Roaming"));
                push_terminal(&mut term_parents, user_docs.join("AppData").join("Local"));
            }
            for pf in [pfx.join("drive_c/Program Files"),
                       pfx.join("drive_c/Program Files (x86)")] {
                push_unique(&mut term_parents, pf);
            }
        }
    }
    if let Ok(wp) = std::env::var("WINEPREFIX") {
        let pfx = PathBuf::from(&wp);
        for user_docs in wine_user_dirs(&pfx) {
            push_terminal(&mut term_parents, user_docs.join("AppData").join("Roaming"));
        }
    }

    for app in app_install_dirs_mt() {
        for sub in ["Contents/SharedSupport/prefix", "Contents/Resources/wineprefix",
                    "Contents/Resources/prefix", "Contents/Frameworks/wine",
                    "drive_c", "prefix", "wine", "wineprefix"] {
            let pfx = if sub == "drive_c" { app.clone() } else { app.join(sub) };
            let real_pfx = if pfx.join("drive_c").is_dir() { pfx } else { continue };
            for user_docs in wine_user_dirs(&real_pfx) {
                push_terminal(&mut term_parents, user_docs.join("AppData").join("Roaming"));
            }
            for pf in [real_pfx.join("drive_c/Program Files"),
                       real_pfx.join("drive_c/Program Files (x86)")] {
                push_unique(&mut term_parents, pf);
            }
        }
        push_unique(&mut term_parents, app.clone());
    }
    term_parents
}

/// All `…/MetaQuotes/Terminal/Common/Files` directories. This is where MT4/MT5
/// EAs write when they call `FileOpen(..., FILE_COMMON)` — shared across every
/// terminal in the same Wine prefix. Cascada scans these for the per-login
/// subfolders the EA creates under `Cascada/<MT4|MT5>/<login>/`.
pub(crate) fn mt_common_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for parent in mt_terminal_parents() {
        let c = parent.join("Common").join("Files");
        if c.is_dir() { push_unique(&mut out, c); }
    }
    // Belt-and-braces: derive Common from any Experts dirs we found via deep
    // scan (covers portable installs the targeted parents miss).
    for sub in &["MQL4", "MQL5"] {
        for experts in discover_mt_terminals(sub) {
            if let Some(common) = experts.parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
                .map(|p| p.join("Common").join("Files"))
            {
                if common.is_dir() { push_unique(&mut out, common); }
            }
        }
    }
    out
}

/// Locate every `<…>/<subdir>/Experts` directory wherever an MT4/MT5 terminal
/// could keep its files. `subdir` is "MQL4" or "MQL5".
pub(crate) fn discover_mt_terminals(subdir: &str) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());

    for parent in mt_terminal_parents() {
        let Ok(rd) = std::fs::read_dir(&parent) else { continue };
        for entry in rd.flatten() {
            let experts = entry.path().join(subdir).join("Experts");
            if experts.is_dir() { push_unique(&mut roots, experts); }
        }
    }

    let mut scan_roots: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "windows") {
        for env in ["ProgramFiles", "ProgramFiles(x86)", "ProgramData",
                    "LOCALAPPDATA", "APPDATA", "USERPROFILE"] {
            if let Ok(v) = std::env::var(env) { push_unique(&mut scan_roots, PathBuf::from(v)); }
        }
        for letter in ['C', 'D', 'E'] {
            push_unique(&mut scan_roots, PathBuf::from(format!("{letter}:\\")));
        }
    }
    if cfg!(target_os = "macos") {
        push_unique(&mut scan_roots, PathBuf::from("/Applications"));
        if let Some(h) = &home {
            push_unique(&mut scan_roots, h.join("Applications"));
            push_unique(&mut scan_roots, h.join("Library/Application Support"));
            push_unique(&mut scan_roots, h.join("Library/Containers"));
        }
    }
    if cfg!(target_os = "linux") {
        push_unique(&mut scan_roots, PathBuf::from("/opt"));
        push_unique(&mut scan_roots, PathBuf::from("/usr/local"));
        if let Some(h) = &home {
            push_unique(&mut scan_roots, h.join(".local/share"));
            push_unique(&mut scan_roots, h.join("Games"));
            push_unique(&mut scan_roots, h.clone());
        }
    }
    if let Some(h) = &home {
        for pfx in wine_prefixes(h) {
            push_unique(&mut scan_roots, pfx.join("drive_c"));
        }
    }
    for app in app_install_dirs_mt() { push_unique(&mut scan_roots, app); }

    let want = subdir.to_string();
    for root in scan_roots {
        deep_scan_for_experts(&root, &want, 6, &mut roots);
    }

    roots
}

/// Walk `dir` up to `depth` levels deep, push every `…/<subdir>/Experts` found.
/// Skips obviously-irrelevant subtrees to keep the scan cheap.
fn deep_scan_for_experts(dir: &Path, subdir: &str, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 { return; }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() { continue; }
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if matches!(name, "node_modules" | ".git" | ".cache" | "Library"
                          | "$Recycle.Bin" | "$RECYCLE.BIN" | "System Volume Information"
                          | "Windows" | "WindowsApps" | "WinSxS"
                          | "Photos" | "Movies" | "Music" | "Videos"
                          | "node_modules.bin") { continue; }
        if name.eq_ignore_ascii_case(subdir) {
            let experts = p.join("Experts");
            if experts.is_dir() { push_unique(out, experts); }
            continue;
        }
        let experts = p.join(subdir).join("Experts");
        if experts.is_dir() { push_unique(out, experts); }
        deep_scan_for_experts(&p, subdir, depth - 1, out);
    }
}

fn app_install_dirs_mt() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    let mut bases: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        bases.push(PathBuf::from("/Applications"));
        if let Some(b) = directories::BaseDirs::new() {
            bases.push(b.home_dir().join("Applications"));
        }
    } else if cfg!(target_os = "windows") {
        for env in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
            if let Ok(v) = std::env::var(env) { bases.push(PathBuf::from(v)); }
        }
        bases.push(PathBuf::from(r"C:\Program Files"));
        bases.push(PathBuf::from(r"C:\Program Files (x86)"));
    } else {
        bases.push(PathBuf::from("/opt"));
        bases.push(PathBuf::from("/usr/local"));
        if let Some(b) = directories::BaseDirs::new() {
            bases.push(b.home_dir().join(".local/share"));
        }
    }
    let needles = ["metatrader", "mt4", "mt5", "metaquotes",
                   "ic markets", "icmarkets", "ftmo", "pepperstone",
                   "exness", "xm", "fxpro", "fxtm", "tickmill",
                   "axi", "alpari", "fbs", "hf markets", "hfmarkets",
                   "vantage", "rocheston", "admiral", "swissquote"];
    for base in bases {
        let Ok(rd) = std::fs::read_dir(&base) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            if needles.iter().any(|n| name.contains(n)) { v.push(p); }
        }
    }
    v
}
