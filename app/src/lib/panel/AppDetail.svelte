<script lang="ts">
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { t, locale } from "$lib/i18n";
  import { appName, formatBytes, type AppInfo, type AppStats, type Failure, type Op } from "./api";
  import AppTile from "./AppTile.svelte";
  import FailureNote from "./FailureNote.svelte";
  import Icon from "./Icon.svelte";
  import Meter from "./Meter.svelte";
  import RecentLogs from "./RecentLogs.svelte";
  import RenameForm from "./RenameForm.svelte";
  import StatusBadge from "./StatusBadge.svelte";
  import Containers from "./Containers.svelte";
  import ClaimBanner from "./ClaimBanner.svelte";

  let {
    app,
    apps,
    stats,
    busy,
    failed,
    version,
    onop,
    onlogs,
    onremove,
    ondismiss,
  }: {
    app: AppInfo;
    apps: AppInfo[];
    stats: AppStats | undefined;
    busy: Op | undefined;
    failed: Failure | undefined;
    version: number;
    onop: (op: Op) => void;
    onlogs: () => void;
    onremove: () => void;
    ondismiss: () => void;
  } = $props();

  let renaming = $state(false);
  let renameButton = $state<HTMLButtonElement>();
  let label = $derived(appName(app.id));
  let host = $derived(app.url.replace(/^https?:\/\//, ""));
  let stopped = $derived(app.stopped || app.status === "stopped");
  let installed = $derived(new Date(app.installed_unix * 1000).toLocaleDateString(locale, { day: "numeric", month: "short", year: "numeric" }));

  // A new app selection closes an open rename form.
  $effect(() => {
    void app.id;
    renaming = false;
  });

  function renameDone() {
    renaming = false;
    queueMicrotask(() => renameButton?.focus());
  }
</script>

<section aria-labelledby="app-title" aria-busy={busy ? "true" : undefined} class="flex flex-col gap-5">
  <div class="flex flex-wrap items-center gap-4">
    <AppTile id={app.id} size="lg" />
    <div class="flex min-w-0 flex-[1_1_280px] flex-col gap-1">
      <div class="flex flex-wrap items-center gap-2.5">
        <h1 id="app-title" class="text-2xl font-bold tracking-tight break-words">{label}</h1>
        <StatusBadge status={app.status} />
      </div>
      {#if app.claimed}
        <button type="button" class="self-start rounded font-mono text-[13px] text-link hover:underline focus-visible:outline-2 focus-visible:outline-link" onclick={() => openUrl(app.url)}
          >{app.url}</button
        >
      {:else}
        <p class="text-[13px] text-muted">{t("panel.claim.future_name", { name: host })}</p>
      {/if}
    </div>
  </div>

  {#if renaming}
    <div class="card"><RenameForm {app} {apps} ondone={renameDone} /></div>
  {:else}
    <div role="toolbar" aria-label={t("panel.actions.group", { name: label })} class="flex flex-wrap gap-2 rounded-[10px] border border-border bg-chrome p-2">
      {#if app.claimed}
        <button type="button" class="btn-primary" onclick={() => openUrl(app.url)}><Icon name="external" />{t("panel.actions.open")}</button>
      {/if}
      {#if stopped}
        <button type="button" class="btn" disabled={!!busy} onclick={() => onop("start")}><Icon name="play" />{t("panel.actions.start")}</button>
      {:else}
        <button type="button" class="btn" disabled={!!busy} onclick={() => onop("stop")}><Icon name="stop" />{t("panel.actions.stop")}</button>
      {/if}
      <button type="button" class="btn" disabled={!!busy || stopped} onclick={() => onop("restart")}><Icon name="restart" />{t("panel.actions.restart")}</button>
      <button type="button" class="btn" onclick={onlogs}><Icon name="logs" />{t("panel.actions.logs")}</button>
      <button type="button" class="btn" disabled={!!busy} bind:this={renameButton} onclick={() => (renaming = true)}><Icon name="rename" />{t("panel.actions.rename")}</button>
      <div class="flex-1"></div>
      <button type="button" class="btn text-error hover:text-error" disabled={!!busy} onclick={onremove}><Icon name="trash" />{t("panel.actions.remove")}</button>
    </div>
  {/if}

  {#if busy}
    <p class="flex items-center gap-2 text-sm text-link" role="status"><Icon name="restart" />{t(`panel.busy.${busy}`)}</p>
  {/if}
  {#if failed}
    <FailureNote failure={failed} {ondismiss} />
  {/if}
  {#if !app.claimed}
    <ClaimBanner {app} disabled={!!busy} onclaim={() => onop("claim")} />
  {/if}

  <div class="grid grid-cols-[repeat(auto-fit,minmax(260px,1fr))] gap-4">
    <div class="card">
      <h2 class="label-caps">{t("panel.detail.overview")}</h2>
      <dl class="grid grid-cols-[max-content_minmax(0,1fr)] gap-x-[18px] gap-y-2 text-[13px]">
        <dt class="text-muted">{t("panel.detail.status")}</dt><dd>{t(`panel.status.${app.status}`)}</dd>
        <dt class="text-muted">{t("panel.detail.installed")}</dt><dd>{installed}</dd>
        <dt class="text-muted">{t("panel.detail.containers")}</dt><dd>{app.containers.length}</dd>
      </dl>
    </div>
    <div class="card">
      <h2 class="label-caps">{t("panel.detail.network")}</h2>
      <dl class="grid grid-cols-[max-content_minmax(0,1fr)] gap-x-[18px] gap-y-2 text-[13px]">
        <dt class="text-muted">{t("panel.detail.address")}</dt><dd class="font-mono break-all">{host}</dd>
        <dt class="text-muted">{t("panel.detail.on_this_pc")}</dt><dd class="font-mono break-all">{app.local_url}</dd>
        <dt class="text-muted">{t("panel.detail.reachable")}</dt><dd>{app.claimed ? t("panel.detail.reach_all") : t("panel.detail.reach_pc")}</dd>
        <dt class="text-muted">{t("panel.detail.connection")}</dt><dd>{app.https ? "HTTPS" : "HTTP"}</dd>
      </dl>
    </div>
    <div class="card">
      <h2 class="label-caps">{t("panel.detail.resources")}</h2>
      {#if stats && !stopped}
        <Meter label={t("panel.detail.cpu")} value="{stats.cpu_percent.toFixed(1)}%" fraction={stats.cpu_percent / 100} />
        <Meter label={t("panel.host.memory")} value={formatBytes(stats.mem_bytes)} fraction={null} />
      {:else}
        <p class="text-[13px] text-muted">{stopped ? t("panel.detail.no_stats_stopped") : t("panel.detail.no_stats")}</p>
      {/if}
    </div>
  </div>

  <div class="grid grid-cols-[repeat(auto-fit,minmax(min(380px,100%),1fr))] gap-4">
    <Containers containers={app.containers} />
    <RecentLogs id={app.id} {version} onopen={onlogs} />
  </div>
</section>
