use super::util::{err, push_unique, whoami, wine_prefixes};
use std::path::{Path, PathBuf};

const CTRADER_BOT_SRC: &str = include_str!("../../../ea/ctrader/CascadaBridge.cs");
pub(crate) const CTRADER_BOT_ALGO: &[u8] = include_bytes!("../../../ea/ctrader/prebuilt/CascadaBridge.algo");

pub(crate) fn discover_ctrader_roots() -> Vec<PathBuf> {
    let mut docs: Vec<PathBuf> = Vec::new();
    let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());

    if let Some(ud) = directories::UserDirs::new() {
        if let Some(d) = ud.document_dir() { push_unique(&mut docs, d.to_path_buf()); }
    }
    if let Some(h) = &home {
        // Native macOS cTrader stores cBots directly in ~/cAlgo; also scan home itself.
        push_unique(&mut docs, h.clone());
        for n in ["Documents", "Mes documents", "Dokumente", "Documenti", "Documentos",
                  "OneDrive/Documents", "OneDrive/Documenti", "OneDrive - Personal/Documents"] {
            push_unique(&mut docs, h.join(n));
        }
        for pfx in wine_prefixes(h) {
            let users = pfx.join("drive_c").join("users");
            let Ok(rd) = std::fs::read_dir(&users) else { continue };
            for e in rd.flatten() {
                let u = e.path();
                for n in ["Documents", "My Documents", "Mes documents"] {
                    push_unique(&mut docs, u.join(n));
                }
            }
        }
    }
    if let Ok(wp) = std::env::var("WINEPREFIX") {
        push_unique(&mut docs, PathBuf::from(&wp).join("drive_c/users").join(whoami()).join("Documents"));
    }
    for app in app_install_dirs() {
        for sub in ["Contents/SharedSupport/prefix", "Contents/Resources/wineprefix",
                    "Contents/Resources/prefix", "prefix", "wine", "wineprefix"] {
            let pfx = app.join(sub);
            if pfx.join("drive_c").is_dir() {
                if let Ok(rd) = std::fs::read_dir(pfx.join("drive_c/users")) {
                    for e in rd.flatten() {
                        push_unique(&mut docs, e.path().join("Documents"));
                    }
                }
            }
        }
        push_unique(&mut docs, app.clone());
    }

    let mut out: Vec<PathBuf> = Vec::new();
    for base in docs {
        let Ok(rd) = std::fs::read_dir(&base) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.is_dir() { continue; }
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            if !(name.contains("calgo") || name.contains("ctrader")) { continue; }
            let robots = p.join("Sources").join("Robots");
            if robots.is_dir() { push_unique(&mut out, robots); }
        }
    }
    out
}

fn app_install_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    let bases: Vec<PathBuf> = if cfg!(target_os = "macos") {
        vec![PathBuf::from("/Applications"),
             directories::BaseDirs::new().map(|b| b.home_dir().join("Applications")).unwrap_or_default()]
    } else if cfg!(target_os = "windows") {
        ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA", "APPDATA"].iter()
            .filter_map(|k| std::env::var_os(k).map(PathBuf::from))
            .chain(std::iter::once(PathBuf::from(r"C:\Program Files")))
            .chain(std::iter::once(PathBuf::from(r"C:\Program Files (x86)")))
            .collect()
    } else {
        vec![PathBuf::from("/opt"), PathBuf::from("/usr/local"),
             directories::BaseDirs::new().map(|b| b.home_dir().join(".local/share")).unwrap_or_default()]
    };
    for base in bases {
        let Ok(rd) = std::fs::read_dir(&base) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            if name.contains("ctrader") || name.contains("calgo") || name.contains("spotware") {
                v.push(p);
            }
        }
    }
    v
}

async fn write_bot_to(root: &Path) -> Result<String, String> {
    let calgo_root = if root.ends_with("Robots") && root.parent().map(|p| p.ends_with("Sources")).unwrap_or(false) {
        root.parent().and_then(|p| p.parent()).unwrap_or(root).to_path_buf()
    } else {
        root.to_path_buf()
    };
    if !CTRADER_BOT_ALGO.is_empty() {
        let dir = calgo_root.join("cBots").join("CascadaBridge");
        tokio::fs::create_dir_all(&dir).await.map_err(err)?;
        let path = dir.join("CascadaBridge.algo");
        tokio::fs::write(&path, CTRADER_BOT_ALGO).await.map_err(err)?;
        Ok(path.to_string_lossy().into_owned())
    } else {
        let dir = calgo_root.join("Sources").join("Robots").join("CascadaBridge").join("CascadaBridge");
        tokio::fs::create_dir_all(&dir).await.map_err(err)?;
        let path = dir.join("CascadaBridge.cs");
        tokio::fs::write(&path, CTRADER_BOT_SRC).await.map_err(err)?;
        Ok(path.to_string_lossy().into_owned())
    }
}

#[tauri::command]
pub async fn install_ctrader_bot() -> Result<Vec<String>, String> {
    let roots = tokio::task::spawn_blocking(discover_ctrader_roots).await.map_err(err)?;
    if roots.is_empty() {
        return Err("No cTrader / cAlgo installation found. Use 'Add manual location'.".into());
    }
    let mut written = Vec::with_capacity(roots.len());
    for root in roots {
        let path = write_bot_to(&root).await?;
        trigger_ctrader_import(&path);
        written.push(path);
    }
    Ok(written)
}

#[tauri::command]
pub async fn install_ctrader_bot_at(path: String) -> Result<String, String> {
    let written = write_bot_to(Path::new(&path)).await?;
    trigger_ctrader_import(&written);
    Ok(written)
}

/// Route the .algo through the OS so cTrader's handler picks it up and prompts
/// the one-click "Install cBot?" dialog. No-op for .cs source installs.
fn trigger_ctrader_import(path: &str) {
    if !path.ends_with(".algo") { return; }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", path]).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}
