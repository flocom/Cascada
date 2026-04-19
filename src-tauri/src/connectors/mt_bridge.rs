//! MT4/MT5 bridge — file-based IPC, identical pattern to the cTrader file
//! bridge but discovered under each terminal's `Common/Files/Cascada/<MT?>/`
//! folder (the EA writes there via the `FILE_COMMON` flag).
//!
//! No network, no whitelist — each EA terminal silently spawns its own folder
//! the moment it attaches to a chart. Multiple terminals are supported in
//! parallel (one folder per login, optionally distributed across multiple
//! Wine prefixes on macOS/Linux).

use crate::commands::mt_common_dirs;
use crate::connectors::file_bridge;
use crate::connectors::ConnectorHandle;
use crate::core::events::LogLevel;
use crate::core::model::*;
use crate::core::state::AppState;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};

const PLATFORMS: &[(Platform, &str)] = &[(Platform::MT4, "MT4"), (Platform::MT5, "MT5")];

pub fn spawn_discovery(app: Arc<AppState>) {
    tokio::spawn(async move {
        // Resolve the list of `Common/Files` parents **once** at startup.
        // Walking Wine/Bottles/CrossOver and `~/Library/Containers/*` on every
        // tick was re-triggering macOS Sequoia's TCC "access data from other
        // apps" prompt in a loop. Now we do the expensive cross-container walk
        // a single time; the 3 s discovery tick only reads the already-resolved
        // `Common/Files/Cascada/<MT?>/` subtree (same sandbox scope, no prompt).
        // New installs mid-session: user can re-run "Auto-install EA" which
        // goes through `install_mt_ea`, or simply restart the app.
        let commons: Arc<[PathBuf]> = Arc::from(
            tokio::task::spawn_blocking(mt_common_dirs)
                .await
                .unwrap_or_default(),
        );
        let mut tick = interval(Duration::from_secs(3));
        loop {
            tick.tick().await;
            let commons = Arc::clone(&commons);
            let logins = tokio::task::spawn_blocking(move || scan_commons(&commons))
                .await
                .unwrap_or_default();
            for (platform, login, dir) in logins {
                let acct = app.find_or_create_mt_account(platform, &login, "").await;
                if app.connectors.contains_key(&acct.id) { continue; }
                let (tx, rx) = mpsc::channel::<ConnectorCmd>(256);
                file_bridge::spawn_with_dir(acct.clone(), dir, rx, app.event_tx.clone());
                app.connectors.insert(acct.id.clone(), ConnectorHandle { tx });
                app.emit_log(LogLevel::Info, &acct.id,
                    format!("attached {:?} login {login}", platform));
            }
        }
    });
}

/// Walk every `<Common>/Files/Cascada/<MT?>/<login>/` folder that contains an
/// `events.jsonl`. Returns `(platform, login, dir)` triples. Uses the
/// pre-resolved `commons` list so we don't re-scan cross-app containers.
fn scan_commons(commons: &[PathBuf]) -> Vec<(Platform, String, PathBuf)> {
    let mut out: Vec<(Platform, String, PathBuf)> = Vec::new();
    for common in commons {
        for (platform, sub) in PLATFORMS {
            let root = common.join("Cascada").join(sub);
            let Ok(rd) = std::fs::read_dir(&root) else { continue };
            for entry in rd.flatten() {
                let p = entry.path();
                if !p.is_dir() { continue; }
                let Some(login) = p.file_name().and_then(|s| s.to_str()) else { continue };
                if login.is_empty() { continue; }
                if !p.join("events.jsonl").is_file() { continue; }
                if out.iter().any(|(pl, lg, _)| *pl == *platform && lg == login) { continue; }
                out.push((*platform, login.to_string(), p));
            }
        }
    }
    out
}
