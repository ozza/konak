<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { t } from "$lib/i18n";
  import { errorText, type ScheduledRun, type WebPorts } from "./api";
  import Icon from "./Icon.svelte";

  let {
    networkCategory,
    firewallMissing,
    repairing,
    repairError,
    webPorts,
    backupFailure,
    onbackup,
  }: {
    networkCategory: string | null;
    firewallMissing: boolean;
    repairing: boolean;
    repairError: string | null;
    webPorts: WebPorts | null;
    backupFailure: ScheduledRun | null;
    onbackup: () => void;
  } = $props();
  let backupReason = $derived(backupFailure ? (errorText(backupFailure.error ?? "").message ?? backupFailure.detail ?? backupFailure.error ?? "") : "");
  let repairFailure = $derived(repairError ? errorText(repairError) : null);
</script>

{#if webPorts && webPorts.http !== 80}
  <section class="flex items-start gap-4 rounded-xl border border-warn-border bg-warn-bg px-[18px] py-4" aria-labelledby="port-title">
    <span class="inline-flex size-9 shrink-0 items-center justify-center rounded-full bg-warn-icon text-warn-title"><Icon name="warn" size={18} /></span>
    <div class="flex min-w-0 flex-col gap-1.5">
      <h2 id="port-title" class="font-semibold text-warn-title">{t("panel.port80.title")}</h2>
      <p class="text-[13px] text-warn-text">
        {webPorts.port80_owner ? t("panel.port80.owner", { owner: webPorts.port80_owner }) : t("panel.port80.owner_unknown")}
        {t("panel.port80.body", { port: webPorts.http })}
      </p>
      <p class="text-[13px] text-warn-text">{t("panel.port80.restart_note")}</p>
    </div>
  </section>
{/if}

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

{#if backupFailure}
  <section class="flex flex-wrap items-start gap-4 rounded-xl border border-warn-border bg-warn-bg px-[18px] py-4" aria-labelledby="backup-failed-title">
    <span class="inline-flex size-9 shrink-0 items-center justify-center rounded-full bg-warn-icon text-warn-title"><Icon name="warn" size={18} /></span>
    <p id="backup-failed-title" class="min-w-0 flex-[1_1_320px] text-[13px] text-warn-text">{t("panel.notice.backup_failed", { reason: backupReason })}</p>
    <button type="button" class="btn" onclick={onbackup}>{t("panel.notice.backup_open")}</button>
  </section>
{/if}
