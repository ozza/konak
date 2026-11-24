<script lang="ts">
  import { t } from "$lib/i18n";
  import { appName, formatBytes, hostOf, type AppInfo, type HostInfo, type Op } from "./api";
  import AppTile from "./AppTile.svelte";
  import Icon from "./Icon.svelte";
  import Meter from "./Meter.svelte";
  import { glyph, tone } from "./StatusBadge.svelte";

  let {
    apps,
    selected,
    host,
    busy,
    onselect,
  }: { apps: AppInfo[] | null; selected: string; host: HostInfo | null; busy: Record<string, Op>; onselect: (id: string) => void } = $props();

  let query = $state("");
  let shown = $derived((apps ?? []).filter((a) => `${appName(a.id)} ${a.name}`.toLowerCase().includes(query.trim().toLowerCase())));
  let hostOk = $derived(host ? host.distro.running && host.network.firewall_ok && host.network.konak_owns_port80 : null);
  const searchId = $props.id();

  const item = (on: boolean) =>
    `flex min-h-[52px] w-full items-center gap-3 rounded-lg border px-2.5 py-2 text-left focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-link ${
      on ? "border-accent/40 bg-accent/12" : "border-transparent hover:bg-raised"
    }`;
</script>

<nav aria-label={t("panel.sidebar.label")} class="flex flex-[1_1_260px] flex-col gap-1 border-r border-border bg-chrome px-3 py-3.5">
  <label for={searchId} class="sr-only">{t("panel.sidebar.search")}</label>
  <div class="relative mb-2.5">
    <span class="pointer-events-none absolute top-1/2 left-3 -translate-y-1/2 text-muted"><Icon name="search" /></span>
    <input
      id={searchId}
      type="search"
      bind:value={query}
      placeholder={t("panel.sidebar.search")}
      class="h-[38px] w-full rounded-lg border border-control bg-surface pr-3 pl-9 text-[13px] placeholder:text-muted focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-link"
    />
  </div>

  <p class="label-caps px-2.5 pt-1.5 pb-1">{t("panel.sidebar.host")}</p>
  <button type="button" class={item(selected === "host")} aria-current={selected === "host" ? "page" : undefined} onclick={() => onselect("host")}>
    <span class="inline-flex size-9 shrink-0 items-center justify-center rounded-lg bg-icon-tile text-icon-ink"><Icon name="pc" size={18} /></span>
    <span class="flex min-w-0 flex-1 flex-col">
      <span class="text-sm font-semibold">{t("panel.host.title")}</span>
      <span class="text-xs text-muted">{t("panel.sidebar.host_sub")}</span>
    </span>
    {#if hostOk !== null}
      <span class="w-3.5 text-center text-xs {hostOk ? 'text-ok' : 'text-warn'}">
        <span aria-hidden="true">{hostOk ? "●" : "!"}</span><span class="sr-only">{hostOk ? t("panel.sidebar.host_ok") : t("panel.sidebar.host_attention")}</span>
      </span>
    {/if}
  </button>

  <p class="label-caps px-2.5 pt-3.5 pb-1">{t("panel.sidebar.apps", { n: apps?.length ?? 0 })}</p>
  {#if apps === null}
    <p class="px-2.5 text-sm text-muted" role="status">{t("panel.loading")}</p>
  {:else if apps.length === 0}
    <p class="px-2.5 text-sm text-muted">{t("panel.empty")}</p>
  {:else if shown.length === 0}
    <p class="px-2.5 text-sm text-muted">{t("panel.sidebar.no_match")}</p>
  {/if}
  <ul class="flex flex-col gap-1">
    {#each shown as app (app.id)}
      <li>
        <button type="button" class={item(selected === app.id)} aria-current={selected === app.id ? "page" : undefined} onclick={() => onselect(app.id)}>
          <AppTile id={app.id} />
          <span class="flex min-w-0 flex-1 flex-col">
            <span class="text-sm font-semibold">{appName(app.id)}</span>
            <span class="truncate font-mono text-xs text-muted">
              {#if busy[app.id]}{t(`panel.busy.${busy[app.id]}`)}{:else if app.claimed}{hostOf(app.url)}{:else}{t("panel.sidebar.setup_needed")}{/if}
            </span>
          </span>
          <span class="w-3.5 text-center text-xs font-bold {app.claimed ? tone[app.status] : 'text-warn'}">
            <span aria-hidden="true">{app.claimed ? glyph[app.status] : "!"}</span><span class="sr-only">{t(`panel.status.${app.status}`)}</span>
          </span>
        </button>
      </li>
    {/each}
  </ul>

  <div class="min-h-4 flex-1"></div>
  {#if host?.memory || host?.disk}
    <div class="flex flex-col gap-2 border-t border-border px-2.5 pt-3 text-xs">
      {#if host.memory}
        <Meter label={t("panel.host.memory")} value="{formatBytes(host.memory.used_bytes)} / {formatBytes(host.memory.total_bytes)}" fraction={host.memory.used_bytes / host.memory.total_bytes} />
      {/if}
      {#if host.disk}
        <Meter label={t("panel.host.disk")} value="{formatBytes(host.disk.used_bytes)} / {formatBytes(host.disk.total_bytes)}" fraction={host.disk.used_bytes / host.disk.total_bytes} />
      {/if}
    </div>
  {/if}
</nav>
