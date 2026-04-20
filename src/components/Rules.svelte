<script lang="ts">
  import { createEventDispatcher } from "svelte";
  import { fly, fade } from "svelte/transition";
  import { api, defaultRule, type Account, type CopyRule, type LotMode } from "../lib/api";
  import { accountIndex, labelOf, platformOf } from "../lib/format";
  import { ask } from "@tauri-apps/plugin-dialog";

  export let rules: CopyRule[];
  export let accounts: Account[];
  const dispatch = createEventDispatcher();
  $: idx = accountIndex(accounts);
  $: masters = accounts.filter((a) => a.role === "Master");
  $: slaves  = accounts.filter((a) => a.role === "Slave" || a.role === "Idle");

  // Build the option lists for the edit drawer — always include the currently
  // bound account (even if its role changed), so editing a rule whose master
  // was demoted still shows that master in the dropdown rather than blanking
  // the selection on open and refusing to save.
  function masterOptions(currentId: string): Account[] {
    const list = masters.slice();
    if (currentId && !list.some((a) => a.id === currentId)) {
      const cur = idx.get(currentId);
      if (cur) list.push(cur);
    }
    return list;
  }
  function slaveOptions(currentId: string): Account[] {
    const list = slaves.slice();
    if (currentId && !list.some((a) => a.id === currentId)) {
      const cur = idx.get(currentId);
      if (cur) list.push(cur);
    }
    return list;
  }

  function ruleIssue(r: CopyRule): string | null {
    const m = idx.get(r.master_id);
    const s = idx.get(r.slave_id);
    if (!m) return "Master account missing";
    if (!s) return "Slave account missing";
    if (m.role !== "Master") return `Master is now ${m.role}`;
    if (s.role !== "Slave") return `Slave is now ${s.role}`;
    return null;
  }

  let editing: CopyRule | null = null;
  type TabId = "lot" | "filters" | "risk" | "orders" | "schedule" | "advanced";
  let activeTab: TabId = "lot";

  let mapPairs: [string, string][] = [];
  function loadMap(r: CopyRule) {
    mapPairs = Object.entries(r.symbol_map ?? {});
  }
  function syncMap() {
    if (!editing) return;
    const out: Record<string, string> = {};
    for (const [k, v] of mapPairs) {
      const kk = k.trim().toUpperCase();
      const vv = v.trim();
      if (kk && vv) out[kk] = vv;
    }
    editing.symbol_map = out;
  }
  function addMapping() { mapPairs = [...mapPairs, ["", ""]]; }
  function removeMapping(i: number) {
    mapPairs = mapPairs.filter((_, j) => j !== i);
    syncMap();
  }
  function updateMapping(i: number, side: 0 | 1, value: string) {
    mapPairs[i][side] = side === 0 ? value.toUpperCase() : value;
    mapPairs = mapPairs;
    syncMap();
  }

  function newDraft() { editing = defaultRule(); activeTab = "lot"; loadMap(editing); }
  function editRule(r: CopyRule) { editing = JSON.parse(JSON.stringify(r)); activeTab = "lot"; loadMap(editing!); }
  function cancel() { editing = null; mapPairs = []; }

  async function save() {
    if (!editing) return;
    if (!editing.master_id || !editing.slave_id) return;
    await api.upsertRule(editing);
    editing = null;
    dispatch("refresh");
  }
  async function toggle(r: CopyRule) {
    await api.upsertRule({ ...r, enabled: !r.enabled });
    dispatch("refresh");
  }
  async function remove(r: CopyRule) {
    const m = idx.get(r.master_id);
    const s = idx.get(r.slave_id);
    const pair = m && s ? ` (${labelOf(idx, r.master_id)} → ${labelOf(idx, r.slave_id)})` : "";
    const name = r.name?.trim() || "this rule";
    const ok = await ask(
      `Delete "${name}"${pair}?\n\nNew master trades will no longer be copied to this slave under this rule. Existing open positions are not touched.`,
      { title: "Delete copy rule?", kind: "warning", okLabel: "Delete", cancelLabel: "Cancel" });
    if (!ok) return;
    await api.deleteRule(r.id);
    dispatch("refresh");
  }

  function csvBind(arr: string[]): string { return arr.join(", "); }
  function fromCsv(s: string): string[] {
    return s.split(",").map((x) => x.trim()).filter(Boolean);
  }
  function minToHHMM(m: number): string {
    const h = Math.floor(m / 60), mm = m % 60;
    return `${String(h).padStart(2, "0")}:${String(mm).padStart(2, "0")}`;
  }
  function hhmmToMin(s: string): number {
    const [h, m] = s.split(":").map((x) => parseInt(x, 10));
    return (isFinite(h) ? h : 0) * 60 + (isFinite(m) ? m : 0);
  }

  const LOT_MODES: { id: LotMode; label: string; hint: string; icon: string }[] = [
    { id: "Multiplier",   label: "Multiplier",     hint: "slave = master × value",            icon: "✕" },
    { id: "Fixed",        label: "Fixed lots",     hint: "always open `value` lots",          icon: "▣" },
    { id: "Equity",       label: "Equity ratio",   hint: "scale by equity ratio × value",     icon: "≈" },
    { id: "BalanceRatio", label: "Balance ratio",  hint: "scale by balance ratio × value",    icon: "⚖" },
    { id: "RiskPercent",  label: "Risk %",         hint: "size by SL distance vs equity %",   icon: "%" },
  ];

  type Chip = { kind: "info" | "warn" | "danger" | "primary"; text: string };
  function chipsForRule(r: CopyRule): Chip[] {
    const out: Chip[] = [];
    out.push({ kind: "primary", text: r.lot_mode === "RiskPercent" ? `${r.lot_value}% risk` : `${r.lot_mode} ×${r.lot_value}` });
    if (r.reverse) out.push({ kind: "warn", text: "reverse" });
    if (r.direction !== "All") out.push({ kind: "info", text: r.direction === "BuyOnly" ? "buy only" : "sell only" });
    if (r.symbol_whitelist.length) out.push({ kind: "info", text: `whitelist · ${r.symbol_whitelist.length}` });
    if (r.symbol_blacklist.length) out.push({ kind: "warn", text: `blacklist · ${r.symbol_blacklist.length}` });
    if (r.symbol_prefix || r.symbol_suffix) out.push({ kind: "info", text: `${r.symbol_prefix}…${r.symbol_suffix}` });
    const mapN = Object.keys(r.symbol_map ?? {}).length;
    if (mapN) out.push({ kind: "info", text: `map · ${mapN}` });
    if (r.sl_mode !== "Copy") out.push({ kind: "info", text: `SL ${r.sl_mode === "Fixed" ? r.sl_pips + "p" : "off"}` });
    if (r.tp_mode !== "Copy") out.push({ kind: "info", text: `TP ${r.tp_mode === "Fixed" ? r.tp_pips + "p" : "off"}` });
    if (r.trailing_pips)       out.push({ kind: "info", text: `trail ${r.trailing_pips}p` });
    if (r.breakeven_after_pips)out.push({ kind: "info", text: `BE +${r.breakeven_after_pips}p` });
    if (r.max_slippage_pips)   out.push({ kind: "info", text: `slip ≤ ${r.max_slippage_pips}p` });
    if (r.min_lot)             out.push({ kind: "info", text: `min ${r.min_lot} lot` });
    if (r.max_lot)             out.push({ kind: "info", text: `max ${r.max_lot} lot` });
    const offN = r.quote_offsets?.length ?? 0;
    if (offN)                  out.push({ kind: "info", text: `offset · ${offN}` });
    if (r.quote_compensate)    out.push({ kind: "info", text: `drift comp${r.quote_skip_pips ? ` · skip>${r.quote_skip_pips}p` : ""}` });
    if (r.comment_filter)      out.push({ kind: "info", text: `cmt “${r.comment_filter}”` });
    if (r.skip_older_than_secs)out.push({ kind: "info", text: `skip >${r.skip_older_than_secs}s` });
    if (r.max_open_positions) out.push({ kind: "info", text: `≤ ${r.max_open_positions} pos` });
    if (r.max_exposure_lots)  out.push({ kind: "info", text: `≤ ${r.max_exposure_lots} lots` });
    if (r.max_daily_loss)     out.push({ kind: "danger", text: `−${r.max_daily_loss} stop` });
    if (r.schedule.enabled)   out.push({ kind: "info", text: `${minToHHMM(r.schedule.start_min)}–${minToHHMM(r.schedule.end_min)}` });
    if (r.schedule.skip_weekends) out.push({ kind: "info", text: "wkdays" });
    if (r.trade_delay_ms)     out.push({ kind: "info", text: `+${r.trade_delay_ms}ms` });
    return out;
  }

  // Single pass builds chips + issue string per rule, so we don't iterate
  // `rules` once for chips, once for issuesCount, then call ruleIssue() a
  // third time inside the template.
  $: ruleMeta = (() => {
    const m = new Map<string, { chips: Chip[]; issue: string | null }>();
    let issues = 0;
    for (const r of rules) {
      const issue = ruleIssue(r);
      if (issue) issues++;
      m.set(r.id, { chips: chipsForRule(r), issue });
    }
    return { map: m, issues };
  })();
  $: chipsByRule = new Map([...ruleMeta.map].map(([k, v]) => [k, v.chips]));
  $: issuesCount = ruleMeta.issues;

  const TABS: { id: TabId; label: string; icon: string; desc: string }[] = [
    { id: "lot",      label: "Lot sizing",    icon: "⚖", desc: "How slave volume is computed" },
    { id: "filters",  label: "Filters",       icon: "⛃", desc: "Which trades & symbols to copy" },
    { id: "risk",     label: "Risk caps",     icon: "🛡", desc: "Per-slave safety limits" },
    { id: "orders",   label: "Order shaping", icon: "✎", desc: "SL/TP, slippage, delay" },
    { id: "schedule", label: "Schedule",      icon: "⏱", desc: "Time-of-day window" },
    { id: "advanced", label: "Advanced",      icon: "⚙", desc: "Trailing & break-even" },
  ];
</script>

<div class="card rules-root">
  <div class="card-header">
    <div class="header-left">
      <h2>Copy rules</h2>
      <span class="count-pill">{rules.length}</span>
    </div>
    <button class="primary btn-new" on:click={newDraft} disabled={!!editing}>
      <span class="plus">+</span> New rule
    </button>
  </div>

  {#if issuesCount > 0}
    <div class="banner-warn">
      <span class="banner-icon">⚠</span>
      <div class="banner-body">
        <div class="banner-title">{issuesCount} rule{issuesCount > 1 ? "s" : ""} need{issuesCount > 1 ? "" : "s"} your attention</div>
        <div class="banner-sub">A referenced master or slave account no longer matches the role expected by the rule. Reassign or delete the affected rules below.</div>
      </div>
    </div>
  {/if}

  {#if rules.length === 0}
    <div class="empty-state">
      <div class="empty-glyph">⇄</div>
      <h3 class="empty-title">No copy rules yet</h3>
      <p class="empty-sub">
        Mark one account as <b>Master</b> and another as <b>Slave</b> in the Accounts tab,
        then create a rule here to start mirroring trades.
      </p>
      <button class="primary" on:click={newDraft}>+ Create your first rule</button>
    </div>
  {:else}
    <div class="rule-list">
      {#each rules as r (r.id)}
        {@const meta = ruleMeta.map.get(r.id)}
        {@const chips = meta?.chips ?? []}
        {@const issue = meta?.issue ?? null}
        <div class="rule" class:off={!r.enabled} class:warn={!!issue}>
          <div class="rule-status-bar" class:on={r.enabled} class:warn={!!issue}></div>

          <div class="rule-main">
            <div class="rule-name-row">
              <h4 class="rule-name" class:untitled={!r.name?.trim()}>
                {r.name?.trim() || "Untitled rule"}
              </h4>
              {#if issue}
                <span class="warn-pill" title={issue}>⚠ {issue}</span>
              {/if}
            </div>

            <div class="rule-flow">
              <div class="acc-block">
                <span class="role-tag master">MASTER</span>
                <div class="acc-line">
                  <span class="chip platform {platformOf(idx, r.master_id)}">{platformOf(idx, r.master_id)}</span>
                  <span class="acc-label">{labelOf(idx, r.master_id)}</span>
                </div>
              </div>

              <div class="flow-arrow" aria-hidden="true">
                <span class="arrow-line"></span>
                <span class="arrow-head">▶</span>
              </div>

              <div class="acc-block">
                <span class="role-tag slave">SLAVE</span>
                <div class="acc-line">
                  <span class="chip platform {platformOf(idx, r.slave_id)}">{platformOf(idx, r.slave_id)}</span>
                  <span class="acc-label">{labelOf(idx, r.slave_id)}</span>
                </div>
              </div>
            </div>

            <div class="rule-chips">
              {#each chips as c}
                <span class="cfg-chip {c.kind}">{c.text}</span>
              {/each}
            </div>
          </div>

          <div class="rule-actions">
            <button class="toggle" class:on={r.enabled}
                    title={r.enabled ? "Pause copying" : "Resume copying"}
                    on:click={() => toggle(r)}>
              <span class="toggle-track"><span class="toggle-thumb"></span></span>
              <span class="toggle-label">{r.enabled ? "Active" : "Paused"}</span>
            </button>
            <button class="icon-btn" title="Edit rule" on:click={() => editRule(r)}>✎</button>
            <button class="icon-btn danger" title="Delete rule" on:click={() => remove(r)}>✕</button>
          </div>
        </div>
      {/each}
    </div>
  {/if}
</div>

{#if editing}
  {@const e = editing}
  {@const isEdit = rules.some((r) => r.id === e.id)}
  <div class="overlay" on:click={cancel} transition:fade={{ duration: 120 }}></div>
  <aside class="drawer" transition:fly={{ x: 480, duration: 220 }}>
    <header class="drawer-head">
      <div class="drawer-head-main">
        <div class="drawer-eyebrow">{isEdit ? "Editing rule" : "New rule"}</div>
        <input
          type="text"
          class="drawer-name"
          placeholder="Untitled rule"
          bind:value={editing.name}
        />
      </div>
      <button class="icon-btn close" on:click={cancel} title="Close">✕</button>
    </header>

    <div class="drawer-pair">
      <label class="pair-block">
        <span class="pair-label">Master</span>
        <select class="pair-select" bind:value={editing.master_id}>
          <option value="" disabled>Select master…</option>
          {#each masterOptions(editing.master_id) as a}
            <option value={a.id}>{a.label} ({a.platform}){a.role !== "Master" ? ` — ${a.role}` : ""}</option>
          {/each}
        </select>
      </label>
      <div class="pair-arrow">→</div>
      <label class="pair-block">
        <span class="pair-label">Slave</span>
        <select class="pair-select" bind:value={editing.slave_id}>
          <option value="" disabled>Select slave…</option>
          {#each slaveOptions(editing.slave_id) as a}
            <option value={a.id}>{a.label} ({a.platform}){a.role === "Master" ? " — Master" : ""}</option>
          {/each}
        </select>
      </label>
      <label class="pair-toggle" class:on={editing.enabled}>
        <input type="checkbox" bind:checked={editing.enabled} />
        <span class="toggle-track"><span class="toggle-thumb"></span></span>
        <span>{editing.enabled ? "Active" : "Paused"}</span>
      </label>
    </div>

    <div class="drawer-body">
      <nav class="vtabs">
        {#each TABS as t}
          <button class="vtab" class:on={activeTab === t.id} on:click={() => (activeTab = t.id)}>
            <span class="vtab-icon">{t.icon}</span>
            <span class="vtab-text">
              <span class="vtab-label">{t.label}</span>
              <span class="vtab-desc">{t.desc}</span>
            </span>
          </button>
        {/each}
      </nav>

      <section class="vbody">
        {#if activeTab === "lot"}
          <header class="sec-head">
            <h3 class="section-title">Lot sizing</h3>
            <p class="section-sub">Choose how the slave's volume is derived from the master's.</p>
          </header>

          <div class="radio-grid">
            {#each LOT_MODES as m}
              <label class="radio-card" class:on={editing.lot_mode === m.id}>
                <input type="radio" bind:group={editing.lot_mode} value={m.id} />
                <span class="rc-icon">{m.icon}</span>
                <span class="rc-text">
                  <span class="rc-title">{m.label}</span>
                  <span class="rc-hint">{m.hint}</span>
                </span>
              </label>
            {/each}
          </div>

          <div class="form-grid mt">
            <div class="field">
              <label class="f-label" for="lot-value">
                {editing.lot_mode === "RiskPercent" ? "Risk per trade" : "Lot value"}
              </label>
              <div class="input-suffix">
                <input id="lot-value" type="number" step="0.01" min="0" bind:value={editing.lot_value} />
                <span class="suffix">{editing.lot_mode === "RiskPercent" ? "%" : editing.lot_mode === "Fixed" ? "lots" : "×"}</span>
              </div>
              <p class="f-help">
                {editing.lot_mode === "Fixed" ? "Always open exactly this many lots."
                  : editing.lot_mode === "RiskPercent" ? "% of slave equity risked per trade."
                  : "Scaling factor applied on top of the chosen mode."}
              </p>
            </div>
            {#if editing.lot_mode === "RiskPercent"}
              <div class="field">
                <label class="f-label" for="pip-value">Pip value per lot</label>
                <div class="input-suffix">
                  <input id="pip-value" type="number" step="0.01" min="0.01" bind:value={editing.pip_value_per_lot} />
                  <span class="suffix">$/pip</span>
                </div>
                <p class="f-help">Used to translate risk % into a lot size.</p>
              </div>
            {/if}
            <div class="field">
              <label class="f-label" for="min-lot">Minimum lot</label>
              <div class="input-suffix">
                <input id="min-lot" type="number" step="0.01" min="0" bind:value={editing.min_lot} />
                <span class="suffix">lots</span>
              </div>
              <p class="f-help">0 = no floor.</p>
            </div>
            <div class="field">
              <label class="f-label" for="max-lot">Maximum lot</label>
              <div class="input-suffix">
                <input id="max-lot" type="number" step="0.01" min="0" bind:value={editing.max_lot} />
                <span class="suffix">lots</span>
              </div>
              <p class="f-help">0 = no cap.</p>
            </div>
          </div>

          <label class="check-row mt">
            <input type="checkbox" bind:checked={editing.reverse} />
            <span class="check-text">
              <strong>Reverse direction</strong>
              <span class="muted">Mirror Buy ↔ Sell on the slave.</span>
            </span>
          </label>

        {:else if activeTab === "filters"}
          <header class="sec-head">
            <h3 class="section-title">Filters</h3>
            <p class="section-sub">Decide which master trades reach the slave.</p>
          </header>

          <div class="form-grid">
            <div class="field">
              <label class="f-label" for="direction">Direction</label>
              <select id="direction" bind:value={editing.direction}>
                <option value="All">Both buy &amp; sell</option>
                <option value="BuyOnly">Buy only</option>
                <option value="SellOnly">Sell only</option>
              </select>
            </div>
            <div class="field">
              <label class="f-label" for="comment">Comment filter</label>
              <input id="comment" type="text" placeholder="e.g. Scalper#1" bind:value={editing.comment_filter} />
              <p class="f-help">Case-insensitive substring match. Empty = no filter.</p>
            </div>
          </div>

          <h4 class="sub-section">Symbol matching</h4>
          <div class="form-grid">
            <div class="field full">
              <label class="f-label" for="wl">Whitelist</label>
              <input id="wl" type="text" placeholder="EUR, XAU, GER40"
                value={csvBind(editing.symbol_whitelist)}
                on:input={(ev) => editing && (editing.symbol_whitelist = fromCsv(ev.currentTarget.value))} />
              <p class="f-help">Comma-separated substrings. Empty = allow everything.</p>
            </div>
            <div class="field full">
              <label class="f-label" for="bl">Blacklist</label>
              <input id="bl" type="text" placeholder="USDJPY, BTC"
                value={csvBind(editing.symbol_blacklist)}
                on:input={(ev) => editing && (editing.symbol_blacklist = fromCsv(ev.currentTarget.value))} />
              <p class="f-help">Trades matching any of these are skipped.</p>
            </div>
            <div class="field">
              <label class="f-label" for="prefix">Symbol prefix</label>
              <input id="prefix" type="text" placeholder="(none)" bind:value={editing.symbol_prefix} />
            </div>
            <div class="field">
              <label class="f-label" for="suffix">Symbol suffix</label>
              <input id="suffix" type="text" placeholder=".r" bind:value={editing.symbol_suffix} />
            </div>
          </div>
          <div class="hint-box">
            <span class="hint-icon">→</span>
            <span>
              Master <code>EURUSD</code> → slave
              <code>{editing.symbol_prefix || ""}EURUSD{editing.symbol_suffix || ""}</code>
              (exact mappings below take precedence).
            </span>
          </div>

          <h4 class="sub-section">Symbol overrides</h4>
          <p class="section-sub">Exact master → slave mapping. Wins over prefix/suffix when the master symbol matches.</p>
          {#if mapPairs.length === 0}
            <p class="f-help" style="margin: 6px 0 10px;">No overrides. Add one if the slave broker uses a different ticker (e.g. <code>XAUUSD</code> → <code>GOLD.r</code>).</p>
          {:else}
            <div class="map-list">
              {#each mapPairs as pair, i (i)}
                <div class="map-row">
                  <input type="text" placeholder="MASTER (e.g. XAUUSD)"
                    value={pair[0]}
                    on:input={(ev) => updateMapping(i, 0, ev.currentTarget.value)} />
                  <span class="map-arrow">→</span>
                  <input type="text" placeholder="SLAVE (e.g. GOLD.r)"
                    value={pair[1]}
                    on:input={(ev) => updateMapping(i, 1, ev.currentTarget.value)} />
                  <button type="button" class="map-remove" title="Remove" on:click={() => removeMapping(i)}>✕</button>
                </div>
              {/each}
            </div>
          {/if}
          <button type="button" class="map-add" on:click={addMapping}>+ Add mapping</button>

        {:else if activeTab === "risk"}
          <header class="sec-head">
            <h3 class="section-title">Risk caps</h3>
            <p class="section-sub">Hard limits evaluated <strong>before</strong> dispatching to the slave.</p>
          </header>

          <div class="form-grid">
            <div class="field">
              <label class="f-label" for="max-pos">Max open positions</label>
              <div class="input-suffix">
                <input id="max-pos" type="number" min="0" step="1" bind:value={editing.max_open_positions} />
                <span class="suffix">pos</span>
              </div>
              <p class="f-help">0 = unlimited.</p>
            </div>
            <div class="field">
              <label class="f-label" for="max-exp">Max total exposure</label>
              <div class="input-suffix">
                <input id="max-exp" type="number" min="0" step="0.01" bind:value={editing.max_exposure_lots} />
                <span class="suffix">lots</span>
              </div>
              <p class="f-help">Sum of open volume on the slave.</p>
            </div>
            <div class="field">
              <label class="f-label" for="max-loss">Max daily loss</label>
              <div class="input-suffix">
                <input id="max-loss" type="number" min="0" step="1" bind:value={editing.max_daily_loss} />
                <span class="suffix">ccy</span>
              </div>
              <p class="f-help">Stops new copies past this. 0 = off.</p>
            </div>
            <div class="field">
              <label class="f-label" for="max-age">Skip trades older than</label>
              <div class="input-suffix">
                <input id="max-age" type="number" min="0" step="1" bind:value={editing.skip_older_than_secs} />
                <span class="suffix">s</span>
              </div>
              <p class="f-help">Avoid copying stale fills. 0 = off.</p>
            </div>
          </div>
          <div class="hint-box">
            <span class="hint-icon">ℹ</span>
            <span>Daily-loss aggregates closed trades since 00:00 UTC for the slave account.</span>
          </div>

        {:else if activeTab === "orders"}
          <header class="sec-head">
            <h3 class="section-title">Order shaping</h3>
            <p class="section-sub">SL/TP behaviour, slippage tolerance, and copy delay.</p>
          </header>

          <h4 class="sub-section">Stop-loss</h4>
          <div class="form-grid">
            <div class="field">
              <label class="f-label" for="sl-mode">Mode</label>
              <select id="sl-mode" bind:value={editing.sl_mode}>
                <option value="Copy">Copy from master</option>
                <option value="Ignore">Ignore (no SL)</option>
                <option value="Fixed">Fixed distance</option>
              </select>
            </div>
            {#if editing.sl_mode === "Fixed"}
              <div class="field">
                <label class="f-label" for="sl-pips">SL distance</label>
                <div class="input-suffix">
                  <input id="sl-pips" type="number" min="0" step="0.1" bind:value={editing.sl_pips} />
                  <span class="suffix">pips</span>
                </div>
              </div>
            {/if}
          </div>

          <h4 class="sub-section">Take-profit</h4>
          <div class="form-grid">
            <div class="field">
              <label class="f-label" for="tp-mode">Mode</label>
              <select id="tp-mode" bind:value={editing.tp_mode}>
                <option value="Copy">Copy from master</option>
                <option value="Ignore">Ignore (no TP)</option>
                <option value="Fixed">Fixed distance</option>
              </select>
            </div>
            {#if editing.tp_mode === "Fixed"}
              <div class="field">
                <label class="f-label" for="tp-pips">TP distance</label>
                <div class="input-suffix">
                  <input id="tp-pips" type="number" min="0" step="0.1" bind:value={editing.tp_pips} />
                  <span class="suffix">pips</span>
                </div>
              </div>
            {/if}
          </div>

          <h4 class="sub-section">Execution</h4>
          <div class="form-grid">
            <div class="field">
              <label class="f-label" for="slip">Max slippage</label>
              <div class="input-suffix">
                <input id="slip" type="number" min="0" step="1" bind:value={editing.max_slippage_pips} />
                <span class="suffix">pips</span>
              </div>
            </div>
            <div class="field">
              <label class="f-label" for="delay">Trade delay</label>
              <div class="input-suffix">
                <input id="delay" type="number" min="0" step="50" bind:value={editing.trade_delay_ms} />
                <span class="suffix">ms</span>
              </div>
              <p class="f-help">Wait before dispatching to the slave.</p>
            </div>
          </div>

          <h4 class="sub-section">Quote-diff compensation</h4>
          <p class="f-help" style="margin: 0 0 10px;">
            Shift SL/TP for specific symbols by a fixed pip offset, so the slave's
            stop sits at the expected price even when the slave broker quotes drift.
          </p>
          {#if editing.quote_offsets.length === 0}
            <p class="f-help" style="margin: 0 0 10px;">No offsets — add one to compensate a symbol.</p>
          {:else}
            <div class="qo-list">
              {#each editing.quote_offsets as o, i}
                <div class="qo-row">
                  <input type="text" class="qo-sym" placeholder="EURUSD"
                         value={o.symbol}
                         on:input={(e) => { editing.quote_offsets[i].symbol = e.currentTarget.value.toUpperCase(); editing.quote_offsets = editing.quote_offsets; }} />
                  <div class="input-suffix qo-pips">
                    <input type="number" step="0.1" placeholder="0.0"
                           bind:value={editing.quote_offsets[i].pips} />
                    <span class="suffix">pips</span>
                  </div>
                  <button type="button" class="icon-btn danger" title="Remove"
                          on:click={() => editing.quote_offsets = editing.quote_offsets.filter((_, j) => j !== i)}>✕</button>
                </div>
              {/each}
            </div>
          {/if}
          <button type="button" class="ghost mt"
                  on:click={() => editing.quote_offsets = [...editing.quote_offsets, { symbol: "", pips: 0 }]}>
            + Add symbol offset
          </button>

        {:else if activeTab === "schedule"}
          <header class="sec-head">
            <h3 class="section-title">Schedule</h3>
            <p class="section-sub">Restrict copying to a daily time window (UTC).</p>
          </header>

          <label class="check-row">
            <input type="checkbox" bind:checked={editing.schedule.enabled} />
            <span class="check-text">
              <strong>Enable schedule</strong>
              <span class="muted">Outside this window, master trades are skipped.</span>
            </span>
          </label>

          <div class="form-grid mt" class:dim={!editing.schedule.enabled}>
            <div class="field">
              <label class="f-label" for="start">Start time (UTC)</label>
              <input id="start" type="time"
                value={minToHHMM(editing.schedule.start_min)}
                on:input={(ev) => editing && (editing.schedule.start_min = hhmmToMin(ev.currentTarget.value))}
                disabled={!editing.schedule.enabled} />
            </div>
            <div class="field">
              <label class="f-label" for="end">End time (UTC)</label>
              <input id="end" type="time"
                value={minToHHMM(editing.schedule.end_min)}
                on:input={(ev) => editing && (editing.schedule.end_min = hhmmToMin(ev.currentTarget.value))}
                disabled={!editing.schedule.enabled} />
            </div>
          </div>

          <label class="check-row mt">
            <input type="checkbox" bind:checked={editing.schedule.skip_weekends} />
            <span class="check-text">
              <strong>Skip weekends</strong>
              <span class="muted">No copy on Sat/Sun (UTC).</span>
            </span>
          </label>

          <div class="hint-box">
            <span class="hint-icon">ℹ</span>
            <span>If <em>End</em> is before <em>Start</em>, the window wraps overnight (e.g. 22:00 → 06:00).</span>
          </div>

        {:else if activeTab === "advanced"}
          <header class="sec-head">
            <h3 class="section-title">Advanced</h3>
            <p class="section-sub">Position management features that need a live price feed.</p>
          </header>

          <div class="form-grid">
            <div class="field">
              <label class="f-label" for="trail">Trailing stop</label>
              <div class="input-suffix">
                <input id="trail" type="number" min="0" step="0.1" bind:value={editing.trailing_pips} />
                <span class="suffix">pips</span>
              </div>
              <p class="f-help">0 = disabled.</p>
            </div>
            <div class="field">
              <label class="f-label" for="be">Break-even after</label>
              <div class="input-suffix">
                <input id="be" type="number" min="0" step="0.1" bind:value={editing.breakeven_after_pips} />
                <span class="suffix">pips</span>
              </div>
              <p class="f-help">Move SL to entry once profit reaches this.</p>
            </div>
          </div>
          <div class="hint-box warn">
            <span class="hint-icon">⚠</span>
            <span>
              Trailing &amp; break-even are stored on the rule but <strong>not yet executed</strong> by the engine —
              they require slave-side price ticks. To be wired up later.
            </span>
          </div>
        {/if}
      </section>
    </div>

    <footer class="drawer-foot">
      <div class="foot-preview">
        {#each chipsForRule(editing) as c}
          <span class="cfg-chip {c.kind}">{c.text}</span>
        {/each}
      </div>
      <div class="foot-actions">
        <button on:click={cancel}>Cancel</button>
        <button class="primary" on:click={save} disabled={!editing.master_id || !editing.slave_id}>
          {isEdit ? "Save changes" : "Create rule"}
        </button>
      </div>
    </footer>
  </aside>
{/if}

<style>
  .rules-root { overflow: hidden; }
  .header-left { display: flex; align-items: center; gap: 10px; }
  .count-pill {
    display: inline-flex; align-items: center; justify-content: center;
    min-width: 22px; height: 20px; padding: 0 7px;
    border-radius: 999px;
    background: var(--surface-muted); color: var(--text-2);
    font-size: 11px; font-weight: 600;
  }
  .btn-new { display: inline-flex; align-items: center; gap: 6px; }
  .btn-new .plus { font-size: 16px; line-height: 1; margin-top: -1px; }

  /* Empty state */
  .empty-state {
    padding: 56px 24px 64px;
    text-align: center;
    color: var(--text-2);
  }
  .empty-glyph {
    font-size: 36px; line-height: 1;
    width: 64px; height: 64px;
    border-radius: 16px;
    background: var(--primary-soft); color: var(--primary);
    display: inline-flex; align-items: center; justify-content: center;
    margin-bottom: 16px;
  }
  .empty-title { font-size: 15px; margin-bottom: 6px; color: var(--text); }
  .empty-sub { font-size: 13px; margin: 0 auto 18px; max-width: 380px; line-height: 1.55; }

  /* Rule cards */
  .rule-list { display: flex; flex-direction: column; }
  .rule {
    position: relative;
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 16px;
    align-items: center;
    padding: 16px 20px 16px 24px;
    border-bottom: 1px solid #f1f5f9;
    transition: background 0.12s;
  }
  .rule:last-child { border-bottom: none; }
  .rule:hover { background: #fafbfc; }
  .rule.off { opacity: 0.7; }
  .rule.off .acc-label { color: var(--text-2); }

  .rule-status-bar {
    position: absolute; left: 0; top: 0; bottom: 0; width: 3px;
    background: #cbd5e1;
  }
  .rule-status-bar.on { background: var(--success); }
  .rule-status-bar.warn { background: #f59e0b; }
  .rule.warn { border-color: #fcd34d; background: #fffbeb; }
  .banner-warn {
    display: flex; align-items: flex-start; gap: 12px;
    margin: 12px 16px 0;
    padding: 12px 14px;
    background: #fffbeb; border: 1px solid #fcd34d;
    border-radius: var(--radius);
  }
  .banner-icon { font-size: 18px; line-height: 1; color: #b45309; }
  .banner-title { font-weight: 600; color: #92400e; font-size: 13px; }
  .banner-sub { color: #78350f; font-size: 12px; margin-top: 2px; }
  .warn-pill {
    display: inline-flex; align-items: center; gap: 4px;
    padding: 2px 8px; border-radius: 999px;
    background: #fef3c7; color: #92400e;
    font-size: 11px; font-weight: 600;
    border: 1px solid #fcd34d;
  }

  .rule-main { min-width: 0; display: flex; flex-direction: column; gap: 10px; }

  .rule-name-row { display: flex; align-items: center; gap: 8px; }
  .rule-name {
    margin: 0;
    font-size: 14px; font-weight: 600;
    color: var(--text); letter-spacing: -0.005em;
    line-height: 1.3;
    white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
  }
  .rule-name.untitled { color: var(--text-muted); font-style: italic; font-weight: 500; }

  .rule-flow {
    display: flex; align-items: center; gap: 14px;
    flex-wrap: wrap;
  }
  .acc-block { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
  .role-tag {
    font-size: 9px; font-weight: 700; letter-spacing: 0.1em;
    color: var(--text-muted);
  }
  .role-tag.master { color: #4338ca; }
  .role-tag.slave { color: #047857; }
  .acc-line { display: inline-flex; align-items: center; gap: 8px; min-width: 0; }
  .acc-label { font-weight: 600; color: var(--text); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; max-width: 200px; }

  .flow-arrow {
    display: inline-flex; align-items: center; gap: 0;
    color: var(--text-muted);
    margin-bottom: -6px;
  }
  .arrow-line {
    display: inline-block; width: 28px; height: 1.5px;
    background: linear-gradient(to right, transparent, var(--border-strong));
  }
  .arrow-head { font-size: 9px; transform: translateX(-2px); color: var(--border-strong); }

  .rule-chips { display: flex; flex-wrap: wrap; gap: 5px; }
  .cfg-chip {
    display: inline-flex; align-items: center;
    padding: 2px 8px;
    border-radius: 5px;
    font-size: 11px; font-weight: 500;
    background: var(--surface-muted); color: var(--text-2);
    border: 1px solid transparent;
  }
  .cfg-chip.primary { background: var(--primary-soft); color: var(--primary); }
  .cfg-chip.info    { background: #f1f5f9; color: #475569; }
  .cfg-chip.warn    { background: #fffbeb; color: #b45309; }
  .cfg-chip.danger  { background: #fef2f2; color: #b91c1c; }

  .rule-actions { display: inline-flex; align-items: center; gap: 6px; }

  .icon-btn {
    width: 32px; height: 32px; padding: 0;
    display: inline-flex; align-items: center; justify-content: center;
    border: 1px solid transparent;
    border-radius: 8px;
    background: transparent;
    color: var(--text-2);
    font-size: 13px;
    cursor: pointer;
  }
  .icon-btn:hover { background: var(--surface-muted); color: var(--text); }
  .icon-btn.danger:hover { background: #fef2f2; color: var(--danger); }
  .icon-btn.close { color: var(--text-2); }

  /* Toggle */
  .toggle {
    display: inline-flex; align-items: center; gap: 8px;
    padding: 4px 10px 4px 4px;
    border: 1px solid var(--border);
    border-radius: 999px;
    background: #fff;
    font-size: 12px; font-weight: 600;
    color: #64748b;
    cursor: pointer;
  }
  .toggle-track {
    position: relative; display: inline-block;
    width: 28px; height: 16px;
    background: #cbd5e1; border-radius: 999px;
    transition: background 0.15s ease;
  }
  .toggle-thumb {
    position: absolute; top: 2px; left: 2px;
    width: 12px; height: 12px;
    background: #fff; border-radius: 50%;
    box-shadow: 0 1px 2px rgba(0, 0, 0, 0.2);
    transition: left 0.15s ease;
  }
  .toggle.on { border-color: #10b981; background: #ecfdf5; color: #047857; }
  .toggle.on .toggle-track { background: #10b981; }
  .toggle.on .toggle-thumb { left: 14px; }

  /* Drawer */
  .overlay {
    position: fixed; inset: 0; z-index: 40;
    background: rgba(15, 23, 42, 0.32);
    backdrop-filter: blur(2px);
  }
  .drawer {
    position: fixed; top: 0; right: 0; bottom: 0;
    width: min(760px, 100vw);
    z-index: 50;
    background: var(--surface);
    border-left: 1px solid var(--border);
    box-shadow: -10px 0 30px rgba(15, 23, 42, 0.12);
    display: flex; flex-direction: column;
  }
  .drawer-head {
    display: flex; align-items: flex-start; justify-content: space-between;
    padding: 18px 28px 14px;
    border-bottom: 1px solid var(--border);
  }
  .drawer-eyebrow {
    font-size: 10px; font-weight: 700; letter-spacing: 0.1em;
    color: var(--primary); text-transform: uppercase;
    margin-bottom: 4px;
  }
  .drawer-head-main { flex: 1; min-width: 0; }
  .drawer-name {
    width: 100%;
    margin-top: 2px;
    padding: 4px 8px; margin-left: -8px;
    font-family: inherit;
    font-size: 18px; font-weight: 600;
    color: var(--text); letter-spacing: -0.01em;
    background: transparent; border: 1px solid transparent;
    border-radius: 6px;
    transition: background 0.12s, border-color 0.12s;
    height: auto;
  }
  .drawer-name:hover { background: var(--surface-muted); }
  .drawer-name:focus {
    background: #fff;
    border-color: var(--primary);
    box-shadow: 0 0 0 3px rgba(37, 99, 235, 0.12);
    outline: none;
  }
  .drawer-name::placeholder { color: var(--text-muted); font-weight: 500; }

  .drawer-pair {
    display: grid;
    grid-template-columns: 1fr auto 1fr auto;
    align-items: end; gap: 12px;
    padding: 14px 28px;
    background: #fafbfc;
    border-bottom: 1px solid var(--border);
  }
  .pair-block { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
  .pair-label { font-size: 10px; font-weight: 700; letter-spacing: 0.1em; color: var(--text-muted); text-transform: uppercase; }
  .pair-select { width: 100%; }
  .pair-arrow { color: var(--text-muted); padding-bottom: 9px; font-size: 14px; }
  .pair-toggle {
    display: inline-flex; align-items: center; gap: 8px;
    padding: 7px 12px 7px 6px;
    border: 1px solid var(--border);
    border-radius: 999px; background: #fff;
    font-size: 12px; font-weight: 600;
    color: #64748b; cursor: pointer;
    align-self: end;
  }
  .pair-toggle.on { border-color: var(--success); background: #ecfdf5; color: #047857; }
  .pair-toggle.on .toggle-track { background: var(--success); }
  .pair-toggle.on .toggle-thumb { left: 14px; }
  .pair-toggle input { display: none; }

  .drawer-body {
    flex: 1; min-height: 0;
    display: grid;
    grid-template-columns: 200px 1fr;
  }

  /* Vertical tabs */
  .vtabs {
    border-right: 1px solid var(--border);
    background: #fafbfc;
    padding: 12px 8px;
    overflow-y: auto;
  }
  .vtab {
    display: flex; align-items: flex-start; gap: 10px;
    width: 100%; text-align: left;
    padding: 9px 10px;
    border: 1px solid transparent;
    border-radius: 8px; background: transparent;
    color: var(--text-2);
    margin-bottom: 2px; cursor: pointer;
    transition: background 0.12s, color 0.12s, border-color 0.12s;
  }
  .vtab:hover { background: #fff; color: var(--text); }
  .vtab.on {
    background: #fff; color: var(--primary);
    border-color: var(--border); box-shadow: var(--shadow-sm);
  }
  .vtab-icon {
    width: 24px; height: 24px;
    display: inline-flex; align-items: center; justify-content: center;
    border-radius: 6px;
    background: var(--surface-muted);
    font-size: 12px; flex: none;
  }
  .vtab.on .vtab-icon { background: var(--primary-soft); color: var(--primary); }
  .vtab-text { display: flex; flex-direction: column; min-width: 0; }
  .vtab-label { font-size: 13px; font-weight: 600; }
  .vtab-desc  { font-size: 11px; color: var(--text-muted); font-weight: 400; line-height: 1.3; }
  .vtab.on .vtab-desc { color: var(--text-2); }

  /* Tab body */
  .vbody { padding: 24px 32px 32px; overflow-y: auto; }
  .sec-head { margin-bottom: 20px; }
  .section-title {
    font-size: 15px; color: var(--text); font-weight: 600;
    text-transform: none; letter-spacing: -0.005em;
    margin: 0 0 4px;
  }
  .section-sub { font-size: 12.5px; color: var(--text-2); margin: 0; line-height: 1.5; }
  .sub-section {
    font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.07em;
    color: var(--text-muted); margin: 24px 0 10px;
    padding-bottom: 6px;
    border-bottom: 1px solid #f1f5f9;
  }

  /* Form grid — predictable 2-col, collapses to 1 below ~480px container */
  .form-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: 16px 18px;
    align-items: start;
  }
  .form-grid.mt { margin-top: 14px; }
  .mt { margin-top: 10px; }
  .qo-list { display: flex; flex-direction: column; gap: 6px; margin-bottom: 8px; }
  .qo-row { display: grid; grid-template-columns: minmax(0, 1fr) 130px 32px; gap: 8px; align-items: center; }
  .qo-row > * { min-width: 0; }
  .qo-sym { text-transform: uppercase; width: 100%; }
  .qo-pips { width: 100%; }
  .qo-pips input { width: 100%; }
  .qo-row .icon-btn { width: 32px; height: 32px; padding: 0; }
  .ghost {
    display: inline-flex; align-items: center; gap: 4px;
    padding: 6px 10px;
    background: transparent;
    border: 1px dashed var(--border);
    border-radius: var(--radius-sm);
    color: var(--text-2);
    font-size: 12px; cursor: pointer;
  }
  .ghost:hover { background: var(--surface-muted); color: var(--text); border-style: solid; }
  .form-grid.dim { opacity: 0.55; pointer-events: none; }

  /* Field: label / control / helper, all left-aligned */
  .field {
    min-width: 0;
    display: flex; flex-direction: column;
    gap: 6px;
  }
  .field.full { grid-column: 1 / -1; }

  .f-label {
    font-size: 12.5px; font-weight: 500;
    color: var(--text);
    line-height: 1.3;
    text-transform: none; letter-spacing: 0;
  }
  .f-help {
    margin: 0;
    font-size: 11.5px; color: var(--text-muted);
    line-height: 1.4;
  }

  .field input,
  .field select {
    width: 100%; min-width: 0;
    height: 36px;
    padding: 0 12px;
    box-sizing: border-box;
    font-size: 13px;
  }
  .field input[type="time"] { padding: 0 10px; }
  .field input[type="number"] { -moz-appearance: textfield; appearance: textfield; }
  .field input[type="number"]::-webkit-outer-spin-button,
  .field input[type="number"]::-webkit-inner-spin-button {
    -webkit-appearance: none; margin: 0;
  }

  /* Input with unit suffix */
  .input-suffix {
    position: relative;
    display: flex; align-items: stretch;
  }
  .input-suffix input {
    padding-right: 56px;
    font-variant-numeric: tabular-nums;
  }
  .input-suffix .suffix {
    position: absolute; right: 0; top: 0; bottom: 0;
    display: inline-flex; align-items: center; justify-content: center;
    padding: 0 12px;
    border-left: 1px solid var(--border);
    background: #fafbfc;
    color: var(--text-muted);
    font-size: 11px; font-weight: 600;
    letter-spacing: 0.02em;
    pointer-events: none;
    border-radius: 0 var(--radius) var(--radius) 0;
    min-width: 44px;
  }
  .input-suffix input:focus + .suffix {
    border-left-color: var(--primary);
    color: var(--primary);
  }

  /* Radio cards (lot mode) */
  .radio-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
    gap: 8px;
  }
  .radio-card {
    display: flex; align-items: center; gap: 10px;
    padding: 12px;
    border: 1.5px solid var(--border); border-radius: 10px;
    background: #fff; cursor: pointer;
    transition: border-color 0.12s, background 0.12s, transform 0.08s;
  }
  .radio-card:hover { border-color: var(--border-strong); }
  .radio-card.on {
    border-color: var(--primary);
    background: var(--primary-soft);
  }
  .radio-card input { display: none; }
  .rc-icon {
    width: 32px; height: 32px;
    display: inline-flex; align-items: center; justify-content: center;
    border-radius: 8px;
    background: var(--surface-muted); color: var(--text-2);
    font-size: 14px; font-weight: 700;
    flex: none;
  }
  .radio-card.on .rc-icon { background: #fff; color: var(--primary); }
  .rc-text { display: flex; flex-direction: column; min-width: 0; }
  .rc-title { font-size: 13px; font-weight: 600; color: var(--text); }
  .rc-hint  { font-size: 11px; color: var(--text-muted); }

  /* Check rows */
  .check-row {
    display: flex; align-items: flex-start; gap: 12px;
    padding: 12px 14px;
    border: 1px solid var(--border); border-radius: 8px;
    background: #fff;
    cursor: pointer;
    font-size: 13px;
    transition: border-color 0.12s, background 0.12s;
  }
  .check-row:hover { border-color: var(--border-strong); background: #fafbfc; }
  .check-row.mt { margin-top: 14px; }
  .check-row input { width: 16px; height: 16px; flex: none; cursor: pointer; margin-top: 1px; }
  .check-text { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
  .check-text strong { font-weight: 600; color: var(--text); }
  .check-text .muted { font-weight: 400; font-size: 12px; line-height: 1.4; }

  /* Hint boxes */
  .hint-box {
    display: flex; align-items: flex-start; gap: 10px;
    margin-top: 14px;
    padding: 10px 12px;
    border-radius: 8px;
    background: #f1f5f9;
    border: 1px solid #e2e8f0;
    font-size: 12px; color: var(--text-2);
    line-height: 1.5;
  }
  .hint-box code { background: #fff; padding: 1px 6px; border-radius: 4px; font-size: 11px; }
  .hint-icon {
    width: 18px; height: 18px;
    display: inline-flex; align-items: center; justify-content: center;
    border-radius: 50%;
    background: #fff; color: var(--primary);
    font-size: 11px; font-weight: 700;
    flex: none;
  }
  .hint-box.warn { background: #fffbeb; border-color: #fde68a; color: #92400e; }
  .hint-box.warn .hint-icon { color: #b45309; }

  /* Symbol map editor */
  .map-list { display: flex; flex-direction: column; gap: 6px; margin: 8px 0 10px; }
  .map-row {
    display: grid;
    grid-template-columns: 1fr auto 1fr auto;
    align-items: center;
    gap: 8px;
  }
  .map-row input {
    font: inherit; font-size: 13px;
    padding: 7px 10px;
    border: 1px solid var(--border); border-radius: 6px;
    background: #fff; min-width: 0;
    font-variant-numeric: tabular-nums;
  }
  .map-row input:focus { outline: none; border-color: var(--primary); box-shadow: 0 0 0 3px var(--primary-soft); }
  .map-arrow { color: var(--text-muted); font-weight: 600; }
  .map-remove {
    width: 28px; height: 28px;
    border: 1px solid var(--border); border-radius: 6px;
    background: #fff; color: var(--text-muted);
    font-size: 12px; cursor: pointer;
  }
  .map-remove:hover { background: #fef2f2; border-color: #fecaca; color: #b91c1c; }
  .map-add {
    align-self: flex-start;
    padding: 6px 12px;
    border: 1px dashed var(--border); border-radius: 6px;
    background: transparent; color: var(--text-2);
    font-size: 12px; font-weight: 500; cursor: pointer;
  }
  .map-add:hover { border-color: var(--primary); color: var(--primary); background: var(--primary-soft); }

  /* Drawer footer */
  .drawer-foot {
    display: flex; align-items: center; justify-content: space-between;
    gap: 12px;
    padding: 14px 28px;
    border-top: 1px solid var(--border);
    background: #fff;
  }
  .foot-preview {
    display: flex; flex-wrap: wrap; gap: 4px;
    flex: 1; min-width: 0;
    max-height: 48px; overflow: hidden;
  }
  .foot-actions { display: flex; gap: 8px; flex: none; }

  @media (max-width: 860px) {
    .drawer-body { grid-template-columns: 168px 1fr; }
    .vtab-desc { display: none; }
  }
  @media (max-width: 680px) {
    .drawer-body { grid-template-columns: 1fr; }
    .vtabs {
      display: flex; gap: 4px; padding: 8px;
      overflow-x: auto;
      border-right: none; border-bottom: 1px solid var(--border);
    }
    .vtab { white-space: nowrap; margin-bottom: 0; flex: none; }
    .vtab-text { display: none; }
    .vbody { padding: 18px 18px 22px; }
    .drawer-head, .drawer-pair, .drawer-foot { padding-left: 18px; padding-right: 18px; }
    .drawer-pair {
      grid-template-columns: 1fr 1fr;
      row-gap: 10px;
    }
    .pair-arrow { display: none; }
    .pair-toggle { grid-column: 1 / -1; justify-content: center; }
  }
</style>
