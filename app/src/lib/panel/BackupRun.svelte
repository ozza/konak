<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount } from "svelte";
  import { has, locale, t } from "$lib/i18n";
  import { appName, errorText, formatBytes, type BackupEntry, type BackupProgress } from "./api";
  import Icon from "./Icon.svelte";

  // "Back up now", its progress, the list of backups in the location, and the way into restore.
  let { ready, onrestore }: { ready: boolean; onrestore: () => void } = $props();

  let progress = $state<BackupProgress | null>(null);
  let startError = $state<string | null>(null);
  let history = $state<BackupEntry[] | null>(null);
  let running = $derived(progress?.state === "running");
  let pct = $derived(progress?.total_bytes ? Math.min(100, Math.round(((progress.done_bytes ?? 0) / progress.total_bytes) * 100)) : null);

  const stepText = (step: string) => (has(`panel.backup.step.${step}`) ? t(`panel.backup.step.${step}`) : step);
  const when = (unix: number) => new Date(unix * 1000).toLocaleString(locale, { dateStyle: "medium", timeStyle: "short" });
  const message = (e: string | null) => (e ? (errorText(e).message ?? errorText(e).details) : null);

  async function refresh() {
    history = await invoke<BackupEntry[]>("list_backups").catch(() => []);
  }

  async function start() {
    startError = null;
    try {
      await invoke("backup_now");
    } catch (e) {
      startError = message(String(e));
    }
  }

  onMount(() => {
    refresh();
    const off = listen<BackupProgress>("backup-progress", ({ payload }) => {
      progress = payload;
      if (payload.state !== "running") refresh();
    });
    return () => void off.then((f) => f());
  });
</script>

<div class="flex flex-col gap-3 border-t border-border-soft pt-4">
  <div class="flex flex-wrap items-center gap-2">
    <button type="button" class="btn-primary" disabled={!ready || running} onclick={start}>
      <Icon name="restart" />{running ? t("panel.backup.running") : t("panel.backup.now")}
    </button>
    <button type="button" class="btn" disabled={running} onclick={onrestore}>{t("panel.backup.restore")}</button>
  </div>
  {#if !ready}<p class="text-xs text-muted">{t("panel.backup.not_ready")}</p>{/if}
  {#if startError}<p class="text-sm text-error" role="alert">{startError}</p>{/if}

  {#if progress}
    <div class="flex flex-col gap-1.5" role="status">
      <p class="text-[13px]">
        {#if progress.state === "running"}{stepText(progress.step)}{#if progress.app} · {appName(progress.app)}{/if}
        {:else if progress.state === "done"}<span class="text-ok">{t("panel.backup.done")}</span>
        {:else}<span class="text-error">{t("panel.backup.failed")} {message(progress.error)}</span>{/if}
      </p>
      {#if running}
        <div class="h-1.5 overflow-hidden rounded-full bg-border">
          <div class="h-full rounded-full bg-accent {pct === null ? 'w-1/3 animate-pulse' : ''}" style:width={pct === null ? undefined : `${pct}%`}></div>
        </div>
        <p class="text-xs text-muted">{t("panel.backup.apps_pause")}</p>
      {/if}
    </div>
  {/if}

  <h3 class="label-caps mt-2">{t("panel.backup.history")}</h3>
  {#if history === null}
    <p class="text-[13px] text-muted" role="status">{t("panel.loading")}</p>
  {:else if history.length === 0}
    <p class="text-[13px] text-muted">{t("panel.backup.none")}</p>
  {:else}
    <ul class="flex flex-col">
      {#each history as h (h.file)}
        <li class="flex flex-wrap items-center gap-x-3 gap-y-0.5 border-b border-border-soft py-2.5 text-[13px] last:border-0">
          <span class="w-4 {h.status === 'ok' ? 'text-ok' : h.status === 'failed' ? 'text-error' : 'text-warn'}" aria-hidden="true">{h.status === "ok" ? "●" : h.status === "failed" ? "✕" : "!"}</span>
          <span class="font-medium">{when(h.created_unix)}</span>
          <span class="text-muted">{formatBytes(h.size_bytes)}</span>
          <span class="text-muted">{h.apps.map(appName).join(", ")}</span>
          <span class="sr-only">{t(`panel.backup.status_${h.status}`)}</span>
          {#if h.error}<span class="w-full pl-7 text-xs text-error">{message(h.error)}</span>{/if}
        </li>
      {/each}
    </ul>
  {/if}
</div>
