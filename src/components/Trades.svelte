<script lang="ts">
  import type { Account, Trade } from "../lib/api";
  import { accountIndex, formatTime, labelOf } from "../lib/format";
  import VirtualList from "../lib/VirtualList.svelte";
  export let trades: Trade[];
  export let accounts: Account[];
  $: idx = accountIndex(accounts);
</script>

<div class="card">
  <div class="card-header">
    <h2>Trades</h2>
    <span class="chip">{trades.length} recent</span>
  </div>
  {#if trades.length === 0}
    <div class="empty">No trades yet.</div>
  {:else}
    <div class="tbl">
      <div class="row head">
        <span>Time</span><span>Account</span><span>Ticket</span><span>Symbol</span><span>Side</span>
        <span class="right">Volume</span><span class="right">Price</span><span class="right">SL</span>
        <span class="right">TP</span><span class="right">P/L</span>
      </div>
      <div class="vl-wrap">
        <VirtualList items={trades} rowHeight={32}>
          <svelte:fragment let:item={t}>
            <div class="row">
              <span class="num muted">{formatTime(t.opened_at)}</span>
              <span class="strong">{labelOf(idx, t.account_id)}</span>
              <span class="num muted">{t.ticket}</span>
              <span>{t.symbol}</span>
              <span><span class="chip" class:success={t.side === "Buy"} class:danger={t.side === "Sell"}>{t.side}</span></span>
              <span class="right num">{t.volume}</span>
              <span class="right num">{t.price}</span>
              <span class="right num muted">{t.sl ?? "—"}</span>
              <span class="right num muted">{t.tp ?? "—"}</span>
              <span class="right num" class:pos={(t.profit ?? 0) > 0} class:neg={(t.profit ?? 0) < 0}>
                {t.profit != null ? t.profit.toFixed(2) : "open"}
              </span>
            </div>
          </svelte:fragment>
        </VirtualList>
      </div>
    </div>
  {/if}
</div>

<style>
  .tbl { display: flex; flex-direction: column; height: calc(100vh - 180px); min-height: 300px; }
  .row {
    display: grid;
    grid-template-columns: 110px 1fr 110px 90px 70px 80px 90px 90px 90px 90px;
    align-items: center;
    height: 32px;
    padding: 0 8px;
    border-bottom: 1px solid var(--border);
    font-size: 13px;
  }
  .row.head { font-weight: 600; color: var(--text-2); background: var(--surface-muted); border-bottom: 1px solid var(--border); }
  .right { text-align: right; }
  .vl-wrap { flex: 1; min-height: 0; }
</style>
