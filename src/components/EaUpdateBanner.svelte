<script lang="ts">
  import { onMount } from "svelte";
  import { api, type EaStatus, type Platform } from "../lib/api";

  let eaStatuses: EaStatus[] = [];
  let eaCheckBusy = false;
  let eaUpdateBusy = false;
  $: outdatedEas = eaStatuses.filter((s) => !s.up_to_date);
  $: outdatedPlatforms = Array.from(new Set(outdatedEas.map((s) => s.platform))) as Platform[];

  async function refreshEaVersions() {
    if (eaCheckBusy) return;
    eaCheckBusy = true;
    try { eaStatuses = await api.checkEaVersions(); }
    catch (e) { console.warn("[ea-version] check failed:", e); }
    finally { eaCheckBusy = false; }
  }
  async function updateAllOutdated() {
    if (eaUpdateBusy) return;
    eaUpdateBusy = true;
    try {
      for (const p of outdatedPlatforms) {
        if (p === "cTrader")        await api.installCtraderBot().catch(() => {});
        else if (p === "MT4" || p === "MT5") await api.installMtEa(p).catch(() => {});
      }
      await refreshEaVersions();
    } finally { eaUpdateBusy = false; }
  }
  onMount(refreshEaVersions);
</script>

{#if outdatedEas.length > 0}
  <div class="ea-update-banner" role="status">
    <span class="ea-dot"></span>
    <div class="ea-msg">
      <strong>{outdatedEas.length} EA{outdatedEas.length > 1 ? "s" : ""} out of date.</strong>
      <span class="muted small">
        {outdatedPlatforms.join(" · ")} — Cascada ships a newer build. Installed files will be overwritten.
      </span>
    </div>
    <button class="primary sm" on:click={updateAllOutdated} disabled={eaUpdateBusy}>
      {eaUpdateBusy ? "Updating…" : "Update now"}
    </button>
  </div>
{/if}

<style>
  .ea-update-banner {
    display: flex; align-items: center; gap: 12px;
    padding: 12px 16px;
    margin: 0 16px 0;
    border-top: 1px solid #FED7AA;
    background: linear-gradient(135deg, #FFF7ED, #FEF3C7);
    color: #7C2D12;
  }
  .ea-dot {
    width: 8px; height: 8px; border-radius: 50%;
    background: #F97316;
    box-shadow: 0 0 0 4px rgba(249, 115, 22, 0.18);
    flex-shrink: 0;
  }
  .ea-msg { flex: 1; display: flex; flex-direction: column; gap: 2px; min-width: 0; }
  .ea-msg .muted { color: #9A3412; opacity: 0.8; }
  .primary.sm { font-size: 12px; padding: 6px 12px; }
</style>
