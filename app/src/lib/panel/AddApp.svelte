<script lang="ts">
  import { t } from "$lib/i18n";
  import { appDescription, appName, CATEGORY, formatBytes, type AppInfo, type Failure, type HostInfo, type Manifest, type Op } from "./api";
  import AppTile from "./AppTile.svelte";
  import FailureNote from "./FailureNote.svelte";
  import Icon from "./Icon.svelte";

  let {
    catalog,
    apps,
    busy,
    failed,
    host,
    networkCategory,
    oninstall,
    onopen,
    ondismiss,
  }: {
    catalog: Manifest[];
    apps: AppInfo[];
    busy: Record<string, Op>;
    failed: Record<string, Failure>;
    host: HostInfo | null;
    networkCategory: string | null;
    oninstall: (id: string) => void;
    onopen: (id: string) => void;
    ondismiss: (id: string) => void;
  } = $props();

  const categories = ["all", ...new Set(Object.values(CATEGORY))];
  let filter = $state("all");
  let picked = $state<string | null>(null);

  const isInstalled = (id: string) => apps.some((a) => a.id === id);
  let shown = $derived(catalog.filter((m) => filter === "all" || CATEGORY[m.id] === filter));
  // Until the user picks one, the first app that is not installed yet.
  let pick = $derived(catalog.find((m) => m.id === picked) ?? catalog.find((m) => !isInstalled(m.id)) ?? catalog[0]);

  type Req = { ok: boolean | null; title: string; detail: string };
  let reqs = $derived.by((): Req[] => {
    if (!pick) return [];
    const need = pick.min_ram_mb * 1_048_576;
    const total = host?.memory?.total_bytes ?? null;
    const list: Req[] = [
      {
        ok: total === null ? null : total >= need,
        title: t("panel.add.req_memory", { amount: formatBytes(need) }),
        detail: total === null ? t("panel.add.req_memory_unknown") : total >= need ? t("panel.add.req_memory_ok", { total: formatBytes(total) }) : t("panel.add.req_memory_low", { total: formatBytes(total) }),
      },
      {
        ok: networkCategory === null ? null : networkCategory !== "Public",
        title: t("panel.add.req_network"),
        detail: networkCategory === "Public" ? t("panel.add.req_network_public") : t("panel.add.req_network_ok"),
      },
    ];
    if (pick.https) list.push({ ok: true, title: t("panel.add.req_https"), detail: t("panel.add.req_https_detail") });
    return list;
  });

  const reqTone = (ok: boolean | null) => (ok === null ? "bg-muted/15 text-muted" : ok ? "bg-ok-bg text-ok" : "bg-warn/15 text-warn");
</script>

<section aria-labelledby="add-title" class="mx-auto flex w-full max-w-[1320px] flex-col gap-5">
  <div class="flex flex-col gap-1.5">
    <h1 id="add-title" class="text-[26px] font-bold tracking-tight">{t("panel.add.title")}</h1>
    <p class="max-w-[640px] text-muted">{t("panel.add.intro")}</p>
  </div>

  <div role="group" aria-label={t("panel.add.filter")} class="flex flex-wrap gap-2">
    {#each categories as c (c)}
      <button
        type="button"
        aria-pressed={filter === c}
        onclick={() => (filter = c)}
        class="inline-flex min-h-8 items-center rounded-full border px-3 text-xs font-medium focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-link {filter === c
          ? 'border-text bg-text text-surface'
          : 'border-control text-text-2 hover:bg-raised'}">{t(`panel.category.${c}`)}</button
      >
    {/each}
  </div>

  <div class="flex flex-wrap items-start gap-6">
    <ul class="grid min-w-0 flex-[999_1_560px] grid-cols-[repeat(auto-fill,minmax(250px,1fr))] gap-4">
      {#each shown as m (m.id)}
        <li class="flex">
          <button
            type="button"
            aria-pressed={pick?.id === m.id}
            onclick={() => (picked = m.id)}
            class="flex w-full min-w-0 flex-col gap-3 rounded-xl border bg-panel p-[18px] text-left shadow-card hover:border-control-hover focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-link {pick?.id === m.id
              ? 'border-accent ring-1 ring-accent'
              : 'border-border'}"
          >
            <span class="flex w-full items-center gap-3">
              <AppTile id={m.id} />
              <span class="flex min-w-0 flex-1 flex-col">
                <span class="text-[15px] font-semibold">{appName(m.id)}</span>
                <span class="text-xs text-muted">{t(`panel.category.${CATEGORY[m.id] ?? "all"}`)}</span>
              </span>
              {#if isInstalled(m.id)}
                <span class="rounded-full bg-ok-bg px-2 py-0.5 text-[11px] font-semibold text-ok">{t("panel.add.installed")}</span>
              {:else if busy[m.id] === "install"}
                <span class="rounded-full bg-accent/15 px-2 py-0.5 text-[11px] font-semibold text-link">{t("panel.busy.install")}</span>
              {/if}
            </span>
            <span class="min-h-[38px] text-[13px] text-text-2">{appDescription(m.id) ?? ""}</span>
            <span class="flex flex-wrap gap-1.5 text-xs text-text-2">
              <span class="rounded-md bg-raised px-2 py-0.5">{t("panel.add.needs", { amount: formatBytes(m.min_ram_mb * 1_048_576) })}</span>
              {#if m.https}<span class="rounded-md bg-raised px-2 py-0.5">HTTPS</span>{/if}
            </span>
          </button>
        </li>
      {/each}
    </ul>

    {#if pick}
      <aside aria-labelledby="install-title" class="flex min-w-0 flex-[1_1_340px] flex-col gap-[18px] rounded-2xl border border-control bg-chrome p-5 shadow-card">
        <div class="flex items-center gap-3.5">
          <AppTile id={pick.id} size="lg" />
          <div class="flex min-w-0 flex-col">
            <p class="label-caps">{t("panel.add.install_label")}</p>
            <h2 id="install-title" class="text-xl font-bold">{appName(pick.id)}</h2>
          </div>
        </div>

        {#if failed[pick.id]}
          <FailureNote failure={failed[pick.id]} ondismiss={() => ondismiss(pick.id)} />
        {/if}

        {#if isInstalled(pick.id)}
          <p class="rounded-[10px] bg-raised px-3.5 py-3 text-[13px] text-text-2">{t("panel.add.already", { name: appName(pick.id) })}</p>
          <button type="button" class="btn" onclick={() => onopen(pick.id)}>{t("panel.add.go_to_app")}</button>
        {:else if busy[pick.id] === "install"}
          <div role="status" class="flex flex-col gap-3">
            <p class="text-[13px]">{t("install.running")}</p>
            <div class="h-1.5 overflow-hidden rounded-full bg-border"><div class="h-full w-1/3 animate-pulse rounded-full bg-accent"></div></div>
            <p class="text-xs text-muted">{t("panel.add.install_wait")}</p>
          </div>
        {:else}
          <dl class="flex flex-col gap-1">
            <dt class="text-[13px] font-medium">{t("panel.rename.label")}</dt>
            <dd class="font-mono text-[13px]">{pick.default_name}.local</dd>
            <dd class="text-xs text-muted">{t("panel.add.name_later")}</dd>
          </dl>
          <div class="flex flex-col">
            <h3 class="label-caps mb-1.5">{t("panel.add.before")}</h3>
            <ul>
              {#each reqs as r (r.title)}
                <li class="flex items-start gap-2.5 border-b border-border-soft py-2 last:border-0">
                  <span class="mt-px inline-flex size-5 shrink-0 items-center justify-center rounded-full {reqTone(r.ok)}">
                    <Icon name={r.ok === false ? "warn" : "check"} size={12} stroke={3} />
                  </span>
                  <span class="flex min-w-0 flex-col">
                    <span class="text-[13px] font-medium">{r.title}</span>
                    <span class="text-xs text-muted">{r.detail}</span>
                  </span>
                </li>
              {/each}
            </ul>
          </div>
          <button type="button" class="btn-primary min-h-11 text-sm font-semibold" disabled={!!busy[pick.id]}
            onclick={() => {
              // Keep this app in the panel after it stops being "the first one not installed".
              picked = pick.id;
              oninstall(pick.id);
            }}
            >{t("panel.catalog.install_label", { name: appName(pick.id) })}</button
          >
        {/if}
      </aside>
    {/if}
  </div>
</section>
