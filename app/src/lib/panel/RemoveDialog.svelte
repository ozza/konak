<script lang="ts">
  import { t } from "$lib/i18n";
  import type { AppInfo } from "./api";
  import Dialog from "./Dialog.svelte";

  let { app, name, onclose, onconfirm }: { app: AppInfo | null; name: string; onclose: () => void; onconfirm: (deleteData: boolean) => void } =
    $props();

  const id = $props.id();
  let deleteData = $state(false);

  // Every time the dialog opens, data is kept unless the user ticks the box again.
  $effect(() => {
    if (app) deleteData = false;
  });
</script>

<Dialog open={!!app} title={t("panel.remove.title", { name })} {onclose}>
  <p class="text-sm">{t("panel.remove.body")}</p>
  <label class="flex items-start gap-2 text-sm">
    <input type="checkbox" bind:checked={deleteData} class="mt-0.5 h-4 w-4 shrink-0 accent-error" aria-describedby="{id}-note" />
    {t("panel.remove.delete_label")}
  </label>
  {#if deleteData}
    <p id="{id}-note" class="rounded-md border border-error-border bg-error-bg p-3 text-sm" role="alert">
      <span aria-hidden="true" class="mr-1">⚠</span>{t("panel.remove.delete_warning")}
    </p>
  {:else}
    <p id="{id}-note" class="text-sm text-muted">{t("panel.remove.keep_note")}</p>
  {/if}
  {#snippet actions()}
    <!-- svelte-ignore a11y_autofocus -->
    <button type="button" class="btn" autofocus onclick={onclose}>{t("panel.remove.cancel")}</button>
    <button type="button" class="btn-danger" onclick={() => onconfirm(deleteData)}>
      {deleteData ? t("panel.remove.confirm_delete") : t("panel.remove.confirm")}
    </button>
  {/snippet}
</Dialog>
