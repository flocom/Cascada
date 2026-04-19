//! File-based bridge — cTrader cBot writes to `~/cAlgo/Cascada/<login>/`,
//! MT4/MT5 EAs write to `<MetaQuotes>/Terminal/Common/Files/Cascada/<MT?>/<login>/`.
//! Same wire format on both sides; only the directory differs. Cascada truncates
//! `cmd.jsonl` on attach (fresh session); each side tracks its read offset and
//! resets to 0 if the file shrinks (counterpart rotated).

use crate::connectors::emit_log;
use crate::connectors::proto::{dispatch, C2S, S2C};
use crate::core::events::LogLevel;
use crate::core::model::*;
use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration, Instant};

/// Poll cadence: tight while active, backs off when silent so idle bridges
/// don't burn CPU on stat()/read() every 250 ms.
const POLL_MIN: Duration = Duration::from_millis(100);
const POLL_MAX: Duration = Duration::from_millis(1000);
const POLL_IDLE_AFTER: Duration = Duration::from_secs(10);

/// cTrader entry point — resolves dir from `cascada_root() / login`.
pub fn spawn(
    account: Account,
    cmd_rx: mpsc::Receiver<ConnectorCmd>,
    events: mpsc::UnboundedSender<ConnectorEvent>,
) {
    let dir = cascada_root().unwrap_or_else(|| PathBuf::from(".")).join(&account.login);
    spawn_with_dir(account, dir, cmd_rx, events);
}

/// Generic worker — used by cTrader (above) and MT discovery (mt_bridge).
pub fn spawn_with_dir(
    account: Account,
    dir: PathBuf,
    mut cmd_rx: mpsc::Receiver<ConnectorCmd>,
    events: mpsc::UnboundedSender<ConnectorEvent>,
) {
    tokio::spawn(async move {
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            emit_log(&events, &account.id, LogLevel::Error,
                format!("create dir {}: {e}", dir.display()));
            return;
        }
        let cmd_path = dir.join("cmd.jsonl");
        let evt_path = dir.join("events.jsonl");
        let _ = tokio::fs::write(&cmd_path, b"").await;
        emit_log(&events, &account.id, LogLevel::Info,
            format!("file bridge → {}", dir.display()));

        let mut evt_offset: u64 = tokio::fs::metadata(&evt_path).await
            .map(|m| m.len()).unwrap_or(0);
        // Persistent reader — kept open across polls so we don't pay an open()
        // + seek() syscall every 250 ms. Reopened on rotation (len < offset) or
        // on transient IO errors.
        let mut reader: Option<BufReader<File>> = None;
        let mut last_activity = Instant::now();
        let mut alive_since: Option<Instant> = None;
        const STALL_AFTER: Duration = Duration::from_secs(10);
        let mut cur_delay = POLL_MIN;

        loop {
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => {
                    if matches!(cmd, ConnectorCmd::Shutdown) {
                        let _ = events.send(ConnectorEvent::Disconnected {
                            account_id: account.id.clone(),
                        });
                        break;
                    }
                    if let Err(e) = append_cmd(&cmd_path, &cmd).await {
                        emit_log(&events, &account.id, LogLevel::Warn,
                            format!("append cmd: {e}"));
                    }
                }
                _ = sleep(cur_delay) => {
                    match read_new_events(&evt_path, &mut evt_offset, &mut reader).await {
                        Ok(lines) => {
                            if !lines.is_empty() {
                                alive_since = Some(Instant::now());
                                last_activity = Instant::now();
                                cur_delay = POLL_MIN;
                            } else if last_activity.elapsed() > POLL_IDLE_AFTER {
                                // Backoff toward POLL_MAX when nothing arrives.
                                cur_delay = (cur_delay * 2).min(POLL_MAX);
                            }
                            for line in lines {
                                match serde_json::from_str::<S2C>(&line) {
                                    Ok(msg) => dispatch(&account, msg, &events),
                                    Err(e) => emit_log(&events, &account.id, LogLevel::Warn,
                                        format!("bad frame: {e}")),
                                }
                            }
                        },
                        Err(e) => {
                            reader = None; // force reopen next tick
                            emit_log(&events, &account.id, LogLevel::Warn,
                                format!("read events: {e}"));
                        }
                    }
                    if alive_since.is_some_and(|t| t.elapsed() > STALL_AFTER) {
                        alive_since = None;
                        let _ = events.send(ConnectorEvent::Disconnected {
                            account_id: account.id.clone(),
                        });
                    }
                }
            }
        }
    });
}

/// Root folder where the cTrader cBot writes per-login subfolders.
pub fn cascada_root() -> Option<PathBuf> {
    let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
    let mac_native = home.join("cAlgo");
    let base = if cfg!(target_os = "macos") && mac_native.is_dir() {
        mac_native
    } else {
        directories::UserDirs::new()
            .and_then(|u| u.document_dir().map(|d| d.join("cAlgo")))
            .unwrap_or_else(|| home.join("cAlgo"))
    };
    Some(base.join("Cascada"))
}

async fn append_cmd(path: &Path, cmd: &ConnectorCmd) -> Result<()> {
    let Some(frame) = C2S::from_cmd(cmd) else { return Ok(()) };
    let json = serde_json::to_string(&frame)?;
    let mut f = OpenOptions::new().create(true).append(true).open(path).await?;
    f.write_all(json.as_bytes()).await?;
    f.write_all(b"\n").await?;
    Ok(())
}

async fn read_new_events(
    path: &Path,
    offset: &mut u64,
    reader: &mut Option<BufReader<File>>,
) -> Result<Vec<String>> {
    let meta = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let len = meta.len();
    if len < *offset {
        // File rotated — start over with a fresh handle.
        *offset = 0;
        *reader = None;
    }
    if len == *offset { return Ok(vec![]); }

    // Open once, reuse the handle across polls; reopen if we don't have one
    // or if the previous call tore it down after an error.
    if reader.is_none() {
        let mut f = File::open(path).await?;
        f.seek(std::io::SeekFrom::Start(*offset)).await?;
        *reader = Some(BufReader::new(f));
    }
    let r = reader.as_mut().expect("reader just ensured");

    let mut out = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line).await?;
        if n == 0 { break; }
        if !line.ends_with('\n') { break; }
        *offset += n as u64;
        let end = line.trim_end().len();
        line.truncate(end);
        let start = line.bytes().take_while(|b| b.is_ascii_whitespace()).count();
        if start > 0 { line.drain(..start); }
        if !line.is_empty() { out.push(std::mem::take(&mut line)); }
    }
    Ok(out)
}
