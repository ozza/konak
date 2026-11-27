<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { t } from "$lib/i18n";
  import { errorText } from "./api";

  // Sets or changes the backup password. It is stored for this Windows user only and never shown again.
  let { passwordSet, ondone }: { passwordSet: boolean; ondone: () => void } = $props();

  const MIN = 12;
  const id = $props.id();
  let open = $state(false);
  let current = $state("");
  let next = $state("");
  let repeat = $state("");
  let error = $state<string | null>(null);
  let saving = $state(false);
  let saved = $state(false);

  function reset() {
    current = next = repeat = "";
    error = null;
  }

  async function save(e: SubmitEvent) {
    e.preventDefault();
    saved = false;
    if (next.length < MIN) return (error = t("panel.settings.password_short", { n: MIN }));
    if (next !== repeat) return (error = t("panel.settings.password_mismatch"));
    saving = true;
    try {
      await invoke("set_backup_password", { current: passwordSet && current ? current : null, new: next });
      reset();
      open = false;
      saved = true;
      ondone();
    } catch (err) {
      const f = errorText(err);
      error = f.message ?? f.details;
    } finally {
      saving = false;
    }
  }
</script>

<div class="flex flex-col gap-2">
  <p class="text-sm font-medium">{t("panel.settings.password")}</p>
  <p class="text-xs text-muted">{passwordSet ? t("panel.settings.password_is_set") : t("panel.settings.password_not_set")}</p>
  {#if saved}<p class="text-xs text-ok" role="status">{t("panel.settings.password_saved")}</p>{/if}

  {#if !open}
    <div>
      <button type="button" class="btn" onclick={() => ((open = true), (saved = false))}>
        {passwordSet ? t("panel.settings.password_change") : t("panel.settings.password_set")}
      </button>
    </div>
  {:else}
    <form class="flex max-w-sm flex-col gap-3 rounded-lg border border-border bg-raised p-4" onsubmit={save} novalidate>
      <p class="rounded-md border border-warn-border bg-warn-bg p-2.5 text-xs text-warn-text">{t("panel.settings.password_warning")}</p>
      {#if passwordSet}
        <label class="flex flex-col gap-1 text-sm" for="{id}-current">{t("panel.settings.password_current")}
          <input id="{id}-current" type="password" autocomplete="current-password" bind:value={current} class="h-10 rounded-lg border border-control bg-surface px-3" />
        </label>
      {/if}
      <label class="flex flex-col gap-1 text-sm" for="{id}-new">{t("panel.settings.password_new", { n: MIN })}
        <input id="{id}-new" type="password" autocomplete="new-password" bind:value={next} class="h-10 rounded-lg border border-control bg-surface px-3" />
      </label>
      <label class="flex flex-col gap-1 text-sm" for="{id}-repeat">{t("panel.settings.password_repeat")}
        <input id="{id}-repeat" type="password" autocomplete="new-password" bind:value={repeat} class="h-10 rounded-lg border border-control bg-surface px-3" />
      </label>
      {#if error}<p class="text-sm text-error" role="alert">{error}</p>{/if}
      <div class="flex gap-2">
        <button type="submit" class="btn-primary" disabled={saving}>{t("panel.rename.save")}</button>
        <button type="button" class="btn" onclick={() => ((open = false), reset())}>{t("panel.rename.cancel")}</button>
      </div>
    </form>
  {/if}
</div>
