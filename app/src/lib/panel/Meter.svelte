<script lang="ts">
  // A labelled usage bar. `fraction` is 0..1; null shows the label and value without a bar.
  let { label, value, fraction }: { label: string; value: string; fraction: number | null } = $props();
  let pct = $derived(fraction === null ? 0 : Math.round(Math.min(1, Math.max(0, fraction)) * 100));
</script>

<div class="flex flex-col gap-1.5">
  <div class="flex justify-between gap-3 text-[13px]">
    <span class="text-muted">{label}</span>
    <span class="font-mono">{value}</span>
  </div>
  {#if fraction !== null}
    <div class="h-1.5 overflow-hidden rounded-full bg-border" role="meter" aria-label={label} aria-valuenow={pct} aria-valuemin={0} aria-valuemax={100} aria-valuetext={value}>
      <div class="h-full rounded-full bg-accent" style:width="{pct}%"></div>
    </div>
  {/if}
</div>
