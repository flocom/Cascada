<script lang="ts">
  import { onDestroy } from "svelte";
  import { api, type TvProxyStatus } from "../lib/api";

  // Bound from the parent so TV's status flows into the same banner
  // shared by the cTrader / MT installers (no duplicate UI surface).
  export let installStatus: { kind: "info" | "ok" | "err"; text: string } | null = null;
  // True only while this panel is mounted (i.e. TradingView tab open).
  // Drives the 2 s status poll — paused when inactive to save cycles.
  export let active: boolean = false;

  let tvStatus: TvProxyStatus | null = null;
  let tvBusy: "idle" | "setup" | "start" | "stop" | "open" = "idle";
  let tvPollTimer: ReturnType<typeof setInterval> | null = null;

  // Surface the "Python 3.10+ not found on PATH" failure as its own
  // panel (with a python.org link). String match is brittle on purpose
  // — the only place we generate it is detect_python().
  $: pythonMissing = installStatus?.kind === "err"
    && /python.*not found on path/i.test(installStatus.text);

  async function refreshTvStatus() {
    try { tvStatus = await api.tvProxyStatus(); }
    catch (e) { console.warn("[tv-proxy] status failed:", e); }
  }
  async function setupTvProxy() {
    if (tvBusy !== "idle") return;
    tvBusy = "setup";
    installStatus = { kind: "info", text: "Install in progress" };
    try {
      tvStatus = await api.tvProxySetup();
      installStatus = { kind: "ok", text: "Proxy installed. Click Open TradingView to start trading." };
    } catch (e) {
      installStatus = { kind: "err", text: `${e}` };
    } finally { tvBusy = "idle"; }
  }
  async function openTvBrowser() {
    if (tvBusy !== "idle") return;
    tvBusy = "open";
    installStatus = { kind: "info", text: "Launching TradingView in an isolated browser window…" };
    try {
      tvStatus = await api.tvProxyOpenBrowser();
      installStatus = { kind: "ok", text: "TradingView is open. Place any tiny trade — your master account appears here automatically." };
    } catch (e) {
      installStatus = { kind: "err", text: `${e}` };
    } finally { tvBusy = "idle"; }
  }
  async function stopTvProxy() {
    if (tvBusy !== "idle") return;
    tvBusy = "stop";
    try {
      tvStatus = await api.tvProxyStop();
      installStatus = { kind: "info", text: "Proxy stopped." };
    } catch (e) {
      installStatus = { kind: "err", text: `${e}` };
    } finally { tvBusy = "idle"; }
  }

  // Re-poll while the TradingView pane is active so the UI reflects
  // async setup/start completion (the supervisor task exits the child
  // without calling back into JS). 2 s is fast enough that the running
  // pill flips visibly within one frame of mitmdump exiting.
  $: if (active) {
    if (!tvPollTimer) {
      refreshTvStatus();
      tvPollTimer = setInterval(refreshTvStatus, 2000);
    }
  } else if (tvPollTimer) {
    clearInterval(tvPollTimer);
    tvPollTimer = null;
  }
  onDestroy(() => { if (tvPollTimer) clearInterval(tvPollTimer); });
</script>

<div class="tv-warning">
  <strong>Under active development</strong> — use with caution.
</div>
<div class="tv-status">
  <span class="tv-pill {tvStatus?.running ? 'on' : tvStatus?.installed ? 'idle' : 'off'}">
    {tvStatus?.running ? `Running on :${tvStatus.port}` : tvStatus?.installed ? "Installed · stopped" : "Not installed"}
  </span>
  {#if tvStatus?.pythonVersion}
    <span class="muted small">Python {tvStatus.pythonVersion}</span>
  {/if}
  {#if tvStatus?.lastError}
    <span class="tv-err small" title={tvStatus.lastError}>last error: {tvStatus.lastError.length > 60 ? tvStatus.lastError.slice(0, 60) + "…" : tvStatus.lastError}</span>
  {/if}
</div>
{#if pythonMissing}
  <div class="py-required">
    <div class="py-required-head">
      <strong>Python 3.10+ is required</strong>
      <span class="muted small">Cascada uses Python to run the mitmproxy sidecar that talks to TradingView.</span>
    </div>
    <ol class="py-steps">
      <li>Download &amp; install Python 3.10 or newer.</li>
      <li><strong>Windows users:</strong> in the installer, tick <em>“Add Python to PATH”</em> on the first screen — that's the most common reason setup fails.</li>
      <li>Come back here and click <em>Retry install</em>.</li>
    </ol>
    <div class="install-row">
      <a class="btn-link primary" href="https://www.python.org/downloads/" target="_blank" rel="noreferrer">Download Python →</a>
      <button on:click={setupTvProxy} disabled={tvBusy !== "idle"}>
        {tvBusy === "setup" ? "Retrying…" : "Retry install"}
      </button>
    </div>
  </div>
{:else if tvStatus && !tvStatus.installed}
  <div class="install-row">
    <button class="primary" on:click={setupTvProxy} disabled={tvBusy !== "idle"}>
      {#if tvBusy === "setup"}<span class="spinner" aria-hidden="true"></span>Installing…{:else}Install proxy{/if}
    </button>
  </div>
  {#if tvBusy === "setup"}
    <div class="setup-progress">
      <div class="setup-progress-bar"><div class="setup-progress-fill"></div></div>
    </div>
  {/if}
{:else if tvStatus && !tvStatus.browserPath}
  <div class="browser-required">
    <div class="py-required-head">
      <strong>Chrome, Edge, or Brave required</strong>
      <span class="muted small">Cascada launches TradingView in an isolated window — pick any Chromium-based browser.</span>
    </div>
    <div class="install-row">
      <a class="btn-link primary" href="https://www.google.com/chrome/" target="_blank" rel="noreferrer">Download Chrome →</a>
      <a class="btn-link" href="https://www.microsoft.com/edge" target="_blank" rel="noreferrer">Download Edge</a>
      <button on:click={refreshTvStatus} disabled={tvBusy !== "idle"}>I've installed it</button>
    </div>
  </div>
{:else}
  <div class="install-row">
    <button class="primary" on:click={openTvBrowser} disabled={tvBusy !== "idle" || !tvStatus?.browserReady}>
      {#if tvBusy === "open"}<span class="spinner" aria-hidden="true"></span>Opening…{:else}Open TradingView →{/if}
    </button>
    {#if tvStatus?.running}
      <button on:click={stopTvProxy} disabled={tvBusy !== "idle"} title="Stop the mitmproxy sidecar (closes the proxy, the browser window stays)">
        {tvBusy === "stop" ? "Stopping…" : "Stop proxy"}
      </button>
    {/if}
    <button on:click={setupTvProxy} disabled={tvBusy !== "idle"} title="Reinstall the venv and refresh the cert (rare)">
      Reinstall
    </button>
  </div>
  <p class="hint">
    Cascada will open TradingView in {tvStatus?.browserPath?.includes("Edge") ? "Edge" : tvStatus?.browserPath?.includes("Brave") ? "Brave" : "Chrome"} with a sandboxed profile.
    Place any tiny trade once you're in — your master account appears here automatically.
  </p>
  <p class="hint">
    Email/password and Google/Apple OAuth all work — Cascada tunnels Google + Apple
    auth domains around mitmproxy so Chrome accepts the real provider cert. TradingView
    traffic itself stays intercepted.
  </p>
{/if}

<style>
  .tv-warning {
    display: block;
    margin-bottom: 12px;
    padding: 8px 12px;
    border-radius: 6px;
    background: linear-gradient(135deg, rgba(249, 115, 22, 0.12), rgba(245, 158, 11, 0.12));
    border-left: 3px solid #f97316;
    font-size: 13px;
    color: var(--text);
  }
  .tv-warning strong { color: #c2410c; }

  .tv-status {
    display: flex; flex-wrap: wrap; align-items: center; gap: 10px;
    font-size: 12px;
  }
  .tv-pill {
    display: inline-flex; align-items: center; gap: 6px;
    font-weight: 600; font-size: 11px; letter-spacing: 0.02em;
    padding: 3px 9px; border-radius: 999px;
    border: 1px solid transparent;
  }
  .tv-pill::before {
    content: ""; width: 7px; height: 7px; border-radius: 50%;
    background: currentColor;
  }
  .tv-pill.on   { background: #f0fdf4; color: #166534; border-color: #bbf7d0; }
  .tv-pill.idle { background: #fef9c3; color: #92400e; border-color: #fde68a; }
  .tv-pill.off  { background: #f1f5f9; color: #475569; border-color: #cbd5e1; }
  .tv-err { color: #b91c1c; }

  .install-row { display: flex; gap: 8px; flex-wrap: wrap; }

  .py-required, .browser-required {
    display: flex; flex-direction: column; gap: 12px;
    padding: 14px 16px;
    border: 1px solid #FED7AA;
    background: linear-gradient(135deg, #FFF7ED, #FEFCE8);
    border-radius: 8px;
  }
  .py-required-head { display: flex; flex-direction: column; gap: 2px; }
  .py-required-head strong { font-size: 13px; color: #7C2D12; }
  .py-steps {
    margin: 0; padding-left: 22px;
    font-size: 13px; color: var(--text);
    line-height: 1.7;
  }
  .py-steps em { color: #7C2D12; font-style: normal; font-weight: 600; }
  .btn-link {
    display: inline-flex; align-items: center;
    padding: 8px 14px; border-radius: 6px;
    font-size: 13px; font-weight: 500;
    text-decoration: none;
  }
  .btn-link.primary { background: var(--primary); color: #fff; }
  .btn-link.primary:hover { filter: brightness(1.06); }

  .spinner {
    display: inline-block;
    width: 12px; height: 12px;
    margin-right: 8px;
    border: 2px solid currentColor;
    border-right-color: transparent;
    border-radius: 50%;
    animation: spin 0.7s linear infinite;
    vertical-align: -2px;
    opacity: 0.85;
  }
  @keyframes spin { to { transform: rotate(360deg); } }

  .setup-progress {
    display: flex; flex-direction: column; gap: 6px;
    padding: 4px 0;
  }
  .setup-progress-bar {
    height: 4px; border-radius: 999px;
    background: var(--border);
    overflow: hidden;
  }
  .setup-progress-fill {
    height: 100%; width: 35%; border-radius: 999px;
    background: linear-gradient(90deg, var(--primary), #60a5fa);
    animation: setup-slide 1.4s ease-in-out infinite;
  }
  @keyframes setup-slide {
    0%   { transform: translateX(-100%); }
    100% { transform: translateX(380%); }
  }
</style>
