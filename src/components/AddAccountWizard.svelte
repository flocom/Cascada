<script lang="ts">
  import { api, type Platform } from "../lib/api";
  import { open as openDialog } from "@tauri-apps/plugin-dialog";
  import TvProxyPanel from "./TvProxyPanel.svelte";

  let mode: Platform = "cTrader";
  let installStatus: { kind: "info" | "ok" | "err"; text: string } | null = null;

  const platforms: { id: Platform; name: string; tag: string }[] = [
    { id: "cTrader", name: "cTrader", tag: "cBot · auto-discovered" },
    { id: "MT4",     name: "MetaTrader 4", tag: "EA · auto-discovered" },
    { id: "MT5",     name: "MetaTrader 5", tag: "EA · auto-discovered" },
    { id: "TradingView", name: "TradingView", tag: "Sidecar · auto-discovered" },
  ];

  // Hide the shared install banner when TvProxyPanel surfaces its own
  // "Python missing" panel (avoids duplicate red text).
  $: pythonMissing = mode === "TradingView"
    && installStatus?.kind === "err"
    && /python.*not found on path/i.test(installStatus.text);

  async function installCtraderBot() {
    installStatus = { kind: "info", text: "Scanning cTrader installs…" };
    try {
      const paths = await api.installCtraderBot();
      installStatus = { kind: "ok", text: `Installed into ${paths.length} install${paths.length > 1 ? "s" : ""}. Confirm the cTrader import dialog, attach the cBot to a chart and press Start — your account will appear here automatically.` };
    } catch (e) {
      installStatus = { kind: "err", text: `${e} — try "Pick location" instead.` };
    }
  }
  async function installCtraderBotManual() {
    const picked = await openDialog({ directory: true, title: "Select your cAlgo folder" });
    if (!picked || Array.isArray(picked)) return;
    installStatus = { kind: "info", text: "Installing…" };
    try {
      const p = await api.installCtraderBotAt(picked);
      installStatus = { kind: "ok", text: `Installed → ${p}. Attach the cBot to a chart and press Start.` };
    } catch (e) {
      installStatus = { kind: "err", text: `Failed: ${e}` };
    }
  }
  async function installMtEaAuto() {
    if (mode !== "MT4" && mode !== "MT5") return;
    const target: "MT4" | "MT5" = mode;
    installStatus = { kind: "info", text: `Scanning ${target} terminals…` };
    try {
      const paths = await api.installMtEa(target);
      installStatus = { kind: "ok", text: `EA installed into ${paths.length} terminal${paths.length > 1 ? "s" : ""}. Refresh the Navigator panel in ${target}, then drag CascadaBridge onto a chart.` };
    } catch (e) {
      installStatus = { kind: "err", text: `${e}` };
    }
  }
  async function installMtEaManual() {
    if (mode !== "MT4" && mode !== "MT5") return;
    const target: "MT4" | "MT5" = mode;
    const picked = await openDialog({
      directory: true,
      title: `Select the ${target} data folder (contains MQL${target === "MT4" ? "4" : "5"}/)`,
    });
    if (!picked || Array.isArray(picked)) return;
    installStatus = { kind: "info", text: "Installing EA…" };
    try {
      const p = await api.installMtEaAt(target, picked);
      installStatus = { kind: "ok", text: `EA copied → ${p}. Refresh the Navigator panel in ${target}.` };
    } catch (e) {
      installStatus = { kind: "err", text: `Failed: ${e}` };
    }
  }

  function selectPlatform(id: Platform) {
    mode = id;
    installStatus = null;
  }
</script>

<div class="wizard">
  <div class="platforms">
    {#each platforms as p}
      <button class="plat-card" class:active={mode === p.id}
              on:click={() => selectPlatform(p.id)}>
        <div class="plat-badge-row">
          <span class="plat-badge {p.id}">{p.id}</span>
        </div>
        <span class="plat-name">{p.name}</span>
        <span class="plat-tag">{p.tag}</span>
      </button>
    {/each}
  </div>

  <div class="instructions">
    {#if mode === "cTrader"}
      <p class="lead">Install the <code>CascadaBridge</code> cBot, attach it to any chart, press <strong>Start</strong>. Your account appears here automatically — no login or label needed.</p>
      <div class="install-row">
        <button class="primary" on:click={installCtraderBot}>Auto-install cBot</button>
        <button on:click={installCtraderBotManual}>Pick location…</button>
      </div>
    {:else if mode === "TradingView"}
      <TvProxyPanel bind:installStatus active={mode === "TradingView"} />
    {:else}
      <p class="lead">
        Install <code>CascadaBridge.{mode === "MT4" ? "mq4" : "mq5"}</code>, enable <strong>AutoTrading</strong>, and drag the EA onto any chart — your account appears here automatically. No network setup needed. Multiple {mode} terminals are supported in parallel.
      </p>
      <div class="install-row">
        <button class="primary" on:click={installMtEaAuto}>Auto-install Expert Advisor</button>
        <button on:click={installMtEaManual}>Pick location…</button>
      </div>
    {/if}
    {#if installStatus && !pythonMissing}
      <div class="inst-status {installStatus.kind}">{installStatus.text}</div>
    {/if}
  </div>
</div>

<style>
  .wizard {
    padding: 20px 22px 24px;
    border-bottom: 1px solid var(--border);
    background: linear-gradient(180deg, #fafbfc 0%, #ffffff 100%);
    display: flex; flex-direction: column; gap: 18px;
  }
  .platforms { display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: 10px; }
  .plat-card {
    display: flex; flex-direction: column; gap: 4px;
    padding: 14px 16px;
    border: 1.5px solid var(--border); border-radius: 10px;
    background: #fff; text-align: left; cursor: pointer;
    transition: all 0.12s ease;
  }
  .plat-card:hover { border-color: #cbd5e1; transform: translateY(-1px); }
  .plat-card.active { border-color: var(--primary); background: var(--primary-soft); box-shadow: 0 0 0 3px rgba(37, 99, 235, 0.08); }
  .plat-badge {
    align-self: flex-start;
    font-size: 10px; font-weight: 600; letter-spacing: 0.04em;
    padding: 2px 7px; border-radius: 4px;
    background: var(--surface-muted); color: var(--text-2);
  }
  .plat-badge.cTrader { background: #dbeafe; color: #1d4ed8; }
  .plat-badge.MT4     { background: #fef3c7; color: #a16207; }
  .plat-badge.MT5     { background: #dcfce7; color: #15803d; }
  .plat-badge.TradingView { background: #eff6ff; color: #1d4ed8; }
  .plat-badge-row { display: flex; align-items: center; gap: 6px; }
  .plat-name { font-size: 14px; font-weight: 600; color: var(--text); }
  .plat-tag  { font-size: 11px; color: var(--text-muted); }

  .instructions {
    padding: 14px 16px;
    background: var(--surface-muted);
    border: 1px solid var(--border);
    border-radius: 10px;
    display: flex; flex-direction: column; gap: 10px;
  }
  .lead { margin: 0; font-size: 13px; color: var(--text); }
  .lead code { background: #fff; padding: 1px 6px; border-radius: 4px; font-size: 12px; }
  .install-row { display: flex; gap: 8px; flex-wrap: wrap; }
  .inst-status {
    font-size: 12px; padding: 8px 12px; border-radius: 6px;
    border: 1px solid transparent;
  }
  .inst-status.info { background: #eff6ff; color: #1e40af; border-color: #bfdbfe; }
  .inst-status.ok   { background: #f0fdf4; color: #166534; border-color: #bbf7d0; }
  .inst-status.err  { background: #fef2f2; color: #991b1b; border-color: #fecaca; }
</style>
