<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { t } from "$lib/i18n";
  import { errorText } from "./api";
  import Icon from "./Icon.svelte";

  let {
    networkCategory,
    firewallMissing,
    repairing,
    repairError,
  }: { networkCategory: string | null; firewallMissing: boolean; repairing: boolean; repairError: string | null } = $props();
  let repairFailure = $derived(repairError ? errorText(repairError) : null);
</script>

{#if firewallMissing}
  <section class="flex flex-wrap items-start gap-4 rounded-xl border border-warn-border bg-warn-bg px-[18px] py-4" aria-labelledby="repair-title">
    <span class="inline-flex size-9 shrink-0 items-center justify-center rounded-full bg-warn-icon text-warn-title"><Icon name="warn" size={18} /></span>
    <div class="flex min-w-0 flex-[1_1_320px] flex-col gap-1.5">
      <h2 id="repair-title" class="font-semibold text-warn-title">{t("panel.repair.title")}</h2>
      <p class="text-[13px] text-warn-text">{t("panel.repair.body")}</p>
      {#if repairFailure}
        <div class="text-[13px] text-error" role="alert">
          <p class="font-medium">{t("panel.repair.failed")}</p>
          {#if repairFailure.message}
            <p>{repairFailure.message}</p>
          {:else}
            <details>
              <summary class="cursor-pointer">{t("errors.details")}</summary>
              <pre class="mt-2 text-xs whitespace-pre-wrap">{repairFailure.details}</pre>
            </details>
          {/if}
        </div>
      {/if}
    </div>
    <button type="button" class="btn-primary" disabled={repairing} onclick={() => invoke("repair_network")}>
      {repairing ? t("panel.repair.running") : t("panel.repair.button")}
    </button>
    {#if repairing}<p class="sr-only" role="status">{t("panel.repair.running")}</p>{/if}
  </section>
{/if}

{#if networkCategory === "Public"}
  <p class="flex items-start gap-3 rounded-xl border border-warn-border bg-warn-bg px-[18px] py-3.5 text-[13px] text-warn-text" role="status">
    <span class="mt-0.5 text-warn-title"><Icon name="warn" /></span>{t("panel.network.public")}
  </p>
{/if}
