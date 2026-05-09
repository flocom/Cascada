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
use tokio::io::BufReader;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// Bind a freshly-spawned child to a Windows Job Object configured
/// with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so the OS reaps the
/// child when Cascada's process handle goes away — even on a brutal
/// `TerminateProcess` (auto-updater, Task Manager, crash). Without
/// this, mitmdump.exe survives Cascada's exit and locks
/// `python\DLLs\_asyncio.pyd`, which the next installer can't
/// overwrite ("Error opening file for writing").
///
/// The job handle is intentionally leaked — its lifetime IS the app's
/// lifetime. When the OS tears down Cascada's process, the handle's
/// kernel ref count drops, the job closes, member processes die.
/// `kill_on_drop(true)` only fires for clean shutdowns, this covers
/// the rough ones.
#[cfg(windows)]
fn bind_to_kill_on_close_job(child: &Child) -> std::io::Result<()> {
    use std::sync::OnceLock;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    // Cache the job handle as a raw `isize` (HANDLE's underlying repr
    // in the windows crate). HANDLE itself is `!Sync` so it can't go
    // straight into a `OnceLock`, but the kernel handle is just an
    // integer behind the wrapper.
    static JOB: OnceLock<isize> = OnceLock::new();

    let job_raw: isize = *JOB.get_or_init(|| {
        // Anonymous job (no name), default security descriptor.
        let h = unsafe { CreateJobObjectW(None, windows::core::PCWSTR::null()) };
        let h = match h {
            Ok(h) if !h.is_invalid() => h,
            _ => return 0,
        };
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok.is_err() { return 0; }
        h.0 as isize
    });

    if job_raw == 0 {
        return Err(std::io::Error::other("job object init failed"));
    }
    let job_handle = HANDLE(job_raw as *mut _);

    let pid = child.id().ok_or_else(|| std::io::Error::other("child has no PID"))?;
    let proc_handle = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SET_QUOTA, false.into(), pid) }
        .map_err(|e| std::io::Error::other(format!("OpenProcess: {e}")))?;
    let assign_result = unsafe { AssignProcessToJobObject(job_handle, proc_handle) };
    // Close the per-process handle regardless of assignment outcome —
    // the job keeps its own reference to the process internally.
    let _ = unsafe { CloseHandle(proc_handle) };
    assign_result.map_err(|e| std::io::Error::other(format!("AssignProcessToJobObject: {e}")))?;
    Ok(())
}

#[cfg(not(windows))]
#[inline]
fn bind_to_kill_on_close_job(_child: &Child) -> std::io::Result<()> { Ok(()) }

/// Best-effort kill of any leftover `mitmdump.exe` (Windows) or
/// `mitmdump` (Unix) process. Used as a recovery step when the proxy
/// port is already bound at startup — usually a zombie from a prior
/// Cascada session that survived a brutal app exit (pre-0.5.8, before
/// the Job Object lifecycle binding). Errors are intentionally
/// swallowed: this is a fallback, not a guarantee.
async fn kill_leftover_mitmdump() {
    #[cfg(windows)]
    let mut killer = {
        let mut c = cmd("taskkill");
        c.args(["/F", "/IM", "mitmdump.exe"]);
        c
    };
    #[cfg(unix)]
    let mut killer = {
        let mut c = cmd("pkill");
        c.args(["-f", "mitmdump"]);
        c
    };
    killer.stdout(Stdio::null()).stderr(Stdio::null());
    let _ = killer.status().await;
}

/// Tokio Command builder with `CREATE_NO_WINDOW` set on Windows. Without
/// this flag every spawn of `python.exe` / `mitmdump.exe` opens a stray
/// console window that confuses non-technical users (a black box pops up
/// next to the app while setup runs). On all platforms we also force
/// UTF-8 stdio for any Python child — without a real terminal,
/// mitmdump on Windows otherwise picks up the system locale (cp1252
/// in France) and dies with `OSError [Errno 22] Invalid argument` the
/// first time it prints a non-Latin-1 byte (Google OAuth URLs do this
/// reliably). The env vars are no-ops for non-Python binaries.
#[allow(unused_mut)]
fn cmd<S: AsRef<OsStr>>(program: S) -> Command {
    let mut c = Command::new(program);
    c.env("PYTHONIOENCODING", "utf-8")
     .env("PYTHONUTF8", "1")
     // Force unbuffered stdout/stderr. Without this, Python defaults to
     // block-buffered (8 KB) when stdio is a pipe (not a TTY) — so a
     // freshly-spawned mitmdump can run for hours without flushing a
     // single log line to Cascada's `forward_lines` task. The buffer
     // only drains when mitmdump exits, which is exactly when the user
     // can no longer act on the data. `-u` / `PYTHONUNBUFFERED=1`
     // switches stdout/stderr to line-buffered mode and was the root
     // cause of "addon logs never appear in real time" reports.
     .env("PYTHONUNBUFFERED", "1");
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW = 0x08000000 — suppresses the conhost window
        // for child processes that would otherwise inherit a console.
        // tokio::process::Command exposes `creation_flags` directly on
        // Windows, no trait import needed.
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
    /// CA cert mitmproxy generates on first launch — kept around for
    /// the rare advanced user who wants to import it manually into a
    /// non-Cascada-managed browser.
    pub cert_path: Option<String>,
    /// `true` once the bundled-browser flow is ready to launch (proxy
    /// installed, cert generated, SPKI hash computed). Drives the
    /// "Open TradingView" button in the UI.
    pub browser_ready: bool,
    /// Path to the Chromium-family browser Cascada will spawn for
    /// TradingView (Chrome, Edge, Brave, Chromium). `None` if no
    /// supported browser was found on the machine.
    pub browser_path: Option<String>,
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
    /// SHA-256 of the mitmproxy CA cert's SubjectPublicKeyInfo, base64.
    /// Computed once after `bootstrap_cert()` from the venv's `cryptography`
    /// (mitmproxy dep). Passed to Chrome's
    /// `--ignore-certificate-errors-spki-list` so the bundled browser
    /// trusts only the proxy cert and nothing else — narrower than the
    /// blanket `--ignore-certificate-errors` flag, which has been
    /// restricted to debug builds in modern Chromium.
    cert_spki: Option<String>,
    /// Cascada-launched Chromium process. Stored so we can kill it on
    /// Cascada quit / Stop proxy — without this the browser lingers
    /// after the proxy goes down, leaving the user with a TradingView
    /// window pointed at a dead 127.0.0.1:8080.
    browser_child: Option<Child>,
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
    /// Cached resolved Chromium-family browser. Populated lazily on
    /// first `status()` poll and invalidated only if the binary
    /// disappears between calls — avoids re-statting the per-platform
    /// candidate list every 2 s while the TradingView pane is open.
    /// Sync `parking_lot::Mutex` (already a transitive dep) is enough;
    /// the lookup is so cheap it doesn't warrant the await of
    /// `tokio::sync::Mutex`.
    browser_cache: parking_lot::Mutex<Option<PathBuf>>,
}

impl TvProxyManager {
    pub fn new(events: tokio::sync::mpsc::UnboundedSender<ConnectorEvent>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            events,
            port: DEFAULT_PORT,
            bundled_python_dir: std::sync::OnceLock::new(),
            browser_cache: parking_lot::Mutex::new(None),
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
        self.snapshot(&s)
    }

    /// Build a `TvProxyStatus` from the locked state. Used by `status()`
    /// for external polls and inline by `setup()` / `start()` so they
    /// can return fresh status without dropping the lock and re-acquiring.
    fn snapshot(&self, s: &State) -> TvProxyStatus {
        let venv = venv_dir();
        let browser = self.cached_browser();
        TvProxyStatus {
            installed: s.installed,
            running: s.child.is_some(),
            port: self.port,
            python_path: s.python.as_ref().map(|p| p.path.display().to_string()),
            python_version: s.python.as_ref().map(|p| p.version.clone()),
            addon_path: Some(addon_path().display().to_string()),
            venv_path: venv.as_ref().map(|p| p.display().to_string()),
            cert_path: cert_path().map(|p| p.display().to_string()),
            browser_ready: s.installed && s.cert_spki.is_some() && browser.is_some(),
            browser_path: browser.map(|p| p.display().to_string()),
            last_error: s.last_error.clone(),
        }
    }

    /// Look up the browser path, populating `browser_cache` on first
    /// hit. The Accounts UI polls `status()` every 2 s while the
    /// TradingView pane is open, so without caching we'd re-stat every
    /// candidate path 30 times a minute. Cache invalidates if the
    /// previously-found binary disappears (uninstalled mid-session).
    fn cached_browser(&self) -> Option<PathBuf> {
        if let Some(p) = self.browser_cache.lock().clone() {
            if p.exists() { return Some(p); }
        }
        let found = find_chromium_browser();
        *self.browser_cache.lock() = found.clone();
        found
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

        // pip install — skipped when mitmdump is already in the venv, so
        // re-running setup is a no-op for users with a working install
        // (avoids a PyPI round-trip that can hang on slow networks). To
        // force a fresh install, the user deletes the venv directory.
        let mitmdump = mitmdump_path(&venv);
        if !mitmdump.exists() {
            self.log(LogLevel::Info, "tv-proxy: installing mitmproxy + requests…");
            let pip = cmd(&venv_py)
                .args(["-m", "pip", "install", "--quiet",
                       "mitmproxy", "requests"])
                .output().await?;
            if !pip.status.success() {
                let err = String::from_utf8_lossy(&pip.stderr).into_owned();
                self.state.lock().await.last_error = Some(err.clone());
                return Err(anyhow!("pip install failed: {err}"));
            }
        } else {
            self.log(LogLevel::Info, "tv-proxy: venv already provisioned, skipping pip");
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

        // Pre-compute the cert's SPKI hash so `open_browser()` can pass it
        // to Chrome via `--ignore-certificate-errors-spki-list` without
        // hitting Python again on every launch. Failures here are
        // non-fatal — the open-browser flow surfaces a clearer error if
        // the hash is missing.
        let spki = if let Some(cp) = cert_path().filter(|p| p.exists()) {
            match compute_cert_spki(&venv_py, &cp).await {
                Ok(h) => Some(h),
                Err(e) => {
                    self.log(LogLevel::Warn, format!(
                        "tv-proxy: SPKI hash compute failed — {e}. \
                         Open TradingView will fail until setup is re-run."));
                    None
                }
            }
        } else { None };

        let mut s = self.state.lock().await;
        s.python = Some(py);
        s.installed = true;
        s.last_error = None;
        s.cert_spki = spki;
        Ok(self.snapshot(&s))
    }

    /// Spawn the supervised mitmdump child. Idempotent: returns the
    /// current status if the proxy is already running. Calls `setup()`
    /// first if the venv hasn't been provisioned yet.
    pub async fn start(self: &Arc<Self>) -> Result<TvProxyStatus> {
        {
            let s = self.state.lock().await;
            if s.child.is_some() { return Ok(self.snapshot(&s)); }
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

        // Refuse to spawn if the port is already taken — likely a stale
        // mitmdump from a previous Cascada session that survived `kill_on_drop`
        // (Windows in particular doesn't always honor it on rough exits).
        // Surfacing a clear message here is far better than letting the
        // browser hit ERR_PROXY_CONNECTION_FAILED with no explanation.
        // First we try to auto-recover: a leftover mitmdump.exe from
        // a prior Cascada session is the overwhelmingly common cause,
        // and `taskkill` on the exact venv binary is safe to attempt.
        if tokio::net::TcpStream::connect(("127.0.0.1", self.port)).await.is_ok() {
            self.log(LogLevel::Warn, format!(
                "tv-proxy: port {} already in use — attempting to kill leftover mitmdump.exe",
                self.port));
            kill_leftover_mitmdump().await;
            // Give the OS a beat to release the socket — without this
            // the next bind() races and re-fires the in-use check.
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            if tokio::net::TcpStream::connect(("127.0.0.1", self.port)).await.is_ok() {
                return Err(anyhow!(
                    "Port {} is still in use after attempting cleanup. \
                     Restart your computer or check Task Manager for a process \
                     bound to this port.", self.port));
            }
            self.log(LogLevel::Info, "tv-proxy: leftover process cleared, continuing");
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
            // Silence mitmproxy core's per-flow chatter (`client connect`,
            // `server connect`, WebSocket ping/pong) — it floods the
            // user's Logs panel with hundreds of events per minute and
            // drowns the actual signal. Our addon writes its own
            // structured one-liners directly to stderr (see `_log` in
            // cascada_addon.py), so they bypass this filter and remain
            // visible regardless of what mitmproxy decides to print.
            .arg("--set").arg("termlog_verbosity=warn")
            // Pass-through (don't MITM) hosts that ship hard-coded cert
            // pins beyond what `--ignore-certificate-errors-spki-list`
            // can satisfy. Without this, Google / Apple / Microsoft
            // login flows fail at the TLS handshake step:
            //   "Client TLS handshake failed. … the client does not
            //    trust the proxy's certificate."
            // mitmproxy still receives the CONNECT and forwards bytes
            // verbatim, so the user can sign in with Google or Apple
            // and then land back on TradingView (which IS intercepted
            // because tradingview.com isn't in the ignore list). Patterns
            // are anchored on host endings so we don't accidentally
            // tunnel `<broker>.googleapis-mock.tradingview.com` or
            // similar bridge hosts.
            .arg("--ignore-hosts")
            .arg(r"(google\.com|googleapis\.com|gstatic\.com|googletagmanager\.com|googlesyndication\.com|youtube\.com|ytimg\.com|apple\.com|icloud\.com|microsoft\.com|microsoftonline\.com|windows\.com|live\.com)(:\d+)?$")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = spawn.spawn().map_err(|e| {
            anyhow!("spawn mitmdump failed: {e} — re-run setup if Python deps changed")
        })?;

        // Bind mitmdump to Cascada's kill-on-close job (Windows only).
        // Best-effort: if the assignment fails we still proceed —
        // `kill_on_drop(true)` covers clean shutdowns; only abrupt
        // terminations leak the child without the job binding.
        if let Err(e) = bind_to_kill_on_close_job(&child) {
            self.log(LogLevel::Warn, format!(
                "tv-proxy: could not bind mitmdump to job object ({e}) — \
                 leftover process possible if Cascada is force-killed"));
        }

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

        // mitmdump takes 1-3 s to import the addon and bind the listener.
        // Without an explicit readiness probe, `open_browser()` would race
        // ahead and Chrome would surface ERR_PROXY_CONNECTION_FAILED before
        // the proxy is up. Worse: a silent crash (broken venv, addon import
        // error, antivirus quarantine) would never reach the user — the
        // log forward picks up stderr but only AFTER our caller already
        // reported success. Poll the port for up to 8 s; if the child
        // exited or we timed out, kill it and report a clear error.
        let port = self.port;
        for _ in 0..32 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if let Ok(Some(status)) = child.try_wait() {
                let _ = child.wait().await;
                return Err(anyhow!(
                    "mitmdump exited immediately ({status}) — check the Logs panel \
                     for crash output. If the venv looks broken, click Reinstall."));
            }
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                self.log(LogLevel::Info, format!("tv-proxy: ready on :{port}"));
                let mut s = self.state.lock().await;
                s.child = Some(child);
                s.last_error = None;
                return Ok(self.snapshot(&s));
            }
        }

        // Timed out — child is still alive but never bound. Kill it so we
        // don't leak a zombie that'll trip the port-in-use check next run.
        let _ = child.kill().await;
        let _ = child.wait().await;
        Err(anyhow!(
            "mitmdump didn't accept connections on :{port} within 8 s — see Logs \
             panel for output. Try Reinstall if the issue persists."))
    }

    /// Spawn a Cascada-managed Chrome/Edge window pre-configured to route
    /// through the proxy and trust mitmproxy's CA — the user's main
    /// browser is never touched. The browser uses an isolated profile
    /// under `<cascada_root>/tv-browser/profile/` so TradingView logins
    /// persist across Cascada restarts without polluting (or being
    /// polluted by) the user's regular browsing.
    ///
    /// Calls `start()` first if the proxy isn't running yet — there's no
    /// point opening the browser if mitmdump can't intercept.
    pub async fn open_browser(self: &Arc<Self>) -> Result<()> {
        // Ensure the proxy is up. start() is idempotent (no-op when
        // already running) and triggers setup() on first launch.
        let running = self.state.lock().await.child.is_some();
        if !running { self.start().await?; }

        let browser = find_chromium_browser().ok_or_else(|| anyhow!(
            "Could not find Chrome, Edge, Brave, or Chromium on this machine. \
             Install one of them, then click Open TradingView again."))?;

        let spki = self.state.lock().await.cert_spki.clone().ok_or_else(|| anyhow!(
            "Proxy CA cert hash not computed yet — re-run Install proxy."))?;

        let profile_dir = browser_profile_dir().ok_or_else(|| anyhow!(
            "cannot resolve cascada_root for browser profile"))?;
        tokio::fs::create_dir_all(&profile_dir).await?;

        self.log(LogLevel::Info, format!(
            "tv-proxy: opening TradingView in isolated browser ({})",
            browser.display()));

        // Static portion of the Chrome command line. The dynamic args
        // (proxy-server, user-data-dir, spki list, target URL) follow
        // below. `--app=` would hide browser chrome but TradingView's
        // chart + screener + watchlist tabs all need full Chrome UI,
        // so we run a normal window with a sandboxed user_data_dir.
        //
        // `--disable-quic` is the load-bearing flag here: without it
        // Chrome upgrades repeat-visit hosts to HTTP/3 over UDP and
        // the request never enters mitmdump's TCP CONNECT tunnel —
        // every /trading/place call would silently bypass the proxy.
        // `UseDnsHttpsSvcb` is the modern DNS-side equivalent (HTTPS
        // resource records), disabling it forces classic A/AAAA
        // lookups so all candidates resolve to TCP-only endpoints.
        const STATIC_ARGS: &[&str] = &[
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-quic",
            "--disable-features=UseDnsHttpsSvcb",
        ];

        // `kill_on_drop(true)` couples the browser's lifetime to
        // TvProxyManager — when the user quits Cascada (which the
        // main.rs CloseRequested handler turns into a `stop()` call),
        // the proxy goes down AND the browser closes, instead of
        // leaving a TradingView window pointed at a dead 127.0.0.1:8080.
        // We also explicitly kill any previous browser child below so
        // re-clicking "Open TradingView" doesn't accumulate windows.
        let mut spawn = cmd(&browser);
        spawn
            .arg(format!("--proxy-server=127.0.0.1:{}", self.port))
            .arg(format!("--user-data-dir={}", profile_dir.display()))
            .arg(format!("--ignore-certificate-errors-spki-list={spki}"))
            .args(STATIC_ARGS)
            .arg("https://www.tradingview.com/chart/")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let child = spawn.spawn().map_err(|e| anyhow!("browser spawn failed: {e}"))?;

        // Pin the browser to Cascada's kill-on-close job so a brutal
        // app exit doesn't leave a Chrome window hanging around the
        // Windows file system locks.
        let _ = bind_to_kill_on_close_job(&child);

        // Replace any prior browser child. The previous one — if it's
        // still alive — gets dropped and `kill_on_drop` reaps it.
        let mut s = self.state.lock().await;
        if let Some(mut prev) = s.browser_child.take() {
            let _ = prev.kill().await;
            let _ = prev.wait().await;
        }
        s.browser_child = Some(child);
        Ok(())
    }

    /// Stop the running mitmdump child AND any Cascada-managed browser
    /// window. Best-effort: a child that already died (crash, user
    /// manually closed the browser tab) is treated as success.
    pub async fn stop(&self) -> Result<()> {
        let mut s = self.state.lock().await;
        if let Some(mut child) = s.browser_child.take() {
            self.log(LogLevel::Info, "tv-proxy: closing TradingView browser");
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
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
    // Bail after 5 s. The bundled python-build-standalone binary on
    // macOS arm64 occasionally gets stuck in dyld validation on first
    // launch — without a timeout the whole setup() blocks indefinitely
    // and the UI's "Installing…" spinner never resolves. On timeout we
    // surface an error so resolve_python falls through to PATH detection.
    let fut = cmd(exe)
        .arg("-c")
        .arg("import sys; print('%d.%d %s' % (sys.version_info.major, sys.version_info.minor, sys.executable))")
        .output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(5), fut).await
        .map_err(|_| anyhow!("{}: probe timed out (>5s)", exe.display()))?
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
/// regardless of where the venv lives. Cascada used to surface it for a
/// manual import step; the bundled-browser flow now handles trust via
/// `--ignore-certificate-errors-spki-list` so most users never see it.
fn cert_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().join(".mitmproxy").join("mitmproxy-ca-cert.pem"))
}

/// Persistent profile directory for the Cascada-managed Chromium window.
/// Lives under cascada_root so logins survive a restart but stay
/// isolated from the user's regular browser profiles.
fn browser_profile_dir() -> Option<PathBuf> {
    crate::connectors::file_bridge::cascada_root()
        .map(|r| r.join("tv-browser").join("profile"))
}

/// Locate a Chromium-based browser on the user's machine. Cascada uses
/// the first one found because all of them honor the same
/// `--proxy-server` / `--user-data-dir` /
/// `--ignore-certificate-errors-spki-list` flags. Order = popularity:
/// Chrome > Edge > Brave > generic chromium.
fn find_chromium_browser() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let macos_candidates: &[&str] = &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Arc.app/Contents/MacOS/Arc",
        ];
        for c in macos_candidates {
            let p = PathBuf::from(c);
            if p.exists() { return Some(p); }
        }
    }
    #[cfg(target_os = "windows")]
    {
        // ProgramFiles(x86) for 32-bit Chrome on 64-bit Windows; both
        // exist in the wild because Chrome's installer bitness has
        // flip-flopped over the years.
        let local_app_data = std::env::var("LOCALAPPDATA").ok();
        let candidates: Vec<PathBuf> = [
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe",
        ].iter().map(PathBuf::from)
         .chain(local_app_data.iter().flat_map(|lad| [
            format!(r"{lad}\Google\Chrome\Application\chrome.exe").into(),
            format!(r"{lad}\Microsoft\Edge\Application\msedge.exe").into(),
         ]))
         .collect();
        for p in &candidates {
            if p.exists() { return Some(p.clone()); }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let linux_candidates: &[&str] = &[
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/microsoft-edge",
            "/usr/bin/microsoft-edge-stable",
            "/usr/bin/brave-browser",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
        ];
        for c in linux_candidates {
            let p = PathBuf::from(c);
            if p.exists() { return Some(p); }
        }
    }
    None
}

/// Run the bundled `cryptography` (mitmproxy dep) inside the venv to
/// hash the CA cert's SPKI. Output is `base64(sha256(SPKI_DER))`, the
/// exact format Chrome wants in `--ignore-certificate-errors-spki-list`.
async fn compute_cert_spki(venv_python: &Path, cert: &Path) -> Result<String> {
    // Raw string preserves the 4-space indentation inside `with`; the
    // previous `"\<newline>"` form silently ate leading whitespace
    // (Rust's line-continuation rule), producing an unindented Python
    // script that crashed with `IndentationError`.
    const SCRIPT: &str = r#"import hashlib, base64, sys
from cryptography import x509
from cryptography.hazmat.primitives import serialization
with open(sys.argv[1], 'rb') as f:
    c = x509.load_pem_x509_certificate(f.read())
spki = c.public_key().public_bytes(
    encoding=serialization.Encoding.DER,
    format=serialization.PublicFormat.SubjectPublicKeyInfo)
print(base64.b64encode(hashlib.sha256(spki).digest()).decode())
"#;
    let out = cmd(venv_python)
        .arg("-c").arg(SCRIPT).arg(cert)
        .output().await?;
    if !out.status.success() {
        return Err(anyhow!("python spki probe failed: {}",
            String::from_utf8_lossy(&out.stderr)));
    }
    let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if hash.is_empty() {
        return Err(anyhow!("python spki probe returned empty"));
    }
    Ok(hash)
}

/// Forward every line from `pipe` as a Cascada log entry under the
/// synthetic `tv-proxy` source.
///
/// Reads raw bytes and decodes lossily — `BufReader::lines()` would
/// drop the whole pipe on the first non-UTF-8 byte (mitmdump on
/// Windows occasionally writes cp1252-encoded URLs / headers). When
/// the reader exits, mitmdump's stdout pipe closes, the next print
/// hits broken-pipe / `OSError [Errno 22]`, and the proxy starts
/// dropping flows mid-handshake — exactly the failure mode behind
/// the Google OAuth login breakage in the wild.
async fn forward_lines<R>(
    pipe: R,
    events: tokio::sync::mpsc::UnboundedSender<ConnectorEvent>,
    level: LogLevel,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncBufReadExt;
    let mut reader = BufReader::new(pipe);
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => break, // EOF — child exited cleanly
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let trimmed = line.trim();
                if trimmed.is_empty() { continue; }
                let _ = events.send(ConnectorEvent::Log {
                    account_id: LOG_SOURCE.to_string(),
                    level,
                    message: trimmed.to_string(),
                });
            }
            // Read errors are unrecoverable on a child stdio pipe; bail
            // so the spawned task can shut down rather than spinning.
            Err(_) => break,
        }
    }
}
