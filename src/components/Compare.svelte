<script lang="ts">
  import { onMount, onDestroy, createEventDispatcher } from "svelte";
  import { api, defaultRule, type Account, type CopyRule, type Quote } from "../lib/api";

  const dispatch = createEventDispatcher();

  export let accounts: Account[] = [];
  export let rules: CopyRule[] = [];

  $: masters = accounts.filter((a) => a.role === "Master");
  $: slaves  = accounts.filter((a) => a.role === "Slave");

  let masterId = "";
  let slaveId  = "";
  type Pair = { master: string; slave: string };
  let pairs: Pair[] = [{ master: "", slave: "" }];
  let quotes = new Map<string, Quote>();   // key = `${accountId}|${SYMBOL}`
  let symbolsByAccount = new Map<string, string[]>();
  let loadingSymbols = new Set<string>();
  let unlistenQuote: (() => void) | null = null;
  let unlistenSymbols: (() => void) | null = null;

  $: if (masters.length && !masters.find((m) => m.id === masterId)) masterId = masters[0].id;
  $: if (slaves.length  && !slaves.find((s) => s.id === slaveId))   slaveId  = slaves[0].id;

  // Auto-resubscribe whenever the selected accounts change. Tracks the
  // previous IDs so we can also drop the stale account's subscription
  // (otherwise quotes keep streaming on a side the user no longer cares about).
  let _prevMasterId = "";
  let _prevSlaveId = "";
  $: if (masterId !== _prevMasterId || slaveId !== _prevSlaveId) {
    const stale = [
      _prevMasterId && _prevMasterId !== masterId ? _prevMasterId : null,
      _prevSlaveId  && _prevSlaveId  !== slaveId  ? _prevSlaveId  : null,
    ].filter(Boolean) as string[];
    _prevMasterId = masterId; _prevSlaveId = slaveId;
    // Fire-and-forget; failures are non-fatal (account offline → backend logs it).
    (async () => {
      for (const id of stale) {
        try { await api.subscribeSymbols(id, []); } catch {}
      }
      if (masterId && slaveId) {
        try { await applySubscription(); } catch {}
      }
    })();
  }

  $: masterSymbols = symbolsByAccount.get(masterId) ?? [];
  $: slaveSymbols  = symbolsByAccount.get(slaveId)  ?? [];

  // Memoize per-symbol pip size — each quote tick calls diffPips() which hits
  // this for every pair, and `toUpperCase()` + three substring tests is wasted
  // work when the symbol set is tiny and stable.
  const pipSizeCache = new Map<string, number>();
  function pipSize(sym: string): number {
    let v = pipSizeCache.get(sym);
    if (v !== undefined) return v;
    const s = sym.toUpperCase();
    v = s.includes("JPY") ? 0.01
      : s.startsWith("XAU") || s.startsWith("XAG") ? 0.1
      : 0.0001;
    pipSizeCache.set(sym, v);
    return v;
  }

  function key(accountId: string, sym: string) {
    return `${accountId}|${sym.toUpperCase()}`;
  }

  function getQuote(accountId: string, sym: string): Quote | undefined {
    if (!sym) return undefined;
    return quotes.get(key(accountId, sym));
  }

  function diffPips(p: Pair): number | null {
    const m = getQuote(masterId, p.master);
    const s = getQuote(slaveId,  p.slave || p.master);
    if (!m || !s) return null;
    const mid = (a: Quote) => (a.bid + a.ask) / 2;
    return (mid(s) - mid(m)) / pipSize(p.master);
  }

  function spreadPips(q: Quote | undefined, sym: string): number | null {
    if (!q || !sym) return null;
    return (q.ask - q.bid) / pipSize(sym);
  }

  function fmt(n: number | null | undefined, d = 5) {
    return n == null || Number.isNaN(n) ? "—" : n.toFixed(d);
  }

  function addRow() { pairs = [...pairs, { master: "", slave: "" }]; }
  function removeRow(i: number) {
    pairs = pairs.filter((_, idx) => idx !== i);
    if (pairs.length === 0) addRow();
    applySubscription();
  }
  function syncSlaveDefault(i: number) {
    // If slave is empty when user types master, pre-fill with the master symbol.
    if (!pairs[i].slave) {
      pairs[i].slave = pairs[i].master;
      pairs = pairs;
    }
  }

  async function applySubscription() {
    if (!masterId || !slaveId) return;
    const ms = Array.from(new Set(pairs.map((p) => p.master.trim().toUpperCase()).filter(Boolean)));
    const ss = Array.from(new Set(pairs.map((p) => (p.slave || p.master).trim().toUpperCase()).filter(Boolean)));
    await Promise.all([
      api.subscribeSymbols(masterId, ms),
      api.subscribeSymbols(slaveId,  ss),
    ]);
  }

  async function stop() {
    if (masterId) await api.subscribeSymbols(masterId, []);
    if (slaveId)  await api.subscribeSymbols(slaveId,  []);
    quotes = new Map();
    lastTickAt = new Map();
  }

  const loadingTimers = new Map<string, ReturnType<typeof setTimeout>>();
  async function refreshSymbols(id: string) {
    if (!id || loadingSymbols.has(id)) return;
    loadingSymbols.add(id); loadingSymbols = loadingSymbols;
    try {
      const ok = await api.requestSymbols(id);
      if (!ok) {
        // Account offline — still try cached.
        const cached = await api.listAccountSymbols(id);
        symbolsByAccount.set(id, cached); symbolsByAccount = symbolsByAccount;
      }
    } finally {
      // Stop the spinner after a short window even if no event arrives.
      const prev = loadingTimers.get(id);
      if (prev) clearTimeout(prev);
      const h = setTimeout(() => {
        loadingTimers.delete(id);
        loadingSymbols.delete(id); loadingSymbols = loadingSymbols;
      }, 5000);
      loadingTimers.set(id, h);
    }
  }

  // Auto-suggest a slave equivalent: exact match, then case-insensitive,
  // then the same name with the slave's typical prefix/suffix stripped.
  function bestSlaveMatch(masterSym: string): string {
    const u = masterSym.trim().toUpperCase();
    if (!u || slaveSymbols.length === 0) return masterSym;
    if (slaveSymbols.includes(u)) return u;
    const ci = slaveSymbols.find((s) => s.toUpperCase() === u);
    if (ci) return ci;
    const contains = slaveSymbols.find((s) => s.toUpperCase().includes(u) || u.includes(s.toUpperCase()));
    return contains ?? masterSym;
  }

  // ─── Sampling: capture median pip-diff over a window, push to a rule ──────
  type Sampling = {
    samples: number[];
    durationMs: number;
    startedAt: number;
    elapsedMs: number;
    medianPips?: number;
    appliedRuleIds: Set<string>;
  };
  let sampling = new Map<number, Sampling>();   // key = pair index
  let durationByRow = new Map<number, number>();
  const DEFAULT_DURATION = 15000;
  let tickHandle: ReturnType<typeof setInterval> | null = null;

  function getDuration(i: number): number {
    return durationByRow.get(i) ?? DEFAULT_DURATION;
  }
  function setDuration(i: number, ms: number) {
    durationByRow.set(i, ms);
    durationByRow = durationByRow;
  }

  function startCapture(i: number) {
    const ms = getDuration(i);
    sampling.set(i, {
      samples: [], durationMs: ms, startedAt: Date.now(), elapsedMs: 0,
      medianPips: undefined, appliedRuleIds: new Set(),
    });
    sampling = sampling;
    if (!tickHandle) tickHandle = setInterval(tickSampling, 250);
  }

  function cancelCapture(i: number) {
    sampling.delete(i); sampling = sampling;
    if (sampling.size === 0 && tickHandle) { clearInterval(tickHandle); tickHandle = null; }
  }

  function tickSampling() {
    const t = Date.now();
    let anyActive = false;
    for (const s of sampling.values()) {
      if (s.medianPips !== undefined) continue;
      s.elapsedMs = t - s.startedAt;
      if (s.elapsedMs >= s.durationMs) {
        s.medianPips = median(s.samples);
      } else {
        anyActive = true;
      }
    }
    sampling = sampling;
    if (!anyActive && tickHandle) { clearInterval(tickHandle); tickHandle = null; }
  }

  function median(xs: number[]): number {
    if (xs.length === 0) return NaN;
    const sorted = [...xs].sort((a, b) => a - b);
    const m = Math.floor(sorted.length / 2);
    return sorted.length % 2 ? sorted[m] : (sorted[m - 1] + sorted[m]) / 2;
  }

  function pushSampleIfActive(i: number, p: Pair) {
    const s = sampling.get(i);
    if (!s || s.medianPips !== undefined) return;
    const d = diffPips(p);
    if (d != null && Number.isFinite(d)) s.samples.push(d);
  }

  $: matchingRules = rules.filter((r) => r.master_id === masterId && r.slave_id === slaveId);

  // Precomputed symbol → pair-indices lookup: a single quote event touches
  // at most a handful of rows instead of walking `pairs` every tick.
  $: masterPairIndex = (() => {
    const m = new Map<string, number[]>();
    pairs.forEach((p, i) => {
      const k = p.master.trim().toUpperCase();
      if (!k) return;
      const list = m.get(k); if (list) list.push(i); else m.set(k, [i]);
    });
    return m;
  })();
  $: slavePairIndex = (() => {
    const m = new Map<string, number[]>();
    pairs.forEach((p, i) => {
      const k = (p.slave || p.master).trim().toUpperCase();
      if (!k) return;
      const list = m.get(k); if (list) list.push(i); else m.set(k, [i]);
    });
    return m;
  })();

  /** Does the rule already carry this exact pip value for this symbol? Used to
   * disable the apply button / annotate the dropdown so the user isn't tricked
   * into a useless round-trip. Tolerance = 0.005 pip (we round to .01 anyway). */
  function ruleHasOffset(r: CopyRule, sym: string, pips: number): boolean {
    const u = sym.toUpperCase();
    const cur = r.quote_offsets.find((o) => o.symbol.toUpperCase() === u);
    return cur != null && Math.abs(cur.pips - pips) < 0.005;
  }

  async function applyToRule(i: number, ruleId: string) {
    const s = sampling.get(i);
    if (!s || s.medianPips == null || !Number.isFinite(s.medianPips)) return;
    const r = rules.find((x) => x.id === ruleId);
    if (!r) return;
    const sym = pairs[i].master.trim().toUpperCase();
    if (!sym) return;
    const pips = Number(s.medianPips.toFixed(2));
    // Already there with the same value → no-op, but mark as applied so the
    // UI flips to ✓ and the user sees it's effectively done.
    if (ruleHasOffset(r, sym, pips)) {
      s.appliedRuleIds.add(ruleId);
      sampling = sampling;
      return;
    }
    const next: CopyRule = {
      ...r,
      quote_offsets: [
        ...r.quote_offsets.filter((o) => o.symbol.toUpperCase() !== sym),
        { symbol: sym, pips },
      ],
    };
    try {
      await api.upsertRule(next);
      s.appliedRuleIds.add(ruleId);
      sampling = sampling;
      dispatch("refresh"); // tell App.svelte to re-fetch rules so Rules tab + matchingRules reflect the new offset
    } catch (e) {
      console.error("upsertRule failed", e);
      alert("Failed to update rule: " + e);
    }
  }

  /** Create a fresh rule for the current Master↔Slave pair, with the captured
   * median already populated as a quote_offset. Lets the user skip the trip
   * to the Copy rules tab when they're capturing for a brand-new link. */
  async function createRuleFromCapture(i: number) {
    const s = sampling.get(i);
    if (!s || s.medianPips == null || !Number.isFinite(s.medianPips)) return;
    if (!masterId || !slaveId) return;
    const sym = pairs[i].master.trim().toUpperCase();
    if (!sym) return;
    const fresh = defaultRule(masterId, slaveId);
    fresh.name = `${sym} ${masterAcct?.platform ?? ""}→${slaveAcct?.platform ?? ""}`.trim();
    fresh.quote_offsets = [{ symbol: sym, pips: Number(s.medianPips.toFixed(2)) }];
    try {
      const saved = await api.upsertRule(fresh);
      // Optimistically reflect in local rules list so the badge updates immediately.
      rules = [...rules, saved];
      s.appliedRuleIds.add(saved.id);
      sampling = sampling;
      dispatch("refresh"); // sync App.svelte → Rules tab will see the new rule too
    } catch (e) {
      console.error("create rule failed", e);
      alert("Failed to create rule: " + e);
    }
  }

  $: masterAcct = accounts.find((a) => a.id === masterId);
  $: slaveAcct  = accounts.find((a) => a.id === slaveId);

  function autoFillSlave(i: number) {
    const guess = bestSlaveMatch(pairs[i].master);
    if (guess && guess !== pairs[i].slave) {
      pairs[i].slave = guess;
      pairs = pairs;
    }
  }

  // Per-account last-tick timestamp drives the "live" pulse dot in the header.
  // Refreshed whenever a quote arrives; nuked on Stop.
  let lastTickAt = new Map<string, number>();
  let now = Date.now();
  let _liveTimer: ReturnType<typeof setInterval> | null = null;
  function isLive(id: string): boolean {
    const t = lastTickAt.get(id);
    return !!t && (now - t) < 2500;
  }

  onMount(async () => {
    _liveTimer = setInterval(() => { now = Date.now(); }, 500);
    unlistenQuote = await api.onQuote((q) => {
      quotes.set(key(q.account_id, q.symbol), q);
      quotes = quotes;
      lastTickAt.set(q.account_id, Date.now());
      lastTickAt = lastTickAt;
      // Feed any active samplers on the rows that this quote affects.
      if (sampling.size === 0) return;
      const sym = q.symbol.toUpperCase();
      const hits = q.account_id === masterId ? masterPairIndex.get(sym)
                 : q.account_id === slaveId  ? slavePairIndex.get(sym)
                 : undefined;
      if (!hits) return;
      for (const i of hits) pushSampleIfActive(i, pairs[i]);
    });
    unlistenSymbols = await api.onSymbols((id, syms) => {
      symbolsByAccount.set(id, syms);
      symbolsByAccount = symbolsByAccount;
      loadingSymbols.delete(id);
      loadingSymbols = loadingSymbols;
    });
    // Hydrate any cached lists for the currently selected accounts.
    if (masterId) {
      const cached = await api.listAccountSymbols(masterId);
      if (cached.length) { symbolsByAccount.set(masterId, cached); symbolsByAccount = symbolsByAccount; }
    }
    if (slaveId) {
      const cached = await api.listAccountSymbols(slaveId);
      if (cached.length) { symbolsByAccount.set(slaveId, cached); symbolsByAccount = symbolsByAccount; }
    }
  });

  onDestroy(async () => {
    if (unlistenQuote) unlistenQuote();
    if (unlistenSymbols) unlistenSymbols();
    if (tickHandle) { clearInterval(tickHandle); tickHandle = null; }
    if (_liveTimer) { clearInterval(_liveTimer); _liveTimer = null; }
    for (const h of loadingTimers.values()) clearTimeout(h);
    loadingTimers.clear();
    try { await stop(); } catch { /* ignore */ }
  });

  function platformBadge(p?: string): string {
    if (p === "cTrader") return "cT";
    if (p === "MT4") return "M4";
    if (p === "MT5") return "M5";
    return "?";
  }
  function platformClass(p?: string): string {
    if (p === "cTrader") return "pf-ct";
    if (p === "MT4") return "pf-mt4";
    if (p === "MT5") return "pf-mt5";
    return "";
  }
</script>

<div class="compare">
  <header class="hd">
    <div class="hd-title">
      <h2>Quote compare</h2>
      {#if isLive(masterId) || isLive(slaveId)}
        <span class="live-pill"><span class="live-dot"></span> live</span>
      {/if}
    </div>
    <p class="sub">Live bid/ask side-by-side. Capture the median pip diff over a window, then push it straight into a copy rule.</p>
  </header>

  {#if masters.length === 0 || slaves.length === 0}
    <div class="empty">
      <div class="empty-icon">⇄</div>
      <p>You need at least one <strong>Master</strong> and one <strong>Slave</strong> account to compare quotes.</p>
    </div>
  {:else}
    <div class="picker">
      <!-- Master card -->
      <div class="acct-card master">
        <div class="acct-head">
          <span class="lbl-tag">Master</span>
          <span class="live-state" class:on={isLive(masterId)} title={isLive(masterId) ? "Receiving quotes" : "No recent quote"}>
            <span class="live-dot"></span>
          </span>
        </div>
        <div class="acct-row">
          <span class="pf-badge {platformClass(masterAcct?.platform)}">{platformBadge(masterAcct?.platform)}</span>
          <select class="acct-select" bind:value={masterId}>
            {#each masters as m}<option value={m.id}>{m.label || m.login}</option>{/each}
          </select>
        </div>
        <div class="acct-meta">
          <span class="login-chip" title="Broker login">#{masterAcct?.login ?? "—"}</span>
          <button class="sym-pill"
                  title="Ask the EA to dump its symbol list"
                  disabled={loadingSymbols.has(masterId)}
                  on:click={() => refreshSymbols(masterId)}>
            {#if loadingSymbols.has(masterId)}
              <span class="spin">↻</span>
            {:else}
              ↻
            {/if}
            <strong>{masterSymbols.length || 0}</strong> symbols
          </button>
        </div>
      </div>

      <!-- Center action column -->
      <div class="vs-column">
        <div class="vs-arrow" aria-hidden="true">
          <svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
            <path d="M3 8h14m0 0l-4-4m4 4l-4 4M21 16H7m0 0l4-4m-4 4l4 4"/>
          </svg>
        </div>
        <div class="vs-actions">
          <button class="btn-ghost" title="Stop streaming on both sides and clear quotes" on:click={stop}>
            ⏸ Stop
          </button>
          <button class="btn-link" title="Re-send the subscription if quotes look stuck" on:click={applySubscription}>
            ↻ Resync
          </button>
        </div>
      </div>

      <!-- Slave card -->
      <div class="acct-card slave">
        <div class="acct-head">
          <span class="lbl-tag slave-tag">Slave</span>
          <span class="live-state" class:on={isLive(slaveId)} title={isLive(slaveId) ? "Receiving quotes" : "No recent quote"}>
            <span class="live-dot"></span>
          </span>
        </div>
        <div class="acct-row">
          <span class="pf-badge {platformClass(slaveAcct?.platform)}">{platformBadge(slaveAcct?.platform)}</span>
          <select class="acct-select" bind:value={slaveId}>
            {#each slaves as s}<option value={s.id}>{s.label || s.login}</option>{/each}
          </select>
        </div>
        <div class="acct-meta">
          <span class="login-chip" title="Broker login">#{slaveAcct?.login ?? "—"}</span>
          <button class="sym-pill"
                  title="Ask the EA to dump its symbol list"
                  disabled={loadingSymbols.has(slaveId)}
                  on:click={() => refreshSymbols(slaveId)}>
            {#if loadingSymbols.has(slaveId)}
              <span class="spin">↻</span>
            {:else}
              ↻
            {/if}
            <strong>{slaveSymbols.length || 0}</strong> symbols
          </button>
        </div>
      </div>
    </div>

    <datalist id="dl-master">
      {#each masterSymbols as s (s)}<option value={s}></option>{/each}
    </datalist>
    <datalist id="dl-slave">
      {#each slaveSymbols as s (s)}<option value={s}></option>{/each}
    </datalist>

    <div class="table-wrap">
      <table>
        <thead>
          <tr>
            <th class="grp grp-m" colspan="4">Master</th>
            <th class="grp grp-d">Δ</th>
            <th class="grp grp-s" colspan="4">Slave</th>
            <th class="grp grp-c">Capture</th>
            <th class="grp"></th>
          </tr>
          <tr class="sub-head">
            <th class="sym mcell">Symbol</th>
            <th class="num mcell">Bid</th>
            <th class="num mcell">Ask</th>
            <th class="num mcell sep">Sprd</th>
            <th class="num diff-h">pips</th>
            <th class="sym scell">Symbol</th>
            <th class="num scell">Bid</th>
            <th class="num scell">Ask</th>
            <th class="num scell sep">Sprd</th>
            <th class="capture-col"></th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {#each pairs as p, i (i)}
            {@const m = getQuote(masterId, p.master)}
            {@const s = getQuote(slaveId, p.slave || p.master)}
            {@const d = diffPips(p)}
            {@const samp = sampling.get(i)}
            {@const diffMag = d == null ? 0 : Math.abs(d)}
            {@const diffClass = d == null ? "" : diffMag >= 5 ? "diff-strong" : diffMag >= 1 ? "diff-mid" : "diff-low"}
            <tr>
              <td class="sym mcell">
                <input class="sym-input" type="text" list="dl-master" placeholder="EURUSD"
                       bind:value={p.master}
                       on:input={() => syncSlaveDefault(i)}
                       on:change={() => { autoFillSlave(i); applySubscription(); }} />
              </td>
              <td class="num mcell">{fmt(m?.bid)}</td>
              <td class="num mcell">{fmt(m?.ask)}</td>
              <td class="num mcell sep dim">{fmt(spreadPips(m, p.master), 1)}</td>
              <td class="diff-cell {diffClass}">
                {#if d == null}
                  <span class="diff-empty">—</span>
                {:else}
                  <span class="diff-arrow">{d > 0 ? "▲" : d < 0 ? "▼" : "·"}</span>
                  <span class="diff-num">{(d > 0 ? "+" : "") + d.toFixed(1)}</span>
                {/if}
              </td>
              <td class="sym scell">
                <input class="sym-input" type="text" list="dl-slave" placeholder="(same as master)"
                       bind:value={p.slave}
                       on:change={applySubscription} />
              </td>
              <td class="num scell">{fmt(s?.bid)}</td>
              <td class="num scell">{fmt(s?.ask)}</td>
              <td class="num scell sep dim">{fmt(spreadPips(s, p.slave || p.master), 1)}</td>
              <td class="capture-col">
                {#if !samp}
                  <div class="cap-controls">
                    <select class="cap-dur"
                            on:change={(e) => setDuration(i, Number(e.currentTarget.value) * 1000)}
                            value={String(getDuration(i) / 1000)}>
                      <option value="5">5s</option>
                      <option value="15">15s</option>
                      <option value="30">30s</option>
                      <option value="60">60s</option>
                    </select>
                    <button class="cap-btn"
                            disabled={!p.master || !masterId || !slaveId}
                            on:click={() => startCapture(i)}>● Capture</button>
                  </div>
                {:else if samp.medianPips === undefined}
                  <div class="cap-progress">
                    <div class="cap-bar" style="width: {Math.min(100, (samp.elapsedMs / samp.durationMs) * 100)}%"></div>
                    <span class="cap-text">{Math.max(0, Math.ceil((samp.durationMs - samp.elapsedMs) / 1000))}s · {samp.samples.length} ticks</span>
                    <button class="cap-cancel" title="Cancel" on:click={() => cancelCapture(i)}>✕</button>
                  </div>
                {:else if samp.samples.length === 0 || !Number.isFinite(samp.medianPips)}
                  <div class="cap-result">
                    <span class="cap-warn" title="No quotes arrived during the capture window. Make sure the cBot/EA is loaded on this account, then press Stream.">
                      no samples — EA not streaming?
                    </span>
                    <button class="cap-cancel" title="Reset" on:click={() => cancelCapture(i)}>↺</button>
                  </div>
                {:else}
                  <div class="cap-result">
                    <span class="cap-median" title="{samp.samples.length} samples">
                      median {samp.medianPips > 0 ? "+" : ""}{samp.medianPips.toFixed(2)}p
                    </span>
                    {#if matchingRules.length === 0}
                      <button class="cap-apply"
                              title="Create a new copy rule for this Master ↔ Slave pair, with the captured offset pre-filled"
                              on:click={() => createRuleFromCapture(i)}>
                        + Create rule
                      </button>
                    {:else if matchingRules.length === 1}
                      {@const r0 = matchingRules[0]}
                      {@const sym0 = pairs[i].master.trim().toUpperCase()}
                      {@const upToDate0 = ruleHasOffset(r0, sym0, Number(samp.medianPips.toFixed(2)))}
                      <button class="cap-apply"
                              disabled={samp.appliedRuleIds.has(r0.id) || upToDate0}
                              title={upToDate0 ? "This rule already carries the same offset for this symbol" : ""}
                              on:click={() => applyToRule(i, r0.id)}>
                        {samp.appliedRuleIds.has(r0.id)
                          ? "✓ Saved"
                          : upToDate0
                            ? "✓ Already set"
                            : `→ ${r0.name?.trim() || "rule"}`}
                      </button>
                    {:else}
                      {@const sym0 = pairs[i].master.trim().toUpperCase()}
                      {@const pips0 = Number(samp.medianPips.toFixed(2))}
                      <select class="cap-rule" on:change={(e) => { const v = e.currentTarget.value; if (v) applyToRule(i, v); }}>
                        <option value="">Apply to rule…</option>
                        {#each matchingRules as r}
                          {@const upToDate = ruleHasOffset(r, sym0, pips0)}
                          <option value={r.id} disabled={samp.appliedRuleIds.has(r.id) || upToDate}>
                            {r.name?.trim() || "Untitled"}{samp.appliedRuleIds.has(r.id) ? " ✓" : upToDate ? " (à jour)" : ""}
                          </option>
                        {/each}
                      </select>
                    {/if}
                    <button class="cap-cancel" title="Reset" on:click={() => cancelCapture(i)}>↺</button>
                  </div>
                {/if}
              </td>
              <td class="actions-cell">
                <button class="row-x" title="Remove pair" on:click={() => removeRow(i)}>✕</button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
    <div class="footer-row">
      <button class="add-row" on:click={addRow}>+ Add pair</button>
      <p class="hint">
        Δ = (slave mid − master mid) / pip · positive = slave quotes higher than master.
      </p>
    </div>
  {/if}
</div>

<style>
  /* ─────────── Layout ─────────── */
  .compare { display: flex; flex-direction: column; gap: 18px; }

  /* ─────────── Header ─────────── */
  .hd-title { display: flex; align-items: center; gap: 10px; }
  .hd h2 { margin: 0; font-size: 19px; letter-spacing: -0.01em; }
  .sub { margin: 4px 0 0; color: var(--text-2); font-size: 13px; }
  .live-pill {
    display: inline-flex; align-items: center; gap: 6px;
    padding: 2px 8px;
    background: rgba(16,185,129,0.12);
    color: #047857;
    border: 1px solid rgba(16,185,129,0.3);
    border-radius: 999px;
    font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.05em;
  }
  .live-pill .live-dot {
    width: 6px; height: 6px; border-radius: 50%;
    background: #10b981;
    box-shadow: 0 0 0 0 rgba(16,185,129,0.6);
    animation: pulse 1.6s ease-out infinite;
  }
  @keyframes pulse {
    0% { box-shadow: 0 0 0 0 rgba(16,185,129,0.55); }
    70% { box-shadow: 0 0 0 8px rgba(16,185,129,0); }
    100% { box-shadow: 0 0 0 0 rgba(16,185,129,0); }
  }

  /* ─────────── Empty state ─────────── */
  .empty {
    padding: 40px 24px; text-align: center; color: var(--text-2);
    background: var(--surface); border: 1px dashed var(--border);
    border-radius: var(--radius);
  }
  .empty-icon {
    font-size: 32px; line-height: 1;
    margin-bottom: 12px;
    color: var(--primary);
    opacity: 0.5;
  }

  /* ─────────── Picker (Master / VS / Slave) ─────────── */
  .picker {
    display: grid;
    grid-template-columns: 1fr auto 1fr;
    gap: 16px;
    align-items: stretch;
  }
  .acct-card {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: 14px 16px;
    display: flex; flex-direction: column; gap: 10px;
    min-width: 0;
    position: relative;
    transition: border-color 0.15s ease;
  }
  .acct-card.master { border-top: 3px solid #2563eb; padding-top: 12px; }
  .acct-card.slave  { border-top: 3px solid #7c3aed; padding-top: 12px; }
  .acct-head { display: flex; justify-content: space-between; align-items: center; }
  .lbl-tag {
    font-size: 10px; font-weight: 700; letter-spacing: 0.08em;
    text-transform: uppercase; color: #2563eb;
  }
  .lbl-tag.slave-tag { color: #7c3aed; }
  .live-state {
    display: inline-flex; align-items: center;
    width: 10px; height: 10px;
  }
  .live-state .live-dot {
    width: 8px; height: 8px; border-radius: 50%;
    background: var(--text-2); opacity: 0.3;
    transition: background 0.2s, opacity 0.2s;
  }
  .live-state.on .live-dot {
    background: #10b981; opacity: 1;
    animation: pulse 1.6s ease-out infinite;
  }

  .acct-row {
    display: flex; align-items: center; gap: 10px;
    min-width: 0;
  }
  .pf-badge {
    flex: 0 0 auto;
    width: 32px; height: 32px;
    border-radius: 8px;
    display: inline-flex; align-items: center; justify-content: center;
    font-size: 11px; font-weight: 700;
    background: var(--surface-muted); color: var(--text-2);
    letter-spacing: 0.02em;
  }
  .pf-badge.pf-ct  { background: linear-gradient(135deg, #1e40af, #3b82f6); color: #fff; }
  .pf-badge.pf-mt4 { background: linear-gradient(135deg, #b91c1c, #ef4444); color: #fff; }
  .pf-badge.pf-mt5 { background: linear-gradient(135deg, #166534, #22c55e); color: #fff; }
  .acct-select {
    flex: 1; min-width: 0;
    border: 1px solid var(--border);
    background: var(--bg); color: var(--text);
    border-radius: var(--radius-sm);
    padding: 7px 10px;
    font-size: 13px; font-weight: 500;
  }
  .acct-meta { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
  .login-chip {
    font-size: 11px; color: var(--text-2);
    background: var(--surface-muted);
    padding: 3px 8px;
    border-radius: 6px;
    font-variant-numeric: tabular-nums;
  }
  .sym-pill {
    display: inline-flex; align-items: center; gap: 6px;
    padding: 3px 10px;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-2);
    border-radius: 999px;
    font-size: 11px;
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
  }
  .sym-pill strong { color: var(--text); font-weight: 600; }
  .sym-pill:hover:not(:disabled) { background: var(--surface-muted); color: var(--text); }
  .sym-pill:disabled { opacity: 0.6; cursor: progress; }
  .spin { display: inline-block; animation: spin 0.8s linear infinite; }
  @keyframes spin { to { transform: rotate(360deg); } }

  /* ─────────── VS column ─────────── */
  .vs-column {
    display: flex; flex-direction: column; align-items: center; justify-content: space-between;
    gap: 12px;
    padding: 4px 4px;
  }
  .vs-arrow {
    color: var(--text-2);
    opacity: 0.4;
    margin-top: 18px;
  }
  .vs-actions { display: flex; flex-direction: column; gap: 6px; width: 100px; }
  .btn-primary, .btn-ghost {
    padding: 8px 14px;
    border-radius: var(--radius-sm);
    font-size: 13px; font-weight: 500;
    cursor: pointer;
    border: 1px solid var(--border);
    transition: filter 0.15s, background 0.15s;
  }
  .btn-primary {
    background: linear-gradient(180deg, var(--primary), color-mix(in srgb, var(--primary) 85%, black));
    color: #fff; border-color: var(--primary);
    display: inline-flex; align-items: center; justify-content: center; gap: 6px;
    box-shadow: 0 1px 2px rgba(37,99,235,0.3);
  }
  .btn-primary:hover { filter: brightness(1.06); }
  .btn-primary .dot-tx {
    width: 6px; height: 6px; border-radius: 50%;
    background: #fff; opacity: 0.85;
  }
  .btn-ghost { background: var(--surface); color: var(--text); }
  .btn-ghost:hover { background: var(--surface-muted); }
  .btn-link {
    background: transparent; border: none;
    color: var(--text-2); font-size: 11px;
    padding: 4px 8px; cursor: pointer;
    text-decoration: none; border-radius: var(--radius-sm);
    transition: color 0.15s, background 0.15s;
  }
  .btn-link:hover { color: var(--primary); background: var(--surface-muted); }

  /* ─────────── Table ─────────── */
  .table-wrap {
    background: var(--surface); border: 1px solid var(--border);
    border-radius: var(--radius); overflow: auto;
  }
  table { width: 100%; border-collapse: collapse; font-size: 13px; }

  /* Group header row */
  th.grp {
    padding: 8px 10px; font-size: 10px; font-weight: 700;
    text-transform: uppercase; letter-spacing: 0.08em;
    color: var(--text-2); background: var(--surface-muted);
    border-bottom: 1px solid var(--border);
    text-align: center;
  }
  th.grp-m { color: #2563eb; background: rgba(37,99,235,0.06); }
  th.grp-s { color: #7c3aed; background: rgba(124,58,237,0.06); }
  th.grp-d { color: var(--text); background: var(--surface-muted); }
  th.grp-c { color: var(--text-2); }

  /* Sub-header */
  tr.sub-head th {
    padding: 6px 10px; font-size: 11px; font-weight: 600;
    color: var(--text-2);
    background: var(--surface);
    border-bottom: 1px solid var(--border);
    position: sticky; top: 0; z-index: 1;
  }
  tr.sub-head th.mcell { background: rgba(37,99,235,0.04); }
  tr.sub-head th.scell { background: rgba(124,58,237,0.04); }
  tr.sub-head th.diff-h { text-align: center; color: var(--text); font-weight: 700; }
  tr.sub-head th.sym { text-align: left; }
  tr.sub-head th.num { text-align: right; }
  th.sep, td.sep { border-right: 1px solid var(--border); }

  tbody tr { transition: background 0.12s; }
  tbody tr:hover { background: var(--surface-muted); }
  td {
    padding: 8px 10px;
    border-bottom: 1px solid var(--border);
  }
  tbody tr:last-child td { border-bottom: none; }
  td.mcell { background: rgba(37,99,235,0.025); }
  td.scell { background: rgba(124,58,237,0.025); }
  td.num { text-align: right; font-variant-numeric: tabular-nums; }
  td.dim { color: var(--text-2); font-size: 12px; }

  th.sym, td.sym { min-width: 140px; }
  .sym-input {
    width: 100%; padding: 5px 8px;
    border: 1px solid transparent;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text);
    font-size: 13px; font-weight: 600;
    font-variant-numeric: tabular-nums;
    transition: border-color 0.12s, background 0.12s;
  }
  .sym-input:hover { background: var(--bg); border-color: var(--border); }
  .sym-input:focus {
    outline: none;
    background: var(--bg);
    border-color: var(--primary);
    box-shadow: 0 0 0 3px rgba(37,99,235,0.12);
  }

  /* Hero diff cell */
  .diff-cell {
    text-align: center;
    padding: 6px 12px;
    font-variant-numeric: tabular-nums;
    border-left: 1px solid var(--border);
    border-right: 1px solid var(--border);
    background: var(--bg);
    min-width: 90px;
  }
  .diff-arrow { font-size: 11px; margin-right: 4px; opacity: 0.7; }
  .diff-num   { font-size: 15px; font-weight: 700; letter-spacing: -0.01em; }
  .diff-empty { color: var(--text-2); }
  .diff-low    { color: var(--text); }
  .diff-low    .diff-arrow { color: var(--text-2); }
  .diff-mid    { color: #b45309; }
  .diff-mid    .diff-arrow { color: #d97706; }
  .diff-strong { color: #b91c1c; background: rgba(239,68,68,0.06); }
  .diff-strong .diff-arrow { color: #dc2626; }

  /* Action cell */
  .actions-cell { width: 32px; text-align: center; }
  .row-x {
    border: none; background: transparent; cursor: pointer; color: var(--text-2);
    font-size: 13px; padding: 4px 6px;
    border-radius: 4px;
    transition: background 0.12s, color 0.12s;
  }
  .row-x:hover { color: #c2410c; background: rgba(220,38,38,0.08); }

  /* ─────────── Footer row ─────────── */
  .footer-row {
    display: flex; justify-content: space-between; align-items: center; gap: 12px;
    flex-wrap: wrap;
  }
  .add-row {
    padding: 8px 14px; font-size: 12px; font-weight: 500;
    border: 1px dashed var(--border); border-radius: var(--radius-sm);
    background: transparent; color: var(--text-2); cursor: pointer;
    transition: background 0.12s, color 0.12s, border-color 0.12s;
  }
  .add-row:hover {
    background: var(--surface-muted); color: var(--primary);
    border-color: var(--primary); border-style: solid;
  }
  .hint { font-size: 12px; color: var(--text-2); margin: 0; }

  /* ─────────── Capture column ─────────── */
  .capture-col { min-width: 240px; }
  .cap-controls { display: flex; gap: 6px; align-items: center; }
  .cap-dur {
    padding: 5px 8px; font-size: 12px;
    border: 1px solid var(--border); border-radius: var(--radius-sm);
    background: var(--bg); color: var(--text);
    font-variant-numeric: tabular-nums;
  }
  .cap-btn {
    padding: 5px 12px; font-size: 12px; font-weight: 600;
    border: 1px solid var(--primary); background: var(--primary); color: #fff;
    border-radius: var(--radius-sm); cursor: pointer;
    display: inline-flex; align-items: center; gap: 4px;
    transition: filter 0.15s;
  }
  .cap-btn:hover:not(:disabled) { filter: brightness(1.08); }
  .cap-btn:disabled {
    opacity: 0.5; cursor: not-allowed;
    background: var(--surface-muted); border-color: var(--border); color: var(--text-2);
  }

  .cap-progress {
    position: relative;
    display: flex; align-items: center; gap: 8px;
    padding: 6px 10px;
    background: rgba(59,130,246,0.08); border: 1px solid rgba(59,130,246,0.35);
    border-radius: var(--radius-sm);
    overflow: hidden;
    min-height: 28px;
  }
  .cap-bar {
    position: absolute; left: 0; top: 0; bottom: 0;
    background: linear-gradient(90deg, rgba(59,130,246,0.2), rgba(99,102,241,0.28));
    transition: width 0.2s linear;
    z-index: 0;
  }
  .cap-text {
    font-size: 11px; color: #1d4ed8; font-weight: 600; z-index: 1;
    font-variant-numeric: tabular-nums;
  }
  .cap-cancel {
    margin-left: auto; z-index: 1;
    border: none; background: transparent; cursor: pointer;
    color: var(--text-2); font-size: 12px;
    padding: 2px 4px; border-radius: 4px;
  }
  .cap-cancel:hover { color: var(--text); background: rgba(0,0,0,0.05); }

  .cap-result { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
  .cap-median {
    font-size: 12px; font-weight: 700; color: var(--text);
    padding: 4px 10px;
    background: linear-gradient(135deg, var(--surface-muted), var(--bg));
    border: 1px solid var(--border);
    border-radius: 999px;
    font-variant-numeric: tabular-nums;
  }
  .cap-apply {
    padding: 5px 12px; font-size: 12px; font-weight: 600;
    border: 1px solid #10b981; background: #10b981; color: #fff;
    border-radius: var(--radius-sm); cursor: pointer;
    display: inline-flex; align-items: center; gap: 4px;
    transition: filter 0.15s;
  }
  .cap-apply:hover:not(:disabled) { filter: brightness(1.08); }
  .cap-apply:disabled {
    background: rgba(16,185,129,0.12); color: #047857;
    border-color: rgba(16,185,129,0.3); cursor: default;
  }
  .cap-rule {
    padding: 5px 8px; font-size: 12px;
    border: 1px solid var(--border); border-radius: var(--radius-sm);
    background: var(--bg); color: var(--text);
  }
  .cap-warn {
    font-size: 11px; color: #c2410c; font-style: italic;
    background: rgba(220,38,38,0.06);
    padding: 3px 8px; border-radius: 6px;
  }
</style>
