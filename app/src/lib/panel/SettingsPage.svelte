<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount, tick } from "svelte";
  import { t } from "$lib/i18n";
  import { errorText, type AppInfo, type Settings } from "./api";
  import BackupSettings from "./BackupSettings.svelte";
  import GeneralSettings from "./GeneralSettings.svelte";
  import RestoreDialog from "./RestoreDialog.svelte";

  let { apps }: { apps: AppInfo[] } = $props();

  let settings = $state<Settings | null>(null);
  let loadError = $state<string | null>(null);
  let saved = $state(false);
  let restoring = $state(false);

  async function load() {
    try {
      settings = await invoke<Settings>("get_settings");
    } catch (e) {
      loadError = errorText(e).message ?? errorText(e).details;
    }
  }

  // Saves run one after another, each on the value the previous one stored, so a slow save (a network
  // folder) cannot be overwritten by a later one built from an older copy.
  let queue: Promise<unknown> = Promise.resolve();

  /** Applies `patch` to the stored settings and saves; resolves to an error text, or null when saved. */
  function save(patch: (s: Settings) => Settings): Promise<string | null> {
    const run = queue.then(async () => {
      saved = false;
      try {
        settings = await invoke<Settings>("save_settings", { settings: patch(settings!) });
        // Cleared and set again so a screen reader announces every save, not only the first.
        await tick();
        saved = true;
        return null;
      } catch (e) {
        // The caller puts its control back to the stored value.
        return errorText(e).message ?? errorText(e).details;
      }
    });
    queue = run;
    return run;
  }

  onMount(load);
</script>

<section aria-labelledby="settings-title" class="flex flex-col gap-5">
  <div class="flex flex-wrap items-baseline justify-between gap-3">
    <h1 id="settings-title" class="text-2xl font-bold tracking-tight">{t("panel.settings.title")}</h1>
    <p class="text-xs text-muted" role="status" aria-live="polite">{saved ? t("panel.settings.saved") : ""}</p>
  </div>

  {#if loadError}
    <p class="text-sm text-error" role="alert">{loadError}</p>
  {:else if !settings}
    <p class="text-sm text-muted" role="status">{t("panel.loading")}</p>
  {:else}
    <GeneralSettings {settings} onsave={save} />
    <BackupSettings {settings} {apps} onsave={save} onreload={load} onrestore={() => (restoring = true)} />
  {/if}
</section>

<RestoreDialog open={restoring} {apps} passwordSet={settings?.backup.password_set ?? false} onclose={() => (restoring = false)} />
