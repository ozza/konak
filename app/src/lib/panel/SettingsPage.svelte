<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount } from "svelte";
  import { t } from "$lib/i18n";
  import { errorText, type AppInfo, type Settings } from "./api";
  import BackupSettings from "./BackupSettings.svelte";
  import GeneralSettings from "./GeneralSettings.svelte";
  import RestoreDialog from "./RestoreDialog.svelte";

  let { apps }: { apps: AppInfo[] } = $props();

  let settings = $state<Settings | null>(null);
  let loadError = $state<string | null>(null);
  let saveError = $state<string | null>(null);
  let savedAt = $state(0);
  let restoring = $state(false);

  async function load() {
    try {
      settings = await invoke<Settings>("get_settings");
    } catch (e) {
      loadError = errorText(e).message ?? errorText(e).details;
    }
  }

  /** Saves at once; the backend validates and returns what it stored. */
  async function save(next: Settings): Promise<boolean> {
    saveError = null;
    try {
      settings = await invoke<Settings>("save_settings", { settings: next });
      savedAt = Date.now();
      return true;
    } catch (e) {
      saveError = errorText(e).message ?? errorText(e).details;
      return false;
    }
  }

  onMount(load);
</script>

<section aria-labelledby="settings-title" class="flex flex-col gap-5">
  <div class="flex flex-wrap items-baseline justify-between gap-3">
    <h1 id="settings-title" class="text-2xl font-bold tracking-tight">{t("panel.settings.title")}</h1>
    <p class="text-xs" role="status" aria-live="polite">
      {#if saveError}<span class="text-error">{saveError}</span>{:else if savedAt}<span class="text-muted">{t("panel.settings.saved")}</span>{/if}
    </p>
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

<RestoreDialog open={restoring} {apps} onclose={() => (restoring = false)} />
