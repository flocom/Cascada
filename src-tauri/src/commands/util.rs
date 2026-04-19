use crate::core::state::AppState;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::State;

pub type S<'a> = State<'a, Arc<AppState>>;

pub fn err<E: std::fmt::Display>(e: E) -> String { e.to_string() }

pub fn push_unique(v: &mut Vec<PathBuf>, p: PathBuf) {
    if !v.contains(&p) { v.push(p); }
}

pub fn whoami() -> String {
    std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default()
}

/// All Wine/CrossOver/PlayOnMac/Bottles/Lutris/Heroic/Proton prefixes under `home`.
pub fn wine_prefixes(home: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    let fixed = [
        ".wine",
        "Library/Application Support/CrossOver/Bottles",
        "Library/PlayOnMac/wineprefix",
        ".local/share/bottles/bottles",
        ".var/app/com.usebottles.bottles/data/bottles/bottles",
        ".local/share/wineprefixes",
        ".steam/steam/steamapps/compatdata",
        ".local/share/Steam/steamapps/compatdata",
    ];
    for f in fixed {
        let p = home.join(f);
        if !p.exists() { continue; }
        if p.join("drive_c").is_dir() { v.push(p.clone()); }
        if let Ok(rd) = std::fs::read_dir(&p) {
            for e in rd.flatten() {
                let inner = e.path();
                if inner.join("drive_c").is_dir() { v.push(inner.clone()); }
                let pfx = inner.join("pfx");
                if pfx.join("drive_c").is_dir() { v.push(pfx); }
            }
        }
    }
    v
}

pub fn wine_user_dirs(pfx: &Path) -> Vec<PathBuf> {
    let users = pfx.join("drive_c").join("users");
    let Ok(rd) = std::fs::read_dir(&users) else { return Vec::new() };
    rd.flatten().map(|e| e.path()).collect()
}
