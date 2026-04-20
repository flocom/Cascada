<script lang="ts">
  import { updateState, installUpdate } from "../lib/updater";
  $: s = $updateState;
  $: pctStr = s.kind === "downloading" ? `${s.pct.toFixed(0)}%` : "";
</script>

{#if s.kind === "available"}
  <button class="banner" on:click={installUpdate} title={s.notes ?? `Install version ${s.version} and relaunch`}>
    <span class="arrow">↑</span>
    <span class="text">
      <span class="line-1">Update available</span>
      <span class="line-2">v{s.version} · click to install</span>
    </span>
  </button>
{:else if s.kind === "downloading"}
  <div class="banner downloading" aria-live="polite">
    <span class="arrow spinning">↻</span>
    <span class="text">
      <span class="line-1">Downloading v{s.version}</span>
      <span class="line-2">{pctStr}</span>
    </span>
    <div class="bar"><div class="bar-fill" style:width={pctStr || "0%"}></div></div>
  </div>
{:else if s.kind === "ready"}
  <div class="banner ready">
    <span class="arrow">✓</span>
    <span class="text">
      <span class="line-1">Update installed</span>
      <span class="line-2">Relaunching…</span>
    </span>
  </div>
{:else if s.kind === "error"}
  <button class="banner error" on:click={installUpdate} title="Retry">
    <span class="arrow">!</span>
    <span class="text">
      <span class="line-1">Update failed</span>
      <span class="line-2">Click to retry</span>
    </span>
  </button>
{/if}

<style>
  .banner {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    padding: 10px 10px;
    margin-top: 4px;
    border: 1px solid #93C5FD;
    background: linear-gradient(135deg, #EFF6FF, #DBEAFE);
    color: #1E3A8A;
    border-radius: 8px;
    font: inherit;
    text-align: left;
    cursor: pointer;
    transition: background 120ms, transform 80ms;
  }
  .banner:hover { background: linear-gradient(135deg, #DBEAFE, #BFDBFE); }
  .banner:active { transform: translateY(1px); }
  .banner.downloading { cursor: default; background: #F1F5F9; border-color: var(--border); color: var(--text-2); flex-wrap: wrap; }
  .banner.ready       { cursor: default; background: #ECFDF5; border-color: #6EE7B7; color: #065F46; }
  .banner.error       { background: #FEF2F2; border-color: #FCA5A5; color: #991B1B; }
  .arrow {
    width: 22px; height: 22px;
    border-radius: 50%;
    display: grid; place-items: center;
    background: rgba(255,255,255,0.6);
    font-weight: 700;
    font-size: 12px;
    flex-shrink: 0;
  }
  .arrow.spinning { animation: spin 1s linear infinite; }
  @keyframes spin { to { transform: rotate(360deg); } }
  .text { display: flex; flex-direction: column; min-width: 0; }
  .line-1 { font-size: 12px; font-weight: 600; }
  .line-2 { font-size: 11px; opacity: 0.75; }
  .bar {
    width: 100%;
    height: 3px;
    background: #CBD5E1;
    border-radius: 2px;
    margin-top: 2px;
    overflow: hidden;
  }
  .bar-fill {
    height: 100%;
    background: #3B82F6;
    transition: width 200ms ease-out;
  }
</style>
