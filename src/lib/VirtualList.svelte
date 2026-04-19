<script lang="ts">
  export let items: any[];
  export let rowHeight: number;
  export let overscan = 6;
  let scrollTop = 0;
  let viewportHeight = 0;
  let viewport: HTMLDivElement;

  $: total = items.length;
  $: start = Math.max(0, Math.floor(scrollTop / rowHeight) - overscan);
  $: visibleCount = viewportHeight ? Math.ceil(viewportHeight / rowHeight) + overscan * 2 : 0;
  $: end = Math.min(total, start + visibleCount);
  $: slice = items.slice(start, end);
  $: padTop = start * rowHeight;
  $: padBottom = (total - end) * rowHeight;
</script>

<div
  bind:this={viewport}
  bind:clientHeight={viewportHeight}
  on:scroll={() => (scrollTop = viewport.scrollTop)}
  class="vl"
>
  <div style="height: {padTop}px"></div>
  {#each slice as item, i (start + i)}
    <slot {item} index={start + i} />
  {/each}
  <div style="height: {padBottom}px"></div>
</div>

<style>
  .vl { height: 100%; overflow: auto; }
</style>
