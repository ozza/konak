<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { t } from "$lib/i18n";
  import { errorText, nameError, type AppInfo } from "./api";

  let { app, apps, ondone }: { app: AppInfo; apps: AppInfo[]; ondone: () => void } = $props();

  const id = $props.id();
  // svelte-ignore state_referenced_locally
  let name = $state(app.name);
  let error = $state<string | null>(null);
  let submitted = $state(false);

  // After the first submit, the message follows the input as the user types.
  $effect(() => {
    if (submitted) error = nameError(name.trim(), app.id, apps);
  });

  async function save(e: SubmitEvent) {
    e.preventDefault();
    submitted = true;
    const value = name.trim();
    error = nameError(value, app.id, apps);
    if (error) return;
    if (value === app.name) return ondone();
    try {
      // Local checks answer here; the LAN check runs afterwards and reports through app-progress.
      await invoke("rename_app", { id: app.id, name: value });
      ondone();
    } catch (err) {
      error = String(err);
    }
  }
</script>

<form class="flex flex-col gap-2" onsubmit={save} novalidate>
  <label for="{id}-name" class="text-sm font-medium">{t("panel.rename.label")}</label>
  <div class="flex flex-wrap items-center gap-2">
    <div class="flex min-w-0 flex-1 items-center rounded-md border bg-surface focus-within:outline-2 focus-within:outline-accent {error ? 'border-error' : 'border-border'}">
      <!-- svelte-ignore a11y_autofocus -->
      <input
        id="{id}-name"
        bind:value={name}
        autofocus
        autocomplete="off"
        spellcheck="false"
        maxlength="63"
        aria-invalid={error ? "true" : undefined}
        aria-describedby="{id}-hint{error ? ` ${id}-error` : ''}"
        onkeydown={(e) => e.key === "Escape" && ondone()}
        class="min-w-0 flex-1 bg-transparent px-2 py-1.5 font-mono text-sm outline-none"
      />
      <span class="pr-2 font-mono text-sm text-muted" aria-hidden="true">.local</span>
    </div>
    <button type="submit" class="btn-primary">{t("panel.rename.save")}</button>
    <button type="button" class="btn" onclick={ondone}>{t("panel.rename.cancel")}</button>
  </div>
  <p id="{id}-hint" class="text-xs text-muted">{t("panel.rename.hint")}</p>
  {#if error}
    {@const f = errorText(error)}
    <p id="{id}-error" class="text-sm text-error" role="alert">{f.message ?? f.details}</p>
  {/if}
</form>
