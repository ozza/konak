<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { open as pickFile } from "@tauri-apps/plugin-dialog";
  import { has, locale, t } from "$lib/i18n";
  import { appName, errorText, formatBytes, type AppInfo, type BackupManifest, type BackupProgress } from "./api";
  import Dialog from "./Dialog.svelte";
  import RestoreSummary from "./RestoreSummary.svelte";

  // Restore: pick a backup file, unlock it, choose what to bring back, confirm, follow progress.
  let { open, apps, onclose }: { open: boolean; apps: AppInfo[]; onclose: () => void } = $props();

  let file = $state<string | null>(null);
  let password = $state("");
  let manifest = $state<BackupManifest | null>(null);
  let chosen = $state<string[]>([]);
  let replace = $state(false);
  let photos = $state(false);
  let media = $state(false);
  let error = $state<string | null>(null);
  let busy = $state(false);
  let progress = $state<BackupProgress | null>(null);
  const id = $props.id();

  let clashes = $derived(chosen.filter((a) => apps.some((x) => x.id === a)));
  let running = $derived(progress?.state === "running");
  const message = (e: unknown) => errorText(e).message ?? errorText(e).details;
  const stepText = (step: string) => (has(`panel.backup.step.${step}`) ? t(`panel.backup.step.${step}`) : step);

  $effect(() => {
    if (!open) return;
    file = manifest = progress = error = null;
    password = "";
    replace = photos = media = false;
    const off = listen<BackupProgress>("restore-progress", ({ payload }) => (progress = payload));
    return () => void off.then((f) => f());
  });

  async function choose() {
    const f = await pickFile({ multiple: false, directory: false, filters: [{ name: "Konak", extensions: ["konakbackup"] }] });
    if (typeof f === "string") (file = f), (manifest = null);
  }

  async function inspect(e: SubmitEvent) {
    e.preventDefault();
    if (!file) return;
    busy = true;
    error = null;
    try {
      manifest = await invoke<BackupManifest>("inspect_backup", { path: file, password });
      chosen = manifest.apps.map((a) => a.id);
    } catch (err) {
      error = message(err);
    } finally {
      busy = false;
    }
  }

  async function restore() {
    if (!file || !manifest) return;
    error = null;
    try {
      await invoke("restore_backup", { path: file, password, apps: chosen, replace, photos, media });
    } catch (err) {
      error = message(err);
    }
  }
</script>

<Dialog {open} title={t("panel.restore.title")} onclose={() => !running && onclose()}>
  {#if !manifest}
    <form class="flex flex-col gap-3" onsubmit={inspect}>
      <p class="text-sm">{t("panel.restore.intro")}</p>
      <div class="flex flex-wrap items-center gap-2">
        <span class="min-w-0 flex-1 truncate rounded-lg border border-border bg-raised px-3 py-2 font-mono text-xs">{file ?? t("panel.restore.no_file")}</span>
        <button type="button" class="btn" onclick={choose}>{t("panel.restore.choose")}</button>
      </div>
      <label class="flex flex-col gap-1 text-sm" for="{id}-pw">{t("panel.restore.password")}
        <input id="{id}-pw" type="password" autocomplete="off" bind:value={password} aria-describedby="{id}-pw-hint" class="h-10 rounded-lg border border-control bg-surface px-3" />
      </label>
      <p id="{id}-pw-hint" class="text-xs text-muted">{t("panel.restore.password_hint")}</p>
      {#if error}<p class="text-sm text-error" role="alert">{error}</p>{/if}
      <button type="submit" class="btn-primary self-start" disabled={!file || busy}>{t("panel.restore.open")}</button>
    </form>
  {:else}
    <p class="text-sm">{t("panel.restore.from", { date: new Date(manifest.created_unix * 1000).toLocaleString(locale) })}</p>
    <fieldset class="flex flex-col gap-2" disabled={running}>
      <legend class="mb-1 text-sm font-medium">{t("panel.restore.what")}</legend>
      {#each manifest.apps as a (a.id)}
        <label class="flex items-center gap-2.5 text-sm">
          <input type="checkbox" value={a.id} bind:group={chosen} class="size-4 accent-accent" />
          {appName(a.id)} <span class="text-xs text-muted">{a.name}.local · {formatBytes(a.size_bytes)}</span>
        </label>
      {/each}
      {#if manifest.photos_bytes !== null}
        <label class="flex items-center gap-2.5 text-sm"><input type="checkbox" bind:checked={photos} class="size-4 accent-accent" />{t("panel.restore.photos", { size: formatBytes(manifest.photos_bytes) })}</label>
      {/if}
      {#if manifest.media_bytes !== null}
        <label class="flex items-center gap-2.5 text-sm"><input type="checkbox" bind:checked={media} class="size-4 accent-accent" />{t("panel.restore.media", { size: formatBytes(manifest.media_bytes) })}</label>
      {/if}
      {#if photos || media}<p class="text-xs text-muted">{t("panel.restore.folders_merge")}</p>{/if}
    </fieldset>
    {#if clashes.length > 0}
      <label class="flex items-start gap-2.5 rounded-lg border border-error-border bg-error-bg p-3 text-sm">
        <input type="checkbox" bind:checked={replace} disabled={running} class="mt-0.5 size-4 shrink-0 accent-error" />
        {t("panel.restore.replace", { apps: clashes.map(appName).join(", ") })}
      </label>
    {/if}
    {#if progress}
      <p class="text-sm" role="status">
        {#if progress.state === "running"}{stepText(progress.step)}{#if progress.app} · {appName(progress.app)}{/if}
        {:else if progress.state === "done"}<span class="text-ok">{t("panel.restore.done")}</span>
        {:else}<span class="text-error">{t("panel.restore.failed")} {progress.error ? message(progress.error) : ""}</span>{/if}
      </p>
      {#if progress.summary}<RestoreSummary summary={progress.summary} />{/if}
    {/if}
    {#if error}<p class="text-sm text-error" role="alert">{error}</p>{/if}
  {/if}
  {#snippet actions()}
    <button type="button" class="btn" disabled={running} onclick={onclose}>{progress?.state === "done" ? t("panel.logs.close") : t("panel.rename.cancel")}</button>
    {#if manifest && progress?.state !== "done"}
      <button type="button" class={clashes.length ? "btn-danger" : "btn-primary"} disabled={running || chosen.length + Number(photos) + Number(media) === 0 || (clashes.length > 0 && !replace)} onclick={restore}>
        {t("panel.restore.start")}
      </button>
    {/if}
  {/snippet}
</Dialog>
