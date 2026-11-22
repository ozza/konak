<script lang="ts">
  import { t } from "$lib/i18n";
  import { appDescription, appName, type Failure, type Manifest, type Op } from "./api";
  import FailureNote from "./FailureNote.svelte";

  let {
    items,
    busy,
    failed,
    oninstall,
    ondismiss,
  }: {
    items: Manifest[];
    busy: Record<string, Op>;
    failed: Record<string, Failure>;
    oninstall: (id: string) => void;
    ondismiss: (id: string) => void;
  } = $props();

  // 256 -> "256 MB", 1536 -> "1.5 GB"
  const memory = (mb: number) => (mb < 1024 ? `${mb} MB` : `${Math.round((mb / 1024) * 10) / 10} GB`);
</script>

<section aria-labelledby="catalog-title" class="flex flex-col gap-3">
  <h2 id="catalog-title" class="text-lg font-semibold">{t("panel.catalog.title")}</h2>
  {#if items.length === 0}
    <p class="text-sm text-muted">{t("panel.catalog.empty")}</p>
  {:else}
    <ul class="grid grid-cols-1 gap-3 md:grid-cols-2">
      {#each items as m (m.id)}
        {@const name = appName(m.id)}
        {@const description = appDescription(m.id)}
        <li class="flex flex-col gap-2 rounded-md border border-border bg-panel p-4" aria-busy={busy[m.id] ? "true" : undefined}>
          <h3 class="font-semibold">{name}</h3>
          {#if description}<p class="text-sm">{description}</p>{/if}
          <p class="text-xs text-muted">{t("panel.catalog.ram", { amount: memory(m.min_ram_mb) })}</p>
          {#if m.https}
            <p class="text-xs text-muted">{t("panel.catalog.https")}</p>
          {/if}
          {#if failed[m.id]}
            <FailureNote failure={failed[m.id]} ondismiss={() => ondismiss(m.id)} />
          {/if}
          <div class="mt-auto pt-1">
            {#if busy[m.id]}
              <p class="text-sm text-accent" role="status">{t(`panel.busy.${busy[m.id]}`)}</p>
            {:else}
              <button type="button" class="btn-primary" aria-label={t("panel.catalog.install_label", { name })} onclick={() => oninstall(m.id)}
                >{t("panel.catalog.install")}</button
              >
            {/if}
          </div>
        </li>
      {/each}
    </ul>
  {/if}
</section>
