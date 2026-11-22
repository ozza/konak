<script lang="ts">
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { t } from "$lib/i18n";
  import { appName, type AppInfo, type Failure, type Op } from "./api";
  import FailureNote from "./FailureNote.svelte";
  import RenameForm from "./RenameForm.svelte";
  import StatusBadge from "./StatusBadge.svelte";

  let {
    app,
    apps,
    busy,
    failed,
    onrestart,
    onclaim,
    onlogs,
    onremove,
    ondismiss,
  }: {
    app: AppInfo;
    apps: AppInfo[];
    busy: Op | undefined;
    failed: Failure | undefined;
    onrestart: () => void;
    onclaim: () => void;
    onlogs: () => void;
    onremove: () => void;
    ondismiss: () => void;
  } = $props();

  const titleId = $props.id();
  let renaming = $state(false);
  let renameButton = $state<HTMLButtonElement>();
  let label = $derived(appName(app.id));
  let host = $derived(app.url.replace(/^https?:\/\//, ""));

  function renameDone() {
    renaming = false;
    // The form is gone; keep keyboard focus on the card.
    queueMicrotask(() => renameButton?.focus());
  }
</script>

<li class="flex flex-col gap-3 rounded-md border border-border bg-panel p-4" aria-labelledby={titleId} aria-busy={busy ? "true" : undefined}>
  <div class="flex flex-wrap items-start justify-between gap-2">
    <div class="min-w-0">
      <h3 id={titleId} class="font-semibold break-words">{label}</h3>
      {#if app.claimed}
        <button
          type="button"
          class="max-w-full truncate rounded font-mono text-sm text-accent underline-offset-2 hover:underline focus-visible:outline-2 focus-visible:outline-accent"
          onclick={() => openUrl(app.url)}>{host}</button
        >
      {:else}
        <p class="text-sm break-words text-muted">{t("panel.claim.future_name", { name: host })}</p>
      {/if}
    </div>
    <StatusBadge status={app.status} />
  </div>

  {#if !app.claimed}
    <div class="flex flex-col gap-3 rounded-md border border-warn-border bg-warn-bg p-3 text-sm">
      <p class="font-medium">{t("panel.claim.notice")}</p>
      <p>{t("panel.claim.local_only", { url: app.local_url })}</p>
      <div class="flex flex-wrap gap-2">
        <button type="button" class="btn-primary" onclick={() => openUrl(app.local_url)}>{t("panel.claim.open_setup")}</button>
        <button type="button" class="btn" disabled={!!busy} onclick={onclaim}>{t("panel.claim.done")}</button>
      </div>
    </div>
  {/if}

  {#if busy}
    <p class="text-sm text-accent" role="status">{t(`panel.busy.${busy}`)}</p>
  {/if}

  {#if failed}
    <FailureNote failure={failed} {ondismiss} />
  {/if}

  {#if renaming}
    <RenameForm {app} {apps} ondone={renameDone} />
  {:else}
    <div class="flex flex-wrap gap-2" role="group" aria-label={t("panel.actions.group", { name: label })}>
      {#if app.claimed}
        <button type="button" class="btn-primary" onclick={() => openUrl(app.url)}>{t("panel.actions.open")}</button>
      {/if}
      <button type="button" class="btn" disabled={!!busy} onclick={onrestart}>{t("panel.actions.restart")}</button>
      <button type="button" class="btn" onclick={onlogs}>{t("panel.actions.logs")}</button>
      <button type="button" class="btn" disabled={!!busy} bind:this={renameButton} onclick={() => (renaming = true)}>{t("panel.actions.rename")}</button>
      <button type="button" class="btn text-error" disabled={!!busy} onclick={onremove}>{t("panel.actions.remove")}</button>
    </div>
  {/if}
</li>
