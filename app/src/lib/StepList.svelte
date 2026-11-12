<script lang="ts">
  import { t } from "$lib/i18n";

  type Step = { id: string; state: "pending" | "running" | "done" | "skipped" | "failed" };
  let { steps }: { steps: Step[] } = $props();

  // A glyph plus the state text, so state never relies on color alone.
  const glyph = { pending: "○", running: "◐", done: "✓", skipped: "–", failed: "✕" };
  const tone = { pending: "text-muted", running: "text-accent", done: "text-ok", skipped: "text-muted", failed: "text-error" };
</script>

<ol class="flex flex-col divide-y divide-border rounded-md border border-border bg-panel">
  {#each steps as step (step.id)}
    <li class="flex items-center gap-3 px-4 py-2 text-sm" aria-current={step.state === "running" ? "step" : undefined}>
      <span class="w-4 text-center {tone[step.state]}" aria-hidden="true">{glyph[step.state]}</span>
      <span class="flex-1">{t(`steps.${step.id}`)}</span>
      <span class="text-xs {tone[step.state]}">{t(`state.${step.state}`)}</span>
    </li>
  {/each}
</ol>
