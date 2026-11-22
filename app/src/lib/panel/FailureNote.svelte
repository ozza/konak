<script lang="ts">
  import { t } from "$lib/i18n";
  import type { Failure } from "./api";

  let { failure, ondismiss }: { failure: Failure; ondismiss: () => void } = $props();
</script>

<div class="rounded-md border border-error-border bg-error-bg p-3 text-sm" role="alert">
  <p class="font-medium">{t(`panel.failed.${failure.op}`)}</p>
  {#if failure.message}
    <p class="mt-1">{failure.message}</p>
  {:else if failure.details}
    <details class="mt-1">
      <summary class="cursor-pointer">{t("errors.details")}</summary>
      <pre class="mt-2 text-xs break-all whitespace-pre-wrap">{failure.details}</pre>
    </details>
  {/if}
  <button type="button" class="mt-2 text-xs underline focus-visible:outline-2 focus-visible:outline-accent" onclick={ondismiss}>{t("panel.dismiss")}</button>
</div>
