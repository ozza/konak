<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { onMount } from "svelte";
  import { t } from "$lib/i18n";
  import { appName, errorText, HOME_URL, type AppInfo, type AppProgress, type Failure, type Manifest, type Op } from "./api";
  import AppCard from "./AppCard.svelte";
  import Catalog from "./Catalog.svelte";
  import CertHelp from "./CertHelp.svelte";
  import LogsDrawer from "./LogsDrawer.svelte";
  import RemoveDialog from "./RemoveDialog.svelte";

  const POLL_MS = 5000;
  let {
    networkCategory,
    firewallMissing,
    repairing,
    repairError,
  }: { networkCategory: string | null; firewallMissing: boolean; repairing: boolean; repairError: string | null } = $props();
  let repairFailure = $derived(repairError ? errorText(repairError) : null);

  let apps = $state<AppInfo[] | null>(null);
  let catalog = $state<Manifest[]>([]);
  let busy = $state<Record<string, Op>>({});
  let failed = $state<Record<string, Failure>>({});
  let logsFor = $state<AppInfo | null>(null);
  let removeFor = $state<AppInfo | null>(null);

  let available = $derived(catalog.filter((m) => !apps?.some((a) => a.id === m.id)));
  let showCert = $derived(catalog.some((m) => m.https) || (apps?.some((a) => a.https) ?? false));

  async function refresh() {
    apps = await invoke<AppInfo[]>("list_apps");
  }

  function setFailed(id: string, f: Failure | null) {
    const { [id]: _, ...rest } = failed;
    failed = f ? { ...rest, [id]: f } : rest;
  }

  function setBusy(id: string, op: Op | null) {
    const { [id]: _, ...rest } = busy;
    busy = op ? { ...rest, [id]: op } : rest;
  }

  /** Starts an operation; its progress and result arrive as `app-progress` events. */
  async function run(id: string, op: Op, args: Record<string, unknown> = {}) {
    setFailed(id, null);
    try {
      await invoke(`${op}_app`, { id, ...args });
    } catch (e) {
      setFailed(id, { op, ...errorText(e) });
    }
  }

  onMount(() => {
    invoke<Manifest[]>("list_catalog").then((c) => (catalog = c));
    refresh();
    // Network category and firewall rules can change while Konak is not running; both reads are cheap.
    invoke("refresh_network");
    // Poll only while the window is visible; the tray keeps Konak running when it is hidden.
    const timer = setInterval(() => document.visibilityState === "visible" && refresh(), POLL_MS);
    const onVisible = () => document.visibilityState === "visible" && refresh();
    document.addEventListener("visibilitychange", onVisible);
    const unlisten = listen<AppProgress>("app-progress", ({ payload: p }) => {
      setBusy(p.id, p.state === "running" ? p.op : null);
      if (p.state === "failed") setFailed(p.id, { op: p.op, ...errorText(p.error) });
      if (p.state !== "running") refresh();
    });
    return () => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisible);
      unlisten.then((f) => f());
    };
  });
</script>

<div class="mx-auto flex max-w-4xl flex-col gap-6 px-4 py-5 sm:px-6">
  <header class="flex flex-wrap items-center justify-between gap-3">
    <div>
      <h1 class="text-2xl font-semibold">Konak</h1>
      <p class="text-sm text-muted">{t("app.tagline")}</p>
    </div>
    <button type="button" class="btn" aria-label={t("panel.home_link_label")} onclick={() => openUrl(HOME_URL)}>
      <span class="font-mono">{t("panel.home_link")}</span><span aria-hidden="true">↗</span>
    </button>
  </header>

  {#if firewallMissing}
    <section class="flex flex-col gap-2 rounded-md border border-warn-border bg-warn-bg p-3 text-sm" aria-labelledby="repair-title">
      <h2 id="repair-title" class="font-semibold">{t("panel.repair.title")}</h2>
      <p>{t("panel.repair.body")}</p>
      {#if repairFailure}
        <div class="text-error" role="alert">
          <p class="font-medium">{t("panel.repair.failed")}</p>
          {#if repairFailure.message}
            <p>{repairFailure.message}</p>
          {:else}
            <details>
              <summary class="cursor-pointer">{t("errors.details")}</summary>
              <pre class="mt-2 text-xs whitespace-pre-wrap">{repairFailure.details}</pre>
            </details>
          {/if}
        </div>
      {/if}
      <div>
        <button type="button" class="btn-primary" disabled={repairing} onclick={() => invoke("repair_network")}>
          {repairing ? t("panel.repair.running") : t("panel.repair.button")}
        </button>
      </div>
      {#if repairing}<p class="sr-only" role="status">{t("panel.repair.running")}</p>{/if}
    </section>
  {/if}

  {#if networkCategory === "Public"}
    <p class="rounded-md border border-warn-border bg-warn-bg p-3 text-sm" role="status">{t("panel.network.public")}</p>
  {/if}

  <main class="flex flex-col gap-8">
    <section aria-labelledby="installed-title" class="flex flex-col gap-3">
      <h2 id="installed-title" class="text-lg font-semibold">{t("panel.installed_title")}</h2>
      {#if apps === null}
        <p class="text-sm text-muted" role="status">{t("panel.loading")}</p>
      {:else if apps.length === 0}
        <p class="rounded-md border border-dashed border-border p-4 text-sm text-muted">{t("panel.empty")}</p>
      {:else}
        <ul class="grid grid-cols-1 gap-3 md:grid-cols-2">
          {#each apps as app (app.id)}
            <AppCard
              {app}
              {apps}
              busy={busy[app.id]}
              failed={failed[app.id]}
              onrestart={() => run(app.id, "restart")}
              onclaim={() => run(app.id, "claim")}
              onlogs={() => (logsFor = app)}
              onremove={() => (removeFor = app)}
              ondismiss={() => setFailed(app.id, null)}
            />
          {/each}
        </ul>
      {/if}
    </section>

    <Catalog items={available} {busy} {failed} oninstall={(id) => run(id, "install")} ondismiss={(id) => setFailed(id, null)} />

    {#if showCert}<CertHelp />{/if}
  </main>
</div>

<LogsDrawer app={logsFor} name={logsFor ? appName(logsFor.id) : ""} onclose={() => (logsFor = null)} />
<RemoveDialog
  app={removeFor}
  name={removeFor ? appName(removeFor.id) : ""}
  onclose={() => (removeFor = null)}
  onconfirm={(deleteData) => {
    const id = removeFor!.id;
    removeFor = null;
    run(id, "remove", { deleteData });
  }}
/>
