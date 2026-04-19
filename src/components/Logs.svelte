<script lang="ts">
  import type { LogEntry } from "../lib/api";
  import { formatTime } from "../lib/format";
  import VirtualList from "../lib/VirtualList.svelte";
  export let logs: LogEntry[];
  const ROW_H = 30;
</script>

<div class="card">
  <div class="card-header">
    <h2>Logs</h2>
    <span class="chip">{logs.length}</span>
  </div>
  <div class="log-body">
    {#if logs.length === 0}
      <div class="empty">No events yet.</div>
    {:else}
      <VirtualList items={logs} rowHeight={ROW_H} let:item>
        <div class="line">
          <span class="ts num">{formatTime(item.ts)}</span>
          <span class="lvl {item.level}">{item.level}</span>
          <span class="src">{item.source}</span>
          <span class="msg">{item.message}</span>
        </div>
      </VirtualList>
    {/if}
  </div>
</div>

<style>
  .log-body {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 12px;
    height: 70vh;
  }
  .line {
    display: grid;
    grid-template-columns: 90px 64px 180px 1fr;
    gap: 12px;
    padding: 6px 18px;
    border-bottom: 1px solid #f5f6f8;
    height: 30px;
    box-sizing: border-box;
    align-items: center;
  }
  .line:hover { background: #fafbfc; }
  .ts { color: var(--text-muted); }
  .lvl { text-transform: uppercase; font-size: 10px; letter-spacing: 0.06em; font-weight: 600; display: inline-block; }
  .lvl.info { color: var(--primary); }
  .lvl.warn { color: var(--warning); }
  .lvl.error { color: var(--danger); }
  .src { color: var(--text-2); }
  .msg { color: var(--text); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
</style>
