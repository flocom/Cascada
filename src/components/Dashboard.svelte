<script lang="ts">
  import type { Account, CopyRule, Trade } from "../lib/api";
  import { accountIndex, formatTime, labelOf } from "../lib/format";
  export let accounts: Account[];
  export let rules: CopyRule[];
  export let trades: Trade[];

  $: idx = accountIndex(accounts);
  // Single pass over each array keeps the KPI block cheap under heartbeat pressure.
  $: acctStats = (() => {
    let connected = 0, totalEquity = 0, totalBalance = 0;
    for (const a of accounts) {
      if (a.connected) connected++;
      totalEquity += a.equity || 0;
      totalBalance += a.balance || 0;
    }
    return { connected, totalEquity, totalBalance };
  })();
  $: connected = acctStats.connected;
  $: totalEquity = acctStats.totalEquity;
  $: totalBalance = acctStats.totalBalance;
  $: activeRules = rules.reduce((n, r) => n + (r.enabled ? 1 : 0), 0);
  $: openPositions = trades.reduce((n, t) => n + (t.closed_at == null ? 1 : 0), 0);
  $: recentTrades = trades.slice(0, 8);
</script>

<div class="grid">
  <div class="kpi card">
    <div class="kpi-label">Total equity</div>
    <div class="kpi-value num">{totalEquity.toFixed(2)}</div>
    <div class="kpi-sub">Balance {totalBalance.toFixed(2)}</div>
  </div>
  <div class="kpi card">
    <div class="kpi-label">Accounts</div>
    <div class="kpi-value num">{connected}<span class="kpi-unit"> / {accounts.length}</span></div>
    <div class="kpi-sub">connected</div>
  </div>
  <div class="kpi card">
    <div class="kpi-label">Active rules</div>
    <div class="kpi-value num">{activeRules}</div>
    <div class="kpi-sub">{rules.length} total</div>
  </div>
  <div class="kpi card">
    <div class="kpi-label">Open positions</div>
    <div class="kpi-value num">{openPositions}</div>
    <div class="kpi-sub">{trades.length} trades logged</div>
  </div>
</div>

<div class="card" style="margin-top: 24px;">
  <div class="card-header">
    <h2>Recent activity</h2>
    <span class="chip">live</span>
  </div>
  {#if recentTrades.length === 0}
    <div class="empty">
      <p>No trades yet. Connect a master account and the activity will stream here.</p>
    </div>
  {:else}
    <table>
      <thead>
        <tr><th>Time</th><th>Account</th><th>Symbol</th><th>Side</th><th>Volume</th><th>Price</th><th>P/L</th></tr>
      </thead>
      <tbody>
        {#each recentTrades as t (t.ticket + t.account_id)}
          <tr>
            <td class="num">{formatTime(t.opened_at)}</td>
            <td>{labelOf(idx, t.account_id)}</td>
            <td>{t.symbol}</td>
            <td><span class="chip" class:success={t.side === "Buy"} class:danger={t.side === "Sell"}>{t.side}</span></td>
            <td class="num">{t.volume}</td>
            <td class="num">{t.price}</td>
            <td class="num" class:pos={(t.profit ?? 0) > 0} class:neg={(t.profit ?? 0) < 0}>
              {t.profit != null ? t.profit.toFixed(2) : "—"}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</div>

<style>
  .grid { display: grid; grid-template-columns: repeat(4, 1fr); gap: 16px; }
  .kpi { padding: 18px 20px; display: flex; flex-direction: column; gap: 4px; }
  .kpi-label { font-size: 12px; color: var(--text-muted); font-weight: 500; text-transform: uppercase; letter-spacing: 0.04em; }
  .kpi-value { font-size: 26px; font-weight: 600; color: var(--text); letter-spacing: -0.02em; }
  .kpi-unit { color: var(--text-muted); font-size: 18px; font-weight: 500; }
  .kpi-sub { font-size: 12px; color: var(--text-muted); }
</style>
