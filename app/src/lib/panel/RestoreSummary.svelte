<script lang="ts">
  import { has, t } from "$lib/i18n";
  import { appName, errorText, type BackupProgress, type FolderResult } from "./api";

  // What a finished restore brought back, renamed, skipped or could not restore.
  let { summary }: { summary: NonNullable<BackupProgress["summary"]> } = $props();

  const message = (e: string) => errorText(e).message ?? errorText(e).details;
  const note = (code: string) => (has(`panel.restore.note.${code}`) ? t(`panel.restore.note.${code}`) : code);
  const folder = (r: FolderResult) => t("panel.restore.folder_result", { written: r.written, identical: r.identical, kept: r.kept_both, failed: r.failed });
</script>

<div class="flex flex-col gap-2 text-sm">
  {#if summary.restored.length}
    <ul class="flex flex-col gap-1">
      {#each summary.restored as r (r.id)}
        <li><span class="text-ok" aria-hidden="true">● </span>{appName(r.id)} · <span class="font-mono text-xs">{r.name}.local</span>
          {#if r.renamed_from}<span class="block pl-4 text-xs text-muted">{t("panel.restore.renamed", { from: r.renamed_from, to: r.name })}</span>{/if}
        </li>
      {/each}
    </ul>
  {/if}
  {#if summary.failed.length}
    <ul class="flex flex-col gap-1">
      {#each summary.failed as f (f.id)}
        <li><span class="text-error" aria-hidden="true">✕ </span>{appName(f.id)}: <span class="text-error">{message(f.error)}</span></li>
      {/each}
    </ul>
  {/if}
  {#if summary.photos}<p>{t("panel.restore.photos_done")}: {folder(summary.photos)}</p>{/if}
  {#if summary.media}<p>{t("panel.restore.media_done")}: {folder(summary.media)}</p>{/if}
  {#each summary.notes as n (n)}<p class="text-xs text-muted">{note(n)}</p>{/each}
</div>
