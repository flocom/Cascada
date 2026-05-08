//! TradingView bridge — discovery + spawn for TV-backed accounts.
//!
//! TV is a browser product, so the broker API is intercepted by an external
//! Python sidecar (`tv-proxy/cascada_addon.py`, a mitmproxy addon shipped
//! in this repo) that watches the user's TradingView session and writes
//! events to a local folder in the same wire format as the cTrader cBot /
//! MT EA. Cascada then attaches via the generic `file_bridge::spawn_with_dir`
//! — no TV-specific runtime path.
//!
//! Discovery folder layout:
//!
//! ```text
//! <cascada_root>/TradingView/<login>/events.jsonl   (proxy → Cascada)
//! <cascada_root>/TradingView/<login>/cmd.jsonl      (Cascada → proxy)
//! ```
//!
//! `<login>` is whatever the proxy chose to identify the TV PaperTrading or
//! TV-broker account (typically the TV `account_id`). Multiple TV sessions
//! are supported — one folder per account, just like MT4/MT5.
//!
//! TV is currently used as a **master** (read-only) — the proxy translates
//! TV order/position events into `S2C::Open`/`Close`/`Modify`/`Pending*`.
//! The cmd.jsonl side is allocated so a future "close from Cascada" or
//! "modify SL/TP from Cascada" can land without a protocol change.

use crate::connectors::file_bridge::{cascada_root, spawn_with_dir};
use crate::connectors::ConnectorHandle;
use crate::core::events::LogLevel;
use crate::core::model::*;
use crate::core::state::AppState;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};

/// Subfolder name under `cascada_root()`. The cTrader-discovery loop must
/// skip this name so it doesn't try to register the TV folder as a cTrader
/// login.
pub const TV_SUBDIR: &str = "TradingView";

pub fn spawn_discovery(app: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(3));
        loop {
            tick.tick().await;
            let Some(root) = cascada_root() else { continue };
            let dir = root.join(TV_SUBDIR);
            let Ok(mut rd) = tokio::fs::read_dir(&dir).await else { continue };
            while let Ok(Some(entry)) = rd.next_entry().await {
                let p = entry.path();
                if !p.is_dir() { continue; }
                // Take ownership of the login string up-front; the path is
                // moved into `spawn_with_dir` below, after which a borrow
                // tied to its bytes would dangle.
                let Some(login) = p.file_name().and_then(|s| s.to_str()).map(str::to_owned) else {
                    continue
                };
                if login.is_empty() { continue; }
                if !tokio::fs::try_exists(p.join("events.jsonl")).await.unwrap_or(false) { continue; }
                let acct = app.find_or_create_tv_account(&login).await;
                if app.connectors.contains_key(&acct.id) { continue; }
                let (tx, rx) = mpsc::channel::<ConnectorCmd>(256);
                spawn_with_dir(acct.clone(), p, rx, app.event_tx.clone());
                app.connectors.insert(acct.id.clone(), ConnectorHandle { tx });
                app.emit_log(LogLevel::Info, &acct.id,
                    format!("attached TradingView session {login}"));
            }
        }
    });
}
