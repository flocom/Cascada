<script lang="ts">
  import { createEventDispatcher } from "svelte";
  import { api, defaultRule, type Account, type Platform, type CopyRule } from "../lib/api";
  import { open as openDialog, ask } from "@tauri-apps/plugin-dialog";

  export let accounts: Account[];
  export let rules: CopyRule[] = [];
  const dispatch = createEventDispatcher();

  let showAdd = false;
  let mode: Platform = "cTrader";
  let installStatus: { kind: "info" | "ok" | "err"; text: string } | null = null;

  let editingId: string | null = null;
  let editingLabel = "";

  let dragId: string | null = null;
  let dragOver: string | null = null;   // master id being hovered
  const dragEnterCount = new Map<string, number>();

  function onDragStart(e: DragEvent, id: string) {
    dragId = id;
    e.dataTransfer?.setData("text/plain", id);
    if (e.dataTransfer) e.dataTransfer.effectAllowed = "move";
  }
  function onDragEnd() { dragId = null; dragOver = null; dragEnterCount.clear(); }
  function onDragEnter(e: DragEvent, masterId: string) {
    if (!dragId || dragId === masterId) return;
    e.preventDefault();
    dragEnterCount.set(masterId, (dragEnterCount.get(masterId) ?? 0) + 1);
    dragOver = masterId;
  }
  function onDragOver(e: DragEvent, masterId: string) {
    if (!dragId || dragId === masterId) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
    if (dragOver !== masterId) dragOver = masterId;
  }
  function onDragLeave(_e: DragEvent, masterId: string) {
    const n = (dragEnterCount.get(masterId) ?? 1) - 1;
    dragEnterCount.set(masterId, n);
    if (n <= 0 && dragOver === masterId) dragOver = null;
  }
  async function onDrop(e: DragEvent, master: Account) {
    e.preventDefault();
    e.stopPropagation();
    const id = (e.dataTransfer?.getData("text/plain") || dragId);
    dragOver = null; dragId = null; dragEnterCount.clear();
    if (!id || id === master.id) return;
    const acc = accountMap.get(id);
    if (!acc) return;
    if (acc.role === "Master") return;
    const stale = rules.filter((r) => r.slave_id === acc.id && r.master_id !== master.id);
    if (stale.length > 0) {
      const slaveLbl = acc.label || acc.login;
      const masterLbl = master.label || master.login;
      const ok = await ask(
        `Move "${slaveLbl}" under "${masterLbl}"?\n\n` +
        `${stale.length} existing rule(s) linking this slave to its previous master will be deleted.`,
        { title: "Reassign slave?", kind: "warning", okLabel: "Move & delete", cancelLabel: "Cancel" });
      if (!ok) return;
      for (const r of stale) await api.deleteRule(r.id);
    }
    await linkSlave(master, acc);
  }
  async function onDropOrphan(e: DragEvent) {
    e.preventDefault();
    e.stopPropagation();
    const id = (e.dataTransfer?.getData("text/plain") || dragId);
    dragOver = null; dragId = null; dragEnterCount.clear();
    if (!id) return;
    const acc = accountMap.get(id);
    if (!acc || acc.role === "Master") return;
    // Don't delete the rules — leave them in the Rules tab so the user can
    // reassign instead of losing config. Just demote the account.
    if (acc.role !== "Idle") await api.setRole(acc.id, "Idle");
    dispatch("refresh");
  }

  // Stable color per master (cycled palette).
  const PALETTE = [
    { tint: "#fef3c7", border: "#f59e0b", text: "#92400e" },  // amber
    { tint: "#dbeafe", border: "#3b82f6", text: "#1d4ed8" },  // blue
    { tint: "#dcfce7", border: "#10b981", text: "#047857" },  // emerald
    { tint: "#fce7f3", border: "#ec4899", text: "#be185d" },  // pink
    { tint: "#ede9fe", border: "#8b5cf6", text: "#6d28d9" },  // violet
    { tint: "#ffedd5", border: "#f97316", text: "#c2410c" },  // orange
  ];
  $: colorByMaster = new Map(masters.map((m, i) => [m.id, PALETTE[i % PALETTE.length]]));
  function colorOf(masterId: string) {
    return colorByMaster.get(masterId) ?? PALETTE[0];
  }

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
    if (mode === "cTrader") return;
    installStatus = { kind: "info", text: `Scanning ${mode} terminals…` };
    try {
      const paths = await api.installMtEa(mode);
      installStatus = { kind: "ok", text: `EA installed into ${paths.length} terminal${paths.length > 1 ? "s" : ""}. Refresh the Navigator panel in ${mode}, then drag CascadaBridge onto a chart.` };
    } catch (e) {
      installStatus = { kind: "err", text: `${e}` };
    }
  }
  async function installMtEaManual() {
    if (mode === "cTrader") return;
    const picked = await openDialog({
      directory: true,
      title: `Select the ${mode} data folder (contains MQL${mode === "MT4" ? "4" : "5"}/)`,
    });
    if (!picked || Array.isArray(picked)) return;
    installStatus = { kind: "info", text: "Installing EA…" };
    try {
      const p = await api.installMtEaAt(mode, picked);
      installStatus = { kind: "ok", text: `EA copied → ${p}. Refresh the Navigator panel in ${mode}.` };
    } catch (e) {
      installStatus = { kind: "err", text: `Failed: ${e}` };
    }
  }

  async function promote(a: Account) {
    await api.setRole(a.id, "Master");
    dispatch("refresh");
  }
  async function demoteToIdle(a: Account) {
    // Keep existing rules — they'll be flagged with a warning in the Rules tab
    // until the user reassigns or deletes them. Silent deletion here was making
    // rules vanish unexpectedly.
    await api.setRole(a.id, "Idle");
    dispatch("refresh");
  }

  async function linkSlave(master: Account, slave: Account) {
    // Reuse any existing rule for this (master, slave) pair so re-linking after
    // a demote doesn't create a duplicate. Otherwise create a fresh draft.
    const existing = rules.find((r) => r.master_id === master.id && r.slave_id === slave.id);
    await api.upsertRule(existing ?? defaultRule(master.id, slave.id));
    if (slave.role !== "Slave") await api.setRole(slave.id, "Slave");
    dispatch("refresh");
  }
  async function unlinkSlave(rule: CopyRule) {
    const master = accountMap.get(rule.master_id);
    const slave = accountMap.get(rule.slave_id);
    const masterLbl = master ? (master.label || master.login) : "master";
    const slaveLbl  = slave  ? (slave.label  || slave.login)  : "slave";
    const willIdle = slave && !rules.some((r) => r.slave_id === slave.id && r.id !== rule.id);
    const ok = await ask(
      `Unlink "${slaveLbl}" from "${masterLbl}"?\n\n` +
      `The copy rule will be deleted.` +
      (willIdle ? ` "${slaveLbl}" will be moved back to Idle.` : ""),
      { title: "Unlink slave?", kind: "warning", okLabel: "Unlink", cancelLabel: "Cancel" });
    if (!ok) return;
    await api.deleteRule(rule.id);
    if (willIdle && slave) await api.setRole(slave.id, "Idle");
    dispatch("refresh");
  }
  async function toggleRule(rule: CopyRule) {
    await api.upsertRule({ ...rule, enabled: !rule.enabled });
    dispatch("refresh");
  }

  async function removeAccount(a: Account) {
    await api.removeAccount(a.id);
    accounts = accounts.filter((x) => x.id !== a.id);
    dispatch("refresh");
  }

  function startEdit(a: Account) {
    editingId = a.id;
    editingLabel = a.label;
  }
  async function commitEdit(a: Account) {
    const next = editingLabel.trim();
    editingId = null;
    if (!next || next === a.label) return;
    await api.renameAccount(a.id, next);
    dispatch("refresh");
  }

  const platforms: { id: Platform; name: string; tag: string }[] = [
    { id: "cTrader", name: "cTrader", tag: "cBot · auto-discovered" },
    { id: "MT4",     name: "MetaTrader 4", tag: "EA · auto-discovered" },
    { id: "MT5",     name: "MetaTrader 5", tag: "EA · auto-discovered" },
  ];

  $: accountMap = new Map(accounts.map((a) => [a.id, a]));
  $: masters = accounts.filter((a) => a.role === "Master");
  // Only surface a slave under its master in the Accounts UI when its role is
  // still "Slave" — a demoted account stays in the unassigned column even if
  // its rule is preserved (the rule shows up with a warning in the Rules tab).
  // Single O(rules) pass instead of O(masters × rules); also builds the
  // `linkedByMaster` index reused by `candidatesFor` (no more nested `rules.some`).
  $: derived = (() => {
    const byMaster = new Map<string, { rule: CopyRule; slave: Account }[]>();
    const linked = new Map<string, Set<string>>();
    for (const m of masters) {
      byMaster.set(m.id, []);
      linked.set(m.id, new Set());
    }
    const slavesWithActiveMaster = new Set<string>();
    for (const r of rules) {
      const linkSet = linked.get(r.master_id);
      if (linkSet) linkSet.add(r.slave_id);
      const slave = accountMap.get(r.slave_id);
      const master = accountMap.get(r.master_id);
      if (!slave || !master || master.role !== "Master" || slave.role !== "Slave") continue;
      const bucket = byMaster.get(r.master_id);
      if (!bucket) continue;
      bucket.push({ rule: r, slave });
      slavesWithActiveMaster.add(slave.id);
    }
    return { byMaster, linked, slavesWithActiveMaster };
  })();
  $: slavesByMaster = derived.byMaster;
  $: unassigned = accounts.filter(
    (a) => a.role === "Idle" || (a.role === "Slave" && !derived.slavesWithActiveMaster.has(a.id)),
  );
  // Candidates for "Link slave" = anyone not yet linked to THIS master and not the master itself.
  function candidatesFor(masterId: string): Account[] {
    const linkSet = derived.linked.get(masterId);
    return accounts.filter(
      (a) => a.id !== masterId && a.role !== "Master" && !(linkSet && linkSet.has(a.id)),
    );
  }
</script>

<div class="card">
  <div class="card-header">
    <h2>Accounts</h2>
    <button class="primary" on:click={() => { showAdd = !showAdd; installStatus = null; }}>
      {showAdd ? "Close" : "+ Connect platform"}
    </button>
  </div>

  {#if showAdd}
    <div class="wizard">
      <div class="platforms">
        {#each platforms as p}
          <button class="plat-card" class:active={mode === p.id}
                  on:click={() => { mode = p.id; installStatus = null; }}>
            <span class="plat-badge {p.id}">{p.id}</span>
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
        {:else}
          <p class="lead">
            Install <code>CascadaBridge.{mode === "MT4" ? "mq4" : "mq5"}</code>, enable <strong>AutoTrading</strong>, and drag the EA onto any chart — your account appears here automatically. No network setup needed. Multiple {mode} terminals are supported in parallel.
          </p>
          <div class="install-row">
            <button class="primary" on:click={installMtEaAuto}>Auto-install Expert Advisor</button>
            <button on:click={installMtEaManual}>Pick location…</button>
          </div>
        {/if}
        {#if installStatus}
          <div class="inst-status {installStatus.kind}">{installStatus.text}</div>
        {/if}
      </div>
    </div>
  {/if}

  {#if accounts.length === 0}
    <div class="empty">
      <div class="empty-title">Waiting for platforms…</div>
      <div class="empty-body">Install the cBot or EA — accounts appear automatically once connected.</div>
    </div>
  {:else}
    <div class="tree">
      {#each masters as m (m.id)}
        {@const children = slavesByMaster.get(m.id) ?? []}
        {@const cands = candidatesFor(m.id)}
        {@const c = colorOf(m.id)}
        <div class="group" class:drop-active={dragOver === m.id}
             style:--mc-tint={c.tint} style:--mc-border={c.border} style:--mc-text={c.text}
             on:dragenter={(e) => onDragEnter(e, m.id)}
             on:dragover={(e) => onDragOver(e, m.id)}
             on:dragleave={(e) => onDragLeave(e, m.id)}
             on:drop={(e) => onDrop(e, m)}
             role="region">
          <div class="row master">
            <span class="role-badge master-badge">MASTER</span>
            <span class="chip platform {m.platform}">{m.platform}</span>
            <div class="label-col">
              {#if editingId === m.id}
                <input class="label-edit"
                  bind:value={editingLabel}
                  on:blur={() => commitEdit(m)}
                  on:keydown={(e) => { if (e.key === "Enter") (e.currentTarget).blur(); if (e.key === "Escape") editingId = null; }}
                  autofocus />
              {:else}
                <button class="label-btn" on:click={() => startEdit(m)} title="Rename">{m.label}</button>
              {/if}
              <span class="muted small">{m.login || "—"}</span>
            </div>
            <span class="status-pill" class:online={m.connected}>
              <span class="status-dot"></span>
              {m.connected ? "Online" : "Offline"}
            </span>
            <span class="num">{m.balance.toFixed(2)} <span class="muted">{m.currency}</span></span>
            <span class="num subtle">{m.equity.toFixed(2)}</span>
            <div class="row-actions">
              <button class="ghost" title="Demote to unassigned" on:click={() => demoteToIdle(m)}>Unassign</button>
              <button class="danger icon" title="Remove" on:click={() => removeAccount(m)}>✕</button>
            </div>
          </div>

          <div class="children">
            {#each children as { rule, slave } (rule.id)}
              <div class="row slave" class:off={!rule.enabled}
                   draggable="true"
                   on:dragstart={(e) => onDragStart(e, slave.id)}
                   on:dragend={onDragEnd}>
                <span class="tree-mark">↳</span>
                <span class="role-badge slave-badge">SLAVE</span>
                <span class="chip platform {slave.platform}">{slave.platform}</span>
                <div class="label-col">
                  {#if editingId === slave.id}
                    <input class="label-edit"
                      bind:value={editingLabel}
                      on:blur={() => commitEdit(slave)}
                      on:keydown={(e) => { if (e.key === "Enter") (e.currentTarget).blur(); if (e.key === "Escape") editingId = null; }}
                      autofocus />
                  {:else}
                    <button class="label-btn" on:click={() => startEdit(slave)} title="Rename">{slave.label}</button>
                  {/if}
                  <span class="muted small">{slave.login || "—"} · {rule.lot_mode} ×{rule.lot_value}{rule.reverse ? " · reverse" : ""}</span>
                </div>
                <span class="status-pill" class:online={slave.connected}>
                  <span class="status-dot"></span>
                  {slave.connected ? "Online" : "Offline"}
                </span>
                <span class="num">{slave.balance.toFixed(2)} <span class="muted">{slave.currency}</span></span>
                <span class="num subtle">{slave.equity.toFixed(2)}</span>
                <div class="row-actions">
                  <button class="toggle" class:on={rule.enabled} class:off={!rule.enabled}
                          title={rule.enabled ? "Click to pause copying" : "Click to resume copying"}
                          on:click={() => toggleRule(rule)}>
                    <span class="toggle-track"><span class="toggle-thumb"></span></span>
                    <span class="toggle-label">{rule.enabled ? "Active" : "Paused"}</span>
                  </button>
                  <button class="ghost icon" title="Unlink from master" on:click={() => unlinkSlave(rule)}>✕</button>
                </div>
              </div>
            {/each}

            {#if cands.length > 0}
              <div class="link-zone">
                <span class="muted small">Attach as slave →</span>
                {#each cands as c}
                  <button class="candidate" title="Link {c.label} as slave" on:click={() => linkSlave(m, c)}>
                    + <span class="chip platform {c.platform}">{c.platform}</span>
                    <span>{c.label}</span>
                  </button>
                {/each}
              </div>
            {:else if children.length === 0}
              <div class="link-zone subtle"><span class="muted small">No other accounts available — connect another platform first.</span></div>
            {/if}
          </div>
        </div>
      {/each}

      {#if unassigned.length > 0 || dragId}
        <div class="group orphans" class:drop-active={dragOver === '__orphan__'}
             on:dragenter={(e) => onDragEnter(e, '__orphan__')}
             on:dragover={(e) => onDragOver(e, '__orphan__')}
             on:dragleave={(e) => onDragLeave(e, '__orphan__')}
             on:drop={onDropOrphan}
             role="region">
          <div class="group-header">
            <span class="group-title">Unassigned</span>
            <span class="muted small">{dragId ? 'Drop here to detach from master.' : 'Drag onto a master, click "Make master", or pick a master to attach to.'}</span>
          </div>
          {#each unassigned as a (a.id)}
            <div class="row orphan"
                 draggable="true"
                 on:dragstart={(e) => onDragStart(e, a.id)}
                 on:dragend={onDragEnd}
                 class:dragging={dragId === a.id}>
              <span class="chip platform {a.platform}">{a.platform}</span>
              <div class="label-col">
                {#if editingId === a.id}
                  <input class="label-edit"
                    bind:value={editingLabel}
                    on:blur={() => commitEdit(a)}
                    on:keydown={(e) => { if (e.key === "Enter") (e.currentTarget).blur(); if (e.key === "Escape") editingId = null; }}
                    autofocus />
                {:else}
                  <button class="label-btn" on:click={() => startEdit(a)} title="Rename">{a.label}</button>
                {/if}
                <span class="muted small">{a.login || "—"}</span>
              </div>
              <span class="status-pill" class:online={a.connected}>
                <span class="status-dot"></span>
                {a.connected ? "Online" : "Offline"}
              </span>
              <span class="num">{a.balance.toFixed(2)} <span class="muted">{a.currency}</span></span>
              <span class="num subtle">{a.equity.toFixed(2)}</span>
              <div class="row-actions wrap">
                {#each masters as m}
                  <button class="candidate" title="Attach as slave to {m.label}" on:click={() => linkSlave(m, a)}>
                    → {m.label}
                  </button>
                {/each}
                <button class="primary" on:click={() => promote(a)}>Make master</button>
                <button class="danger icon" title="Remove" on:click={() => removeAccount(a)}>✕</button>
              </div>
            </div>
          {/each}
        </div>
      {/if}
    </div>
  {/if}
</div>

<style>
  .wizard {
    padding: 20px 22px 24px;
    border-bottom: 1px solid var(--border);
    background: linear-gradient(180deg, #fafbfc 0%, #ffffff 100%);
    display: flex; flex-direction: column; gap: 18px;
  }
  .platforms { display: grid; grid-template-columns: repeat(3, 1fr); gap: 10px; }
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

  .empty-title { font-size: 15px; font-weight: 600; color: var(--text); margin-bottom: 6px; }
  .empty-body { font-size: 13px; color: var(--text-muted); }

  /* Tree layout */
  .tree { padding: 16px 18px 20px; display: flex; flex-direction: column; gap: 16px; }
  .group {
    border: 1px solid var(--border);
    border-left: 4px solid var(--mc-border, var(--border));
    border-radius: 12px;
    background: #fff;
    overflow: hidden;
    transition: box-shadow 0.15s ease, transform 0.1s ease, border-color 0.15s ease;
  }
  .group.drop-active {
    border-color: var(--mc-border);
    box-shadow: 0 0 0 3px var(--mc-tint), 0 8px 24px -8px rgba(0,0,0,0.15);
    transform: translateY(-1px);
  }
  .group.orphans {
    background: #fafbfc;
    border-left: 4px solid #cbd5e1;
  }
  .group-header {
    padding: 10px 14px;
    background: var(--surface-muted);
    border-bottom: 1px solid var(--border);
    display: flex; align-items: baseline; gap: 10px;
  }
  .group-title {
    font-size: 11px; font-weight: 700; letter-spacing: 0.06em; text-transform: uppercase;
    color: var(--text-2);
  }

  .row {
    display: grid;
    grid-template-columns: auto auto auto 1fr auto auto auto auto;
    align-items: center;
    gap: 12px;
    padding: 10px 14px;
    border-top: 1px solid var(--border);
  }
  .row:first-child { border-top: none; }
  .row.master {
    background: linear-gradient(180deg, var(--mc-tint, #fbfcff) 0%, #ffffff 100%);
    grid-template-columns: auto auto 1fr auto auto auto auto;
    border-top: none;
  }
  .row.slave {
    background: color-mix(in srgb, var(--mc-tint, #fcfdff) 35%, #ffffff);
    grid-template-columns: 20px auto auto 1fr auto auto auto auto;
    padding-left: 48px;
    cursor: grab;
  }
  .row.slave:active { cursor: grabbing; }
  .row.slave.off {
    background: repeating-linear-gradient(
      45deg,
      #f1f5f9 0px, #f1f5f9 8px,
      #e2e8f0 8px, #e2e8f0 9px
    );
    filter: grayscale(1);
    opacity: 0.65;
  }
  .row.slave.off .label-btn,
  .row.slave.off .num,
  .row.slave.off .status-pill,
  .row.slave.off .chip,
  .row.slave.off .role-badge,
  .row.slave.off .tree-mark { color: #94a3b8 !important; }
  .row.slave.off .num .muted,
  .row.slave.off .label-col .small { color: #cbd5e1 !important; }

  .toggle {
    display: inline-flex; align-items: center; gap: 8px;
    padding: 4px 10px 4px 4px;
    border: 1px solid var(--border);
    border-radius: 999px;
    background: #fff;
    font-size: 12px; font-weight: 600;
    cursor: pointer;
  }
  .toggle-track {
    position: relative; display: inline-block;
    width: 28px; height: 16px;
    background: #cbd5e1; border-radius: 999px;
    transition: background .15s ease;
  }
  .toggle-thumb {
    position: absolute; top: 2px; left: 2px;
    width: 12px; height: 12px;
    background: #fff; border-radius: 50%;
    box-shadow: 0 1px 2px rgba(0,0,0,0.2);
    transition: left .15s ease;
  }
  .toggle.on { border-color: #10b981; background: #ecfdf5; color: #047857; }
  .toggle.on .toggle-track { background: #10b981; }
  .toggle.on .toggle-thumb { left: 14px; }
  .toggle.off { color: #64748b; }
  .toggle:hover { filter: brightness(0.97); }
  .row.orphan {
    grid-template-columns: auto 1fr auto auto auto auto;
    background: #fff;
    cursor: grab;
  }
  .row.orphan:active { cursor: grabbing; }
  .row.orphan.dragging { opacity: 0.4; }

  .tree-mark { color: var(--text-muted); font-weight: 600; }
  .label-col { display: flex; flex-direction: column; min-width: 0; }
  .label-col .small { font-size: 11px; }
  .subtle { color: var(--text-2); }
  .num { font-variant-numeric: tabular-nums; text-align: right; white-space: nowrap; }
  .small { font-size: 11px; }
  .muted { color: var(--text-muted); }

  .role-badge {
    font-size: 10px; font-weight: 700; letter-spacing: 0.06em;
    padding: 3px 7px; border-radius: 4px;
  }
  .master-badge { background: var(--mc-border, #f59e0b); color: #fff; }
  .slave-badge  { background: var(--mc-tint, #e0f2fe); color: var(--mc-text, #075985); border: 1px solid var(--mc-border); }

  .label-btn {
    background: none; border: none; padding: 2px 4px;
    font: inherit; color: inherit; cursor: text; text-align: left;
    border-radius: 4px;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .label-btn:hover { background: var(--surface-muted); }
  .label-edit {
    font: inherit; padding: 2px 6px;
    border: 1px solid var(--primary); border-radius: 4px;
    background: #fff; width: 100%;
  }
  .status-pill {
    display: inline-flex; align-items: center; gap: 6px;
    font-size: 10px; font-weight: 700; letter-spacing: 0.04em;
    text-transform: uppercase;
    padding: 3px 9px 3px 8px; border-radius: 999px;
    background: #fef2f2; color: #b91c1c;
    border: 1px solid #fecaca;
    white-space: nowrap;
  }
  .status-pill.online {
    background: #ecfdf5; color: #047857; border-color: #a7f3d0;
  }
  .status-dot {
    width: 6px; height: 6px; border-radius: 50%;
    background: #dc2626;
    box-shadow: 0 0 0 2px rgba(220, 38, 38, 0.15);
  }
  .status-pill.online .status-dot {
    background: #10b981;
    box-shadow: 0 0 0 2px rgba(16, 185, 129, 0.2);
    animation: pulse 2s ease-in-out infinite;
  }
  @keyframes pulse {
    0%, 100% { box-shadow: 0 0 0 2px rgba(16, 185, 129, 0.2); }
    50%      { box-shadow: 0 0 0 4px rgba(16, 185, 129, 0.08); }
  }
  .row-actions { display: inline-flex; gap: 6px; justify-content: flex-end; }
  .row-actions.wrap { flex-wrap: wrap; max-width: 100%; }

  .children { display: flex; flex-direction: column; }
  .link-zone {
    padding: 10px 14px 12px 48px;
    display: flex; align-items: center; gap: 8px; flex-wrap: wrap;
    border-top: 1px dashed var(--mc-border, var(--border));
    background: color-mix(in srgb, var(--mc-tint, #fcfdff) 25%, #ffffff);
  }
  .group.drop-active .link-zone::before {
    content: "↓ Drop here to attach";
    color: var(--mc-text);
    font-weight: 600;
    font-size: 12px;
    margin-right: 6px;
  }
  .candidate {
    display: inline-flex; align-items: center; gap: 6px;
    padding: 4px 10px;
    border: 1px solid var(--mc-border, var(--border)); border-radius: 6px;
    background: #fff; font-size: 12px; font-weight: 500;
    color: var(--mc-text, var(--text));
    cursor: pointer;
    transition: background 0.1s ease, transform 0.1s ease;
  }
  .candidate:hover { background: var(--mc-tint, var(--primary-soft)); transform: translateY(-1px); }
  .ghost {
    background: transparent; border: 1px solid var(--border);
    color: var(--text-2); padding: 4px 10px; border-radius: 6px;
    font-size: 12px; cursor: pointer;
  }
  .ghost:hover { background: var(--surface-muted); }
</style>
