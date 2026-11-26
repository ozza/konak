<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { t } from "$lib/i18n";
  import { errorText, type AppInfo, type ErrorText } from "./api";
  import Dialog from "./Dialog.svelte";

  const LINES = 200;
  let { app, name, onclose }: { app: AppInfo | null; name: string; onclose: () => void } = $props();

  let text = $state<string | null>(null);
  let error = $state<ErrorText | null>(null);
  let loading = $state(false);
  let pre = $state<HTMLPreElement>();

  // Replies can arrive out of order (each goes through WSL); only the latest request may write.
  let seq = 0;

  async function load(id: string) {
    const mine = ++seq;
    loading = true;
    error = null;
    try {
      // Already redacted by the backend (apps::logs).
      const reply = await invoke<string>("app_logs", { id, lines: LINES });
      if (mine !== seq) return;
      text = reply;
      queueMicrotask(() => pre && (pre.scrollTop = pre.scrollHeight));
    } catch (e) {
      if (mine === seq) error = errorText(e);
    } finally {
      if (mine === seq) loading = false;
    }
  }

  $effect(() => {
    if (app) {
      text = null;
      load(app.id);
    } else {
      seq++;
    }
  });
</script>

<Dialog open={!!app} drawer title={t("panel.logs.title", { name })} {onclose}>
  <p class="text-xs text-muted">{t("panel.logs.note", { n: LINES })}</p>
  {#if error}
    <p class="text-sm text-error" role="alert">{error.message ?? error.details}</p>
  {/if}
  {#if text === null}
    <p class="text-sm text-muted" role="status">{t("panel.logs.loading")}</p>
  {:else}
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
    <pre
      bind:this={pre}
      tabindex="0"
      class="min-h-0 flex-1 overflow-auto rounded-md border border-border bg-surface p-3 font-mono text-xs leading-relaxed break-words whitespace-pre-wrap focus-visible:outline-2 focus-visible:outline-accent">{text.trim() ||
        t("panel.logs.empty")}</pre>
  {/if}
  {#snippet actions()}
    <button type="button" class="btn" disabled={loading || !app} onclick={() => app && load(app.id)} aria-busy={loading}>{t("panel.logs.refresh")}</button>
    <!-- svelte-ignore a11y_autofocus -->
    <button type="button" class="btn-primary" autofocus onclick={onclose}>{t("panel.logs.close")}</button>
  {/snippet}
</Dialog>
