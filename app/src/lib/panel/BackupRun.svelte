<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount } from "svelte";
  import { has, locale, t } from "$lib/i18n";
  import { appName, errorText, formatBytes, type BackupEntry, type BackupProgress, type BackupStatus } from "./api";
  import Icon from "./Icon.svelte";

  // "Back up now", its progress, the list of backups in the location, and the way into restore.
  let { ready, location: folder, onrestore }: { ready: boolean; location: string | null; onrestore: () => void } = $props();

  // The list shows the chosen folder, so it reloads when the folder changes.
  $effect(() => {
    void folder;
    refresh();
  });

  let progress = $state<BackupProgress | null>(null);
  let startError = $state<string | null>(null);
  let history = $state<BackupEntry[] | null>(null);
  // What runs now, from the backend: survives leaving this page and a reload of the window.
  let other = $state<BackupStatus["running"]>(null);
  let running = $derived(progress?.state === "running" || other === "backup");
  let blocked = $derived(running || other === "restore" || other === "recover");
  let cancelling = $state(false);
  let pct = $derived(progress?.total_bytes ? Math.min(100, Math.round(((progress.done_bytes ?? 0) / progress.total_bytes) * 100)) : null);

  const stepText = (step: string) => (has(`panel.backup.step.${step}`) ? t(`panel.backup.step.${step}`) : step);
  const when = (unix: number) => new Date(unix * 1000).toLocaleString(locale, { dateStyle: "medium", timeStyle: "short" });
  const message = (e: string | null) => (e ? (errorText(e).message ?? errorText(e).details) : null);

  async function refresh() {
    history = await invoke<BackupEntry[]>("list_backups").catch(() => []);
  }

  async function status() {
    other = (await invoke<BackupStatus>("backup_status").catch(() => ({ running: null }))).running;
  }

  async function cancel() {
    cancelling = true;
    try {
      await invoke("cancel_backup");
    } catch (e) {
      cancelling = false;
      startError = message(String(e));
    }
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
    status();
    const off = listen<BackupProgress>("backup-progress", ({ payload }) => {
      progress = payload;
      if (payload.state !== "running") (refresh(), status(), (cancelling = false));
    });
    // A restore elsewhere (its dialog) or the startup recovery: checked while this page is open.
    const timer = setInterval(() => !progress || progress.state !== "running" ? status() : undefined, 5000);
    return () => {
      clearInterval(timer);
      void off.then((f) => f());
    };
  });
</script>

<div class="flex flex-col gap-3 border-t border-border-soft pt-4">
  <div class="flex flex-wrap items-center gap-2">
    <button type="button" class="btn-primary" disabled={!ready || blocked} onclick={start}>
      <Icon name="restart" />{running ? t("panel.backup.running") : t("panel.backup.now")}
    </button>
    {#if running}<button type="button" class="btn" disabled={cancelling} onclick={cancel}>{t("panel.backup.cancel")}</button>{/if}
    <button type="button" class="btn" disabled={blocked} onclick={onrestore}>{t("panel.backup.restore")}</button>
  </div>
  {#if other === "restore"}<p class="text-xs text-muted">{t("panel.backup.busy_restore")}</p>
  {:else if other === "recover"}<p class="text-xs text-muted">{t("panel.backup.busy_recover")}</p>{/if}
  {#if !ready}<p class="text-xs text-muted">{t("panel.backup.not_ready")}</p>{/if}
  {#if startError}<p class="text-sm text-error" role="alert">{startError}</p>{/if}

  {#if progress}
    <div class="flex flex-col gap-1.5" role="status">
      <div class="text-[13px]">
        {#if progress.state === "running"}{stepText(progress.step)}{#if progress.app} · {appName(progress.app)}{/if}
        {:else if progress.state === "done" && progress.skipped}<span class="text-warn-text">{t("panel.backup.done_partial", { n: progress.skipped })}</span>
          {#if progress.notes?.length}<details class="mt-1 text-xs"><summary class="cursor-pointer text-muted">{t("panel.backup.left_out")}</summary><ul class="mt-1 font-mono break-all">{#each progress.notes as n, i (i)}<li>{n}</li>{/each}</ul></details>{/if}
        {:else if progress.state === "done"}<span class="text-ok">{t("panel.backup.done")}</span>
        {:else if progress.state === "cancelled"}<span class="text-muted">{t("panel.backup.cancelled")}</span>
        {:else}<span class="text-error">{t("panel.backup.failed")} {message(progress.error)}</span>
          {#if progress.detail}<details class="mt-1 text-xs"><summary class="cursor-pointer text-muted">{t("errors.details")}</summary><pre class="mt-1 font-mono break-all whitespace-pre-wrap">{progress.detail}</pre></details>{/if}{/if}
      </div>
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
          <span class="w-4 {h.status === 'ok' ? 'text-ok' : h.status === 'failed' ? 'text-error' : h.status === 'cancelled' ? 'text-muted' : 'text-warn'}" aria-hidden="true">{h.status === "ok" ? "●" : h.status === "failed" ? "✕" : h.status === "cancelled" ? "–" : "!"}</span>
          <span class="font-medium">{when(h.created_unix)}</span>
          <span class="text-muted">{formatBytes(h.size_bytes)}</span>
          <span class="text-muted">{h.apps.map(appName).join(", ")}</span>
          <span class="sr-only">{t(`panel.backup.status_${h.status}`)}</span>
          {#if h.error}<span class="w-full pl-7 text-xs text-error">{message(h.error)}</span>{/if}
          {#if h.status === "partial"}
            <details class="w-full pl-7 text-xs text-warn-text"><summary class="cursor-pointer">{t("panel.backup.status_partial")} ({h.skipped ?? 0})</summary>
              <ul class="mt-1 font-mono break-all text-muted">{#each h.notes ?? [] as n, i (i)}<li>{n}</li>{/each}</ul></details>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</div>
