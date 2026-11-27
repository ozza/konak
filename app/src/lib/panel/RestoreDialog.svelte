<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { open as pickFile } from "@tauri-apps/plugin-dialog";
  import { has, locale, t } from "$lib/i18n";
  import { appName, errorText, formatBytes, type AppInfo, type BackupManifest, type BackupProgress } from "./api";
  import Dialog from "./Dialog.svelte";
  import RestoreSummary from "./RestoreSummary.svelte";

  // Restore: pick a backup file, unlock it, choose what to bring back, confirm, follow progress.
  let { open, apps, passwordSet, onclose }: { open: boolean; apps: AppInfo[]; passwordSet: boolean; onclose: () => void } = $props();

  let file = $state<string | null>(null);
  let password = $state("");
  let manifest = $state<BackupManifest | null>(null);
  let chosen = $state<string[]>([]);
  let replace = $state(false);
  let photos = $state(false);
  let media = $state(false);
  let ca = $state(false);
  let error = $state<string | null>(null);
  let busy = $state(false);
  let starting = $state(false);
  let progress = $state<BackupProgress | null>(null);
  const id = $props.id();

  // Apps with data a "keep data" removal left here: the backend refused them (restore_app_data_left);
  // they need the same Replace consent as installed apps.
  let leftData = $state<string[]>([]);
  let installedClashes = $derived(chosen.filter((a) => apps.some((x) => x.id === a)));
  let dataClashes = $derived(chosen.filter((a) => leftData.includes(a) && !installedClashes.includes(a)));
  let clashes = $derived([...installedClashes, ...dataClashes]);
  // After a run that failed before changing anything, the choices stay and it can be started again.
  let canStart = $derived(!progress || progress.state === "failed");
  let running = $derived(starting || progress?.state === "running");
  // The backup's certificate is put back only into a Konak with no apps yet (restore.rs `ca_action`).
  let caOffered = $derived(!!manifest?.ca_fingerprint && manifest.ca_valid);
  let caAllowed = $derived(apps.length === 0);
  let pct = $derived(progress?.total_bytes ? Math.min(100, Math.round(((progress.done_bytes ?? 0) / progress.total_bytes) * 100)) : null);
  const message = (e: unknown) => errorText(e).message ?? errorText(e).details;
  const stepText = (step: string) => (has(`panel.backup.step.${step}`) ? t(`panel.backup.step.${step}`) : step);

  $effect(() => {
    if (!open) return;
    file = manifest = progress = error = null;
    password = "";
    replace = photos = media = ca = starting = false;
    leftData = [];
    const off = listen<BackupProgress>("restore-progress", ({ payload }) => {
      progress = payload;
      starting = false;
      const left = payload.summary?.failed.filter((f) => f.error === "restore_app_data_left").map((f) => f.id) ?? [];
      if (left.length) leftData = [...new Set([...leftData, ...left])];
    });
    return () => void off.then((f) => f());
  });

  async function choose() {
    const f = await pickFile({ multiple: false, directory: false, filters: [{ name: "Konak", extensions: ["konakbackup"] }] });
    if (typeof f === "string") (file = f), (manifest = null);
  }

  async function inspect(e: SubmitEvent) {
    e.preventDefault();
    if (!file || busy) return;
    busy = true;
    error = null;
    try {
      manifest = await invoke<BackupManifest>("inspect_backup", { path: file, password });
      chosen = manifest.apps.map((a) => a.id);
      ca = !!manifest.ca_fingerprint && manifest.ca_valid && apps.length === 0;
    } catch (err) {
      error = message(err);
    } finally {
      busy = false;
    }
  }

  async function restore() {
    if (!file || !manifest || running) return;
    error = null;
    progress = null;
    starting = true;
    try {
      await invoke("restore_backup", { path: file, password, apps: chosen, replace, photos, media, ca: ca && caAllowed });
    } catch (err) {
      starting = false;
      error = message(err);
    }
  }
</script>

<Dialog {open} title={t("panel.restore.title")} dismissable={!running} onclose={() => !running && onclose()}>
  {#if !manifest}
    <form class="flex flex-col gap-3" onsubmit={inspect}>
      <p class="text-sm">{t("panel.restore.intro")}</p>
      <div class="flex flex-wrap items-center gap-2">
        <span class="min-w-0 flex-1 truncate rounded-lg border border-border bg-raised px-3 py-2 font-mono text-xs" title={file ?? undefined}>{file ?? t("panel.restore.no_file")}</span>
        <button type="button" class="btn" onclick={choose}>{t("panel.restore.choose")}</button>
      </div>
      <label class="flex flex-col gap-1 text-sm" for="{id}-pw">{t("panel.restore.password")}
        <input id="{id}-pw" type="password" autocomplete="off" bind:value={password} aria-describedby="{id}-pw-hint" class="h-10 rounded-lg border border-control bg-surface px-3" />
      </label>
      <p id="{id}-pw-hint" class="text-xs text-muted">{passwordSet ? t("panel.restore.password_hint") : t("panel.restore.password_needed")}</p>
      <p class="text-sm text-muted" role="status">{busy ? t("panel.restore.opening") : ""}</p>
      {#if error}<p class="text-sm text-error" role="alert">{error}</p>{/if}
      <button type="submit" class="btn-primary self-start" disabled={!file || busy || (!passwordSet && !password)}>{t("panel.restore.open")}</button>
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
      <!-- Immich keeps its photos in the Photos folder; its database expects them on start. -->
      {#if chosen.includes("immich") && manifest.photos_bytes !== null && !photos}
        <p class="rounded-md border border-warn-border bg-warn-bg p-2.5 text-xs text-warn-text" role="status">{t("panel.restore.immich_needs_photos")}</p>
      {/if}
      {#if caOffered}
        <label class="flex items-start gap-2.5 text-sm">
          <input type="checkbox" bind:checked={ca} disabled={!caAllowed} aria-describedby="{id}-ca-hint" class="mt-0.5 size-4 shrink-0 accent-accent" />
          <span class="flex min-w-0 flex-col gap-0.5">
            {t("panel.restore.ca")}
            <span id="{id}-ca-hint" class="text-xs text-muted">{caAllowed ? t("panel.restore.ca_hint") : t("panel.restore.ca_not_empty")}</span>
            <span class="font-mono text-[11px] break-all text-muted">{t("panel.restore.ca_fingerprint", { fp: manifest.ca_fingerprint ?? "" })}</span>
          </span>
        </label>
      {:else if manifest.ca_fingerprint}
        <p class="text-xs text-muted">{t("panel.restore.note.restore_ca_invalid")}</p>
      {/if}
    </fieldset>
    {#if clashes.length > 0}
      <label class="flex items-start gap-2.5 rounded-lg border border-error-border bg-error-bg p-3 text-sm">
        <input type="checkbox" bind:checked={replace} disabled={running} class="mt-0.5 size-4 shrink-0 accent-error" />
        <span class="flex flex-col gap-1">
          {#if installedClashes.length}<span>{t("panel.restore.replace", { apps: installedClashes.map(appName).join(", ") })}</span>{/if}
          {#if dataClashes.length}<span>{t("panel.restore.replace_data_left", { apps: dataClashes.map(appName).join(", ") })}</span>{/if}
        </span>
      </label>
    {/if}
    <div class="flex flex-col gap-1.5 text-sm" role="status">
      {#if starting && !progress}{t("panel.restore.starting")}
      {:else if progress?.state === "running"}{stepText(progress.step)}{#if progress.app} · {appName(progress.app)}{/if}
      {:else if progress?.state === "done"}<span class="text-ok">{t("panel.restore.done")}</span>
      {:else if progress}<span class="text-error">{t("panel.restore.failed")} {progress.error ? message(progress.error) : ""}</span>
        {#if progress.detail}<details class="mt-1 text-xs"><summary class="cursor-pointer text-muted">{t("errors.details")}</summary><pre class="mt-1 font-mono break-all whitespace-pre-wrap">{progress.detail}</pre></details>{/if}{/if}
      {#if progress?.state === "running"}
        <div class="h-1.5 overflow-hidden rounded-full bg-border" role="progressbar" aria-label={stepText(progress.step)} aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct ?? undefined}>
          <div class="h-full rounded-full bg-accent {pct === null ? 'w-1/3 animate-pulse' : ''}" style:width={pct === null ? undefined : `${pct}%`}></div>
        </div>
      {/if}
    </div>
    {#if progress?.summary}<RestoreSummary summary={progress.summary} />{/if}
    {#if error}<p class="text-sm text-error" role="alert">{error}</p>{/if}
  {/if}
  {#snippet actions()}
    <button type="button" class="btn" disabled={running} onclick={onclose}>{progress && progress.state !== "running" ? t("panel.logs.close") : t("panel.rename.cancel")}</button>
    {#if manifest && (canStart || running)}
      <button type="button" class={clashes.length ? "btn-danger" : "btn-primary"} disabled={running || chosen.length + Number(photos) + Number(media) === 0 || (clashes.length > 0 && !replace)} onclick={restore}>
        {t("panel.restore.start")}
      </button>
    {/if}
  {/snippet}
</Dialog>
