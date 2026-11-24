<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { t } from "$lib/i18n";
  import { errorText } from "./api";

  const LINES = 8;
  // `version` changes after an operation on the app, so the preview reloads.
  let { id, version, onopen }: { id: string; version: number; onopen: () => void } = $props();

  let text = $state<string | null>(null);
  let error = $state<string | null>(null);

  $effect(() => {
    const current = id;
    void version;
    text = null;
    error = null;
    // Already redacted by the backend (apps::logs).
    invoke<string>("app_logs", { id: current, lines: LINES }).then(
      (s) => current === id && (text = s.trim()),
      (e) => {
        if (current !== id) return;
        const f = errorText(e);
        error = f.message ?? f.details;
      },
    );
  });
</script>

<div class="card">
  <div class="flex items-center justify-between gap-3">
    <h2 class="label-caps">{t("panel.detail.recent_logs")}</h2>
    <button type="button" class="btn min-h-8 px-2.5 text-xs" onclick={onopen}>{t("panel.detail.full_logs")}</button>
  </div>
  {#if error}
    <p class="text-sm text-error" role="alert">{error}</p>
  {:else}
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
    <pre
      tabindex="0"
      class="min-h-24 overflow-x-auto rounded-lg border border-border-soft bg-console px-3.5 py-3 font-mono text-xs leading-relaxed text-console-text focus-visible:outline-2 focus-visible:outline-link">{text ===
      null
        ? t("panel.logs.loading")
        : text || t("panel.logs.empty")}</pre>
  {/if}
  <p class="text-xs text-muted">{t("panel.detail.logs_note")}</p>
</div>
