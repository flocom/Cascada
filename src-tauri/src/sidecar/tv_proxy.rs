//! Cascada-managed TradingView sidecar.
//!
//! Spawns and supervises a `mitmdump` child running the embedded
//! `cascada_addon.py`. On first run, builds an isolated Python venv at
//! `<cascada_root>/tv-proxy/.venv/` and pip-installs `mitmproxy + requests`.
//! The addon itself is `include_str!`-bundled in the binary so users never
//! need a local clone of the repo — Cascada writes a fresh copy to disk on
//! each setup, replacing any stale copy from a previous version.
//!
//! Lifecycle:
//!   1. `setup()` — idempotent: ensure Python ≥ 3.10, build venv, install
//!      deps, extract addon. Safe to re-run; only does work that's missing.
//!   2. `start()` — spawn `mitmdump -s <addon> --listen-port <port>`,
//!      attach a supervisor task that forwards stdout/stderr lines into
//!      Cascada's log stream as `tv-proxy` entries.
//!   3. `stop()`  — best-effort kill + wait. Also called automatically when
//!      the manager is dropped (`kill_on_drop`) so a mitmdump child never
//!      outlives the Tauri process even on rough exits.
//!
//! Python interpreter strategy: each release bundles a self-contained
//! CPython distribution (python-build-standalone, ~30 MB compressed) under
//! the app's `resource_dir/python/`. `attach_app_handle()` resolves this
//! once at startup and registers it via `set_bundled_python_dir()`. The
//! detection routine prefers the bundled interpreter; if it's missing
//! (dev mode without bundled assets, arch mismatch on a universal macOS
//! build running on Intel without aarch64 emulation, etc.) it falls back
//! to scanning PATH for a `python3.10+`.

use crate::core::events::LogLevel;
use crate::core::model::ConnectorEvent;
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// Tokio Command builder with `CREATE_NO_WINDOW` set on Windows. Without
/// this flag every spawn of `python.exe` / `mitmdump.exe` opens a stray
/// console window that confuses non-technical users (a black box pops up
/// next to the app while setup runs). Cross-platform no-op elsewhere.
#[allow(unused_mut)]
fn cmd<S: AsRef<OsStr>>(program: S) -> Command {
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW = 0x08000000 — suppresses the conhost window
        // for child processes that would otherwise inherit a console.
        c.creation_flags(0x08000000);
    }
    c
}

/// Embedded addon — written to disk on every `setup()` so users always
/// run the addon shipped with the current Cascada build.
const ADDON_BODY: &str = include_str!("../../../tv-proxy/cascada_addon.py");

/// Default listen port. Matches the legacy `setup.sh` / `setup.ps1`, so
/// users with a pre-configured browser proxy continue to work.
pub const DEFAULT_PORT: u16 = 8080;

/// Synthetic source name used when forwarding sidecar logs into Cascada's
/// log stream. Shows up under "Source" in the Logs panel.
const LOG_SOURCE: &str = "tv-proxy";

/// Minimum Python (major, minor). mitmproxy ≥ 11 needs 3.10+.
const MIN_PY: (u32, u32) = (3, 10);

/// What the UI needs to know to render a "TV proxy" status panel.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TvProxyStatus {
    /// `true` once the venv exists and mitmproxy is importable inside it.
    pub installed: bool,
    /// `true` while the supervised mitmdump child is still alive.
    pub running: bool,
    pub port: u16,
    pub python_path: Option<String>,
    pub python_version: Option<String>,
    pub addon_path: Option<String>,
    pub venv_path: Option<String>,
    /// CA cert mitmproxy generates on first launch — surfaced so the UI
    /// can show the path the user needs to trust in their browser.
    pub cert_path: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct State {
    child: Option<Child>,
    python: Option<PythonInfo>,
    last_error: Option<String>,
    /// Lazily computed once Python + venv exist; cached so `status()`
    /// doesn't re-stat the venv on every poll.
    installed: bool,
}

#[derive(Clone, Debug)]
struct PythonInfo {
    path: PathBuf,
    version: String,
}

pub struct TvProxyManager {
    state: Mutex<State>,
    events: tokio::sync::mpsc::UnboundedSender<ConnectorEvent>,
    port: u16,
    /// Directory of the bundled python-build-standalone distribution
    /// (set by `attach_app_handle` at startup, resolved from the Tauri
    /// resource dir). `None` in dev builds without bundled resources or
    /// on architectures where bundling was skipped.
    bundled_python_dir: std::sync::OnceLock<PathBuf>,
}

impl TvProxyManager {
    pub fn new(events: tokio::sync::mpsc::UnboundedSender<ConnectorEvent>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            events,
            port: DEFAULT_PORT,
            bundled_python_dir: std::sync::OnceLock::new(),
        }
    }

    /// Register the bundled Python root (e.g. `<resource_dir>/python`).
    /// Called once at startup from `AppState::attach_app_handle`. The
    /// directory is expected to contain `bin/python3` (Unix) or
    /// `python.exe` (Windows). Missing or invalid → silently ignored,
    /// detection falls back to PATH.
    pub fn set_bundled_python_dir(&self, dir: PathBuf) {
        let _ = self.bundled_python_dir.set(dir);
    }

    pub async fn status(&self) -> TvProxyStatus {
        let s = self.state.lock().await;
        let venv = venv_dir();
        TvProxyStatus {
            installed: s.installed,
            running: s.child.is_some(),
            port: self.port,
            python_path: s.python.as_ref().map(|p| p.path.display().to_string()),
            python_version: s.python.as_ref().map(|p| p.version.clone()),
            addon_path: Some(addon_path().display().to_string()),
            venv_path: venv.as_ref().map(|p| p.display().to_string()),
            cert_path: cert_path().map(|p| p.display().to_string()),
            last_error: s.last_error.clone(),
        }
    }

    /// Idempotent first-time setup. Detects Python, creates the venv,
    /// installs mitmproxy + requests, writes the bundled addon to disk.
    /// Returns the post-setup status so the UI can refresh in one round-trip.
    pub async fn setup(self: &Arc<Self>) -> Result<TvProxyStatus> {
        self.log(LogLevel::Info, "tv-proxy: setup starting");

        let py = match self.resolve_python().await {
            Ok(p) => p,
            Err(e) => {
                let msg = format!("tv-proxy: Python detection failed — {e}");
                self.log(LogLevel::Error, &msg);
                self.state.lock().await.last_error = Some(format!("{e}"));
                return Err(e);
            }
        };
        self.log(LogLevel::Info, format!(
            "tv-proxy: using Python {} ({})", py.version, py.path.display()));

        let venv = venv_dir().ok_or_else(|| anyhow!("cannot resolve cascada_root"))?;
        let venv_py = venv_python(&venv);

        if !venv_py.exists() {
            self.log(LogLevel::Info, format!("tv-proxy: creating venv at {}", venv.display()));
            let out = cmd(&py.path)
                .arg("-m").arg("venv").arg(&venv)
                .output().await?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr).into_owned();
                self.state.lock().await.last_error = Some(err.clone());
                return Err(anyhow!("venv create failed: {err}"));
            }
        }

        // pip install — quiet to avoid spamming logs, but keep stderr so
        // failures (network, SSL, etc) surface via `last_error`.
        self.log(LogLevel::Info, "tv-proxy: installing mitmproxy + requests…");
        let pip = cmd(&venv_py)
            .args(["-m", "pip", "install", "--quiet", "--upgrade",
                   "pip", "mitmproxy", "requests"])
            .output().await?;
        if !pip.status.success() {
            let err = String::from_utf8_lossy(&pip.stderr).into_owned();
            self.state.lock().await.last_error = Some(err.clone());
            return Err(anyhow!("pip install failed: {err}"));
        }

        // Write the embedded addon. We always overwrite so a Cascada
        // upgrade picks up addon changes without the user clearing state.
        let addon = addon_path();
        if let Some(parent) = addon.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&addon, ADDON_BODY).await?;
        self.log(LogLevel::Info, format!("tv-proxy: addon → {}", addon.display()));

        // Bootstrap the CA cert by booting mitmdump on a throwaway port —
        // skip if the cert already exists (idempotent).
        if cert_path().map_or(true, |p| !p.exists()) {
            self.bootstrap_cert(&venv).await;
        }

        let mut s = self.state.lock().await;
        s.python = Some(py);
        s.installed = true;
        s.last_error = None;
        Ok(self_status(&s, self.port))
    }

    /// Spawn the supervised mitmdump child. Idempotent: returns the
    /// current status if the proxy is already running. Calls `setup()`
    /// first if the venv hasn't been provisioned yet.
    pub async fn start(self: &Arc<Self>) -> Result<TvProxyStatus> {
        {
            let s = self.state.lock().await;
            if s.child.is_some() { return Ok(self_status(&s, self.port)); }
        }
        let installed = self.state.lock().await.installed;
        if !installed { self.setup().await?; }

        let venv = venv_dir().ok_or_else(|| anyhow!("cannot resolve cascada_root"))?;
        let mitmdump = mitmdump_path(&venv);
        let addon = addon_path();
        if !mitmdump.exists() {
            return Err(anyhow!("mitmdump not found at {} — run setup again", mitmdump.display()));
        }
        if !addon.exists() {
            tokio::fs::write(&addon, ADDON_BODY).await?;
        }

        self.log(LogLevel::Info, format!(
            "tv-proxy: launching mitmdump on :{} ({})", self.port, mitmdump.display()));

        let mut spawn = cmd(&mitmdump);
        spawn.arg("-s").arg(&addon)
            .arg("--listen-port").arg(self.port.to_string())
            // Quiet flow logs — we don't want every TV API hit in the
            // user's log panel. The addon emits its own structured logs
            // for the events Cascada cares about.
            .arg("--set").arg("flow_detail=0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = spawn.spawn().map_err(|e| {
            anyhow!("spawn mitmdump failed: {e} — re-run setup if Python deps changed")
        })?;

        // Forward child stdio into the log stream as separate background
        // tasks so the supervisor doesn't block on either pipe.
        if let Some(out) = child.stdout.take() {
            let evt = self.events.clone();
            tokio::spawn(forward_lines(out, evt, LogLevel::Info));
        }
        if let Some(err) = child.stderr.take() {
            let evt = self.events.clone();
            tokio::spawn(forward_lines(err, evt, LogLevel::Warn));
        }

        let mut s = self.state.lock().await;
        s.child = Some(child);
        s.last_error = None;
        Ok(self_status(&s, self.port))
    }

    /// Stop the running mitmdump child. Best-effort: a child that already
    /// died (crash, manual kill) is treated as success.
    pub async fn stop(&self) -> Result<()> {
        let mut s = self.state.lock().await;
        if let Some(mut child) = s.child.take() {
            self.log(LogLevel::Info, "tv-proxy: stopping mitmdump");
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        Ok(())
    }

    /// Convenience for AppState: emit a `tv-proxy` log without holding
    /// any lock. Goes through the connector event channel so it lands in
    /// the same Logs panel as MT/cTrader entries.
    fn log(&self, level: LogLevel, msg: impl Into<String>) {
        let _ = self.events.send(ConnectorEvent::Log {
            account_id: LOG_SOURCE.to_string(),
            level,
            message: msg.into(),
        });
    }

    /// Boot mitmdump just long enough to seed `~/.mitmproxy/*` files. Same
    /// trick as setup.sh — without it, the CA cert only appears after the
    /// first real launch, which is too late for first-run UX.
    async fn bootstrap_cert(self: &Arc<Self>, venv: &Path) {
        let mitm = mitmdump_path(venv);
        if !mitm.exists() { return; }
        self.log(LogLevel::Info, "tv-proxy: bootstrapping CA cert…");
        let mut child = match cmd(&mitm)
            .args(["--listen-port", "18080", "--quiet"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn() {
                Ok(c) => c,
                Err(e) => {
                    self.log(LogLevel::Warn, format!("tv-proxy: cert bootstrap spawn failed — {e}"));
                    return;
                }
            };
        // Poll for ~3 s; mitmproxy writes the cert within the first second.
        let cert = cert_path();
        for _ in 0..6 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if cert.as_ref().is_some_and(|p| p.exists()) { break; }
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

/// Snapshot helper that doesn't borrow `&self` (avoids re-acquiring the
/// state mutex when we already hold it).
fn self_status(s: &State, port: u16) -> TvProxyStatus {
    let venv = venv_dir();
    TvProxyStatus {
        installed: s.installed,
        running: s.child.is_some(),
        port,
        python_path: s.python.as_ref().map(|p| p.path.display().to_string()),
        python_version: s.python.as_ref().map(|p| p.version.clone()),
        addon_path: Some(addon_path().display().to_string()),
        venv_path: venv.as_ref().map(|p| p.display().to_string()),
        cert_path: cert_path().map(|p| p.display().to_string()),
        last_error: s.last_error.clone(),
    }
}

impl TvProxyManager {
    /// Pick the Python interpreter for this run. Order of preference:
    /// 1. Bundled distribution under `<resource_dir>/python/` (set via
    ///    `set_bundled_python_dir`). Production builds always have this.
    /// 2. PATH detection — the legacy fallback for dev builds and edge
    ///    cases where the bundled binary can't execute (arch mismatch).
    async fn resolve_python(&self) -> Result<PythonInfo> {
        if let Some(dir) = self.bundled_python_dir.get() {
            let exe = bundled_python_exe(dir);
            match probe_python(&exe).await {
                Ok(info) => return Ok(info),
                Err(e) => self.log(LogLevel::Warn, format!(
                    "tv-proxy: bundled python at {} unusable ({e}), \
                     falling back to PATH", exe.display())),
            }
        }
        detect_python_on_path().await
    }
}

/// Path to the python executable inside a python-build-standalone
/// distribution root. Cross-platform — the layout differs between Unix
/// (`<dir>/bin/python3`) and Windows (`<dir>/python.exe`).
fn bundled_python_exe(dir: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        dir.join("python.exe")
    } else {
        dir.join("bin").join("python3")
    }
}

/// Probe a specific python binary, returning its (major.minor, executable
/// path) if it reports >= MIN_PY. Used for both the bundled interpreter
/// and PATH candidates.
async fn probe_python(exe: &Path) -> Result<PythonInfo> {
    let out = cmd(exe)
        .arg("-c")
        .arg("import sys; print('%d.%d %s' % (sys.version_info.major, sys.version_info.minor, sys.executable))")
        .output().await
        .map_err(|e| anyhow!("{}: {e}", exe.display()))?;
    if !out.status.success() {
        return Err(anyhow!("{}: exited {}", exe.display(), out.status));
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (ver_str, real_exe) = stdout.split_once(' ')
        .ok_or_else(|| anyhow!("{}: unexpected output '{stdout}'", exe.display()))?;
    let mut parts = ver_str.split('.');
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    if (major, minor) < MIN_PY {
        return Err(anyhow!(
            "{}: reports {ver_str}, need {}.{}+",
            exe.display(), MIN_PY.0, MIN_PY.1));
    }
    Ok(PythonInfo {
        path: PathBuf::from(real_exe),
        version: ver_str.to_string(),
    })
}

/// Walk a list of candidate executable names on PATH, return the first
/// that reports `Python ≥ MIN_PY`. Fallback path when no bundled
/// interpreter is available.
async fn detect_python_on_path() -> Result<PythonInfo> {
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &["py", "python3", "python", "python3.13", "python3.12", "python3.11", "python3.10"]
    } else {
        &["python3.13", "python3.12", "python3.11", "python3.10", "python3", "python"]
    };
    let mut last_err: Option<String> = None;
    for cand in candidates {
        // `py -3` is the launcher form on Windows — pass `-3` so we don't
        // accidentally probe a Python 2 installation that's still on PATH.
        let mut probe = cmd(cand);
        if cfg!(target_os = "windows") && *cand == "py" { probe.arg("-3"); }
        probe.arg("-c")
           .arg("import sys; print('%d.%d %s' % (sys.version_info.major, sys.version_info.minor, sys.executable))");
        let out = match probe.output().await {
            Ok(o) => o,
            Err(e) => { last_err = Some(format!("{cand}: {e}")); continue; }
        };
        if !out.status.success() { continue; }
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // "3.12 /usr/bin/python3.12"
        let (ver_str, exe) = match stdout.split_once(' ') {
            Some(p) => p,
            None => continue,
        };
        let mut parts = ver_str.split('.');
        let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        if (major, minor) >= MIN_PY {
            return Ok(PythonInfo {
                path: PathBuf::from(exe),
                version: ver_str.to_string(),
            });
        }
    }
    Err(anyhow!(
        "Python {}.{}+ not found on PATH. Install from python.org (Windows: tick \
         \"Add Python to PATH\") and click Setup again.{}",
        MIN_PY.0, MIN_PY.1,
        last_err.map(|e| format!(" Last probe: {e}")).unwrap_or_default()))
}

/// `<cascada_root>/tv-proxy/.venv/`. Returns None if the cascada root
/// can't be resolved (no home directory, sandbox).
fn venv_dir() -> Option<PathBuf> {
    crate::connectors::file_bridge::cascada_root()
        .map(|r| r.join("tv-proxy").join(".venv"))
}

fn venv_python(venv: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

fn mitmdump_path(venv: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        venv.join("Scripts").join("mitmdump.exe")
    } else {
        venv.join("bin").join("mitmdump")
    }
}

fn addon_path() -> PathBuf {
    crate::connectors::file_bridge::cascada_root()
        .map(|r| r.join("tv-proxy").join("cascada_addon.py"))
        .unwrap_or_else(|| PathBuf::from("cascada_addon.py"))
}

/// `~/.mitmproxy/mitmproxy-ca-cert.pem` — mitmproxy hardcodes this path
/// regardless of where the venv lives, so we surface it for the trust
/// step the user still has to do manually.
fn cert_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().join(".mitmproxy").join("mitmproxy-ca-cert.pem"))
}

/// Read every line from `pipe` and forward as a Cascada log entry under
/// the synthetic `tv-proxy` source. Stops when the pipe closes (child
/// exited or stdio was redirected).
async fn forward_lines<R>(
    pipe: R,
    events: tokio::sync::mpsc::UnboundedSender<ConnectorEvent>,
    level: LogLevel,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(pipe).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        let _ = events.send(ConnectorEvent::Log {
            account_id: LOG_SOURCE.to_string(),
            level,
            message: trimmed.to_string(),
        });
    }
}
