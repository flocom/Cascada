<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { open, save, ask, message } from "@tauri-apps/plugin-dialog";
  import { api, type Account, type CopyRule, type LogEntry, type Trade } from "./lib/api";
  import Dashboard from "./components/Dashboard.svelte";
  import Accounts from "./components/Accounts.svelte";
  import Rules from "./components/Rules.svelte";
  import Trades from "./components/Trades.svelte";
  import Compare from "./components/Compare.svelte";
  import Logs from "./components/Logs.svelte";

  type Tab = "dashboard" | "accounts" | "rules" | "trades" | "compare" | "logs";
  let accounts: Account[] = [];
  let rules: CopyRule[] = [];
  let trades: Trade[] = [];
  let logs: LogEntry[] = [];
  let tab: Tab = "dashboard";
  let unlisten: Array<() => void> = [];

  const TRADE_CAP = 500;
  const LOG_CAP = 1000;
  // Push newest-first: callers do `pendingTrades.push(t)` then we reverse on flush.
  // unshift() is O(n); under a burst (e.g. history replay) this dominated the frame.
  let pendingTrades: Trade[] = [];
  let pendingLogs: LogEntry[] = [];
  // Account updates are coalesced into the same rAF so a pulsing heartbeat
  // doesn't trigger N sidebar re-renders per second.
  let pendingAccounts = new Map<string, Account>();
  let flushScheduled = false;
  function scheduleFlush() {
    if (flushScheduled) return;
    flushScheduled = true;
    requestAnimationFrame(() => {
      flushScheduled = false;
      if (pendingTrades.length) {
        pendingTrades.reverse();
        trades = pendingTrades.concat(trades).slice(0, TRADE_CAP);
        pendingTrades = [];
      }
      if (pendingLogs.length) {
        pendingLogs.reverse();
        logs = pendingLogs.concat(logs).slice(0, LOG_CAP);
        pendingLogs = [];
      }
      if (pendingAccounts.size) {
        let mutated = false;
        let appended: Account[] | null = null;
        for (const a of pendingAccounts.values()) {
          const i = accounts.findIndex((x) => x.id === a.id);
          if (i < 0) {
            (appended ??= []).push(a);
            continue;
          }
          const cur = accounts[i];
          if (cur.balance === a.balance && cur.equity === a.equity
              && cur.connected === a.connected && cur.role === a.role
              && cur.label === a.label && cur.currency === a.currency) continue;
          accounts[i] = a;
          mutated = true;
        }
        pendingAccounts.clear();
        if (appended) accounts = [...accounts, ...appended];
        else if (mutated) accounts = accounts;
      }
    });
  }

  async function refresh() {
    [accounts, rules, trades] = await Promise.all([
      api.listAccounts(),
      api.listRules(),
      api.listTrades(),
    ]);
  }

  onMount(async () => {
    await refresh();
    unlisten.push(await api.onAccountUpdate((a) => {
      // Coalesce into the next rAF — multiple heartbeats collapse to one
      // reactive pass for downstream components.
      pendingAccounts.set(a.id, a);
      scheduleFlush();
    }));
    unlisten.push(await api.onTrade((t) => {
      pendingTrades.push(t);
      scheduleFlush();
    }));
    unlisten.push(await api.onEvent((e) => {
      pendingLogs.push(e);
      scheduleFlush();
    }));
  });

  onDestroy(() => unlisten.forEach((fn) => fn()));

  let busy: "" | "export" | "import" = "";

  async function exportSettings() {
    if (busy) return;
    busy = "export";
    try {
      const stamp = new Date().toISOString().slice(0, 10);
      const path = await save({
        title: "Export Cascada settings",
        defaultPath: `cascada-settings-${stamp}.json`,
        filters: [{ name: "Cascada settings", extensions: ["json"] }],
      });
      if (!path) return;
      await api.exportSettings(path);
      await message(`Exported ${accounts.length} account(s) and ${rules.length} rule(s).`,
        { title: "Settings exported", kind: "info" });
    } catch (e) {
      await message(String(e), { title: "Export failed", kind: "error" });
    } finally {
      busy = "";
    }
  }

  async function importSettings() {
    if (busy) return;
    const ok = await ask(
      "Importing will replace ALL current accounts, rules and live connections. " +
      "Account passwords are not part of exports — you'll need to reconnect manually.",
      { title: "Import settings?", kind: "warning", okLabel: "Choose file…", cancelLabel: "Cancel" });
    if (!ok) return;
    busy = "import";
    try {
      const path = await open({
        title: "Import Cascada settings",
        multiple: false,
        filters: [{ name: "Cascada settings", extensions: ["json"] }],
      });
      if (!path || typeof path !== "string") return;
      const report = await api.importSettings(path);
      await refresh();
      await message(`Imported ${report.accounts} account(s) and ${report.rules} rule(s).`,
        { title: "Settings imported", kind: "info" });
    } catch (e) {
      await message(String(e), { title: "Import failed", kind: "error" });
    } finally {
      busy = "";
    }
  }

  // Rules whose master/slave references no longer fit the expected role —
  // surfaced as a badge on the Rules tab so the user is invited to review.
  // Only role matters here; deriving a dedicated {id → role} map means
  // balance/equity heartbeats don't retrigger this block.
  $: roleById = (() => {
    const m = new Map<string, string>();
    for (const a of accounts) m.set(a.id, a.role);
    return m;
  })();
  $: rulesNeedingAttention = rules.filter((r) => {
    const mr = roleById.get(r.master_id);
    const sr = roleById.get(r.slave_id);
    return mr !== "Master" || sr !== "Slave";
  }).length;

  const nav: { id: Tab; label: string; icon: string }[] = [
    { id: "dashboard", label: "Dashboard", icon: "M3 12l2-2 4 4 8-8 4 4" },
    { id: "accounts", label: "Accounts", icon: "M4 7h16M4 12h16M4 17h10" },
    { id: "rules", label: "Copy rules", icon: "M7 7h10M7 12h10M7 17h6" },
    { id: "trades", label: "Trades", icon: "M4 19V5M4 19h16M8 15V9M12 15V7M16 15v-4" },
    { id: "compare", label: "Compare", icon: "M3 6h12M3 12h18M3 18h9" },
    { id: "logs", label: "Logs", icon: "M5 5h14v14H5zM8 9h8M8 13h8M8 17h5" },
  ];
</script>

<div class="app">
  <aside>
    <div class="brand">
      <svg class="logo" viewBox="0 0 1024 1024" aria-hidden="true">
        <defs>
          <linearGradient id="brand-bg" x1="0" y1="0" x2="1" y2="1">
            <stop offset="0%" stop-color="#0EA5E9"/>
            <stop offset="100%" stop-color="#1E3A8A"/>
          </linearGradient>
          <linearGradient id="brand-bar" x1="0" y1="0" x2="1" y2="0">
            <stop offset="0%" stop-color="#FFFFFF" stop-opacity="0.95"/>
            <stop offset="100%" stop-color="#E0F2FE"/>
          </linearGradient>
        </defs>
        <rect width="1024" height="1024" rx="220" ry="220" fill="url(#brand-bg)"/>
        <rect x="130" y="290" width="520" height="96" rx="48" fill="url(#brand-bar)" opacity="0.75"/>
        <rect x="230" y="464" width="520" height="96" rx="48" fill="url(#brand-bar)" opacity="0.88"/>
        <rect x="330" y="638" width="520" height="96" rx="48" fill="url(#brand-bar)"/>
      </svg>
      <span>Cascada</span>
    </div>
    <nav>
      {#each nav as n}
        <button class="nav-item" class:active={tab === n.id} on:click={() => (tab = n.id)}>
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d={n.icon}/></svg>
          <span>{n.label}</span>
          {#if n.id === "rules" && rulesNeedingAttention > 0}
            <span class="nav-badge" title={`${rulesNeedingAttention} rule(s) need your attention`}>
              {rulesNeedingAttention}
            </span>
          {/if}
        </button>
      {/each}
    </nav>
    <div class="sidebar-footer">
      <div class="settings-row">
        <button class="settings-btn" title="Export accounts & rules to a JSON file"
                disabled={busy !== ""} on:click={exportSettings}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12m0 0l-4-4m4 4l4-4M5 21h14"/></svg>
          {busy === "export" ? "…" : "Export"}
        </button>
        <button class="settings-btn" title="Replace settings from a JSON file"
                disabled={busy !== ""} on:click={importSettings}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 21V9m0 0l-4 4m4-4l4 4M5 3h14"/></svg>
          {busy === "import" ? "…" : "Import"}
        </button>
      </div>
      <div class="status-line">
        <span class="dot" class:on={accounts.some(a => a.connected)}></span>
        <span class="muted">{accounts.filter(a => a.connected).length}/{accounts.length} connected</span>
      </div>
    </div>
  </aside>

  <main>
    <header>
      <h1>{nav.find(n => n.id === tab)?.label}</h1>
    </header>
    <section>
      {#if tab === "dashboard"}
        <Dashboard {accounts} {rules} {trades} />
      {:else if tab === "accounts"}
        <Accounts bind:accounts {rules} on:refresh={refresh} />
      {:else if tab === "rules"}
        <Rules bind:rules {accounts} on:refresh={refresh} />
      {:else if tab === "trades"}
        <Trades {trades} {accounts} />
      {:else if tab === "compare"}
        <Compare {accounts} {rules} on:refresh={refresh} />
      {:else if tab === "logs"}
        <Logs {logs} />
      {/if}
    </section>
  </main>
</div>

<style>
  .app { display: grid; grid-template-columns: 224px 1fr; height: 100vh; background: var(--bg); }
  aside {
    background: var(--surface);
    border-right: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    padding: 16px 12px;
    gap: 4px;
  }
  .brand { display: flex; align-items: center; gap: 10px; padding: 6px 8px 18px; font-weight: 600; font-size: 15px; }
  .logo {
    width: 26px; height: 26px; border-radius: 7px;
    display: block;
    box-shadow: 0 2px 6px rgba(37, 99, 235, 0.3);
  }
  nav { display: flex; flex-direction: column; gap: 2px; }
  .nav-item {
    display: flex; align-items: center; gap: 10px;
    padding: 8px 12px;
    border: none; background: transparent;
    color: var(--text-2);
    border-radius: var(--radius);
    font-size: 13px;
    font-weight: 500;
    text-align: left;
    width: 100%;
  }
  .nav-item:hover { background: var(--surface-muted); color: var(--text); }
  .nav-item.active { background: var(--primary-soft); color: var(--primary); }
  .nav-badge {
    margin-left: auto;
    background: #f59e0b; color: #fff;
    font-size: 10px; font-weight: 700;
    padding: 1px 6px; border-radius: 999px;
    line-height: 1.4;
  }
  .sidebar-footer { margin-top: auto; padding: 10px; border-top: 1px solid var(--border); display: flex; flex-direction: column; gap: 8px; }
  .settings-row { display: grid; grid-template-columns: 1fr 1fr; gap: 6px; }
  .settings-btn {
    display: inline-flex; align-items: center; justify-content: center; gap: 5px;
    padding: 6px 8px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--surface);
    color: var(--text-2);
    font-size: 12px; font-weight: 500;
    cursor: pointer;
  }
  .settings-btn:hover:not(:disabled) { background: var(--surface-muted); color: var(--text); }
  .settings-btn:disabled { opacity: 0.55; cursor: progress; }
  .status-line { display: flex; align-items: center; gap: 8px; font-size: 12px; }

  main { display: flex; flex-direction: column; overflow: hidden; }
  header {
    height: 56px; min-height: 56px;
    padding: 0 28px;
    display: flex; align-items: center;
    border-bottom: 1px solid var(--border);
    background: var(--surface);
  }
  section { flex: 1; overflow: auto; padding: 24px 28px; }
</style>
