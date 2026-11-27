<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount, tick } from "svelte";
  import { t } from "$lib/i18n";
  import { appName, errorText, portSuffix, type AppInfo, type AppProgress, type AppStats, type Check, type Failure, type HostInfo, type Manifest, type Op, type WebPorts } from "./api";
  import AddApp from "./AddApp.svelte";
  import AppDetail from "./AppDetail.svelte";
  import Header from "./Header.svelte";
  import HostDetail from "./HostDetail.svelte";
  import SettingsPage from "./SettingsPage.svelte";
  import LogsDrawer from "./LogsDrawer.svelte";
  import McpConfirm from "./McpConfirm.svelte";
  import Notices from "./Notices.svelte";
  import RemoveDialog from "./RemoveDialog.svelte";
  import Sidebar from "./Sidebar.svelte";

  const POLL_MS = 5000;
  const HOST_POLL_MS = 15000;
  /** The first diagnose waits for the supervisor (`host.ready`); this timeout is only a fallback. */
  const FIRST_DIAG_MS = 60000;
  /** Later runs refresh the results while the window is visible. */
  const DIAG_REFRESH_MS = 300000;
  let diagReady = $state(false);
  type Summary = { minutes: number; uac_prompts: number; reboots: number; download_mb: number };
  let {
    networkCategory,
    firewallMissing,
    repairing,
    repairError,
    summary,
    homeUrl,
    webPorts,
  }: {
    networkCategory: string | null;
    firewallMissing: boolean;
    repairing: boolean;
    repairError: string | null;
    summary: Summary | null;
    homeUrl: string;
    webPorts: WebPorts | null;
  } = $props();

  let apps = $state<AppInfo[] | null>(null);
  let catalog = $state<Manifest[]>([]);
  let stats = $state<Record<string, AppStats>>({});
  let host = $state<HostInfo | null>(null);
  let checks = $state<Check[] | null>(null);
  let diagnosing = $state(false);
  let diagError = $state<string | null>(null);
  let busy = $state<Record<string, Op>>({});
  let failed = $state<Record<string, Failure>>({});
  /** Bumped per app when an operation finishes, so its log preview reloads. */
  let versions = $state<Record<string, number>>({});
  /** "host", "add" or an installed app's id. */
  let view = $state<string | null>(takeReturnView());

  /** A view to reopen after a reload the panel started itself (the language switch). */
  function takeReturnView(): string | null {
    try {
      const v = sessionStorage.getItem("konak.returnView");
      sessionStorage.removeItem("konak.returnView");
      return v;
    } catch {
      return null;
    }
  }
  let logsFor = $state<AppInfo | null>(null);
  let removeFor = $state<AppInfo | null>(null);

  let showCert = $derived(catalog.some((m) => m.https) || (apps?.some((a) => a.https) ?? false));
  // Nothing picked yet, or the picked app was removed: the first app, else This PC.
  let shown = $derived(
    view === "add" || view === "host" || view === "settings" || apps?.some((a) => a.id === view) ? view : apps === null ? null : (apps[0]?.id ?? "host"),
  );
  let current = $derived(apps?.find((a) => a.id === shown) ?? null);

  async function refresh() {
    const [list, s] = await Promise.all([invoke<AppInfo[]>("list_apps"), invoke<AppStats[]>("app_stats").catch(() => [] as AppStats[])]);
    apps = list;
    stats = Object.fromEntries(s.map((x) => [x.id, x]));
  }

  async function refreshHost() {
    host = await invoke<HostInfo>("host_info").catch(() => host);
  }

  async function diagnose() {
    diagnosing = true;
    diagError = null;
    try {
      checks = await invoke<Check[]>("diagnose");
    } catch (e) {
      const f = errorText(e);
      diagError = f.message ?? f.details;
    } finally {
      diagnosing = false;
    }
  }

  function patch<T>(map: Record<string, T>, id: string, value: T | null): Record<string, T> {
    const { [id]: _, ...rest } = map;
    return value === null ? rest : { ...rest, [id]: value };
  }

  /** Starts an operation; its progress and result arrive as `app-progress` events. */
  async function run(id: string, op: Op, args: Record<string, unknown> = {}) {
    failed = patch(failed, id, null);
    try {
      await invoke(`${op}_app`, { id, ...args });
    } catch (e) {
      failed = patch(failed, id, { op, ...errorText(e) });
    }
  }

  let mainEl = $state<HTMLElement>();
  /** The app the Add app page shows after an AI agent's install was approved. */
  let addFocus = $state<string | null>(null);

  async function select(id: string) {
    view = id;
    // In the stacked (narrow) layout the detail sits below the sidebar: bring it into view.
    await tick();
    if (mainEl && mainEl.getBoundingClientRect().top > window.innerHeight / 2) mainEl.scrollIntoView({ block: "start" });
  }

  onMount(() => {
    invoke<Manifest[]>("list_catalog").then((c) => (catalog = c));
    refresh();
    refreshHost();
    // The sidebar shows This PC's health from the start, not only once its page is opened. The first
    // run waits for the supervisor's first ticks (keepalive, Caddy), or it reports them as down.
    const diagTimer = setTimeout(() => (diagReady = true), FIRST_DIAG_MS);
    const diagRefresh = setInterval(() => visible() && checks !== null && !diagnosing && diagnose(), DIAG_REFRESH_MS);
    // Network category and firewall rules can change while Konak is not running; both reads are cheap.
    invoke("refresh_network");
    // Poll only while the window is visible; the tray keeps Konak running when it is hidden.
    const visible = () => document.visibilityState === "visible";
    const timer = setInterval(() => visible() && refresh(), POLL_MS);
    const hostTimer = setInterval(() => visible() && refreshHost(), HOST_POLL_MS);
    const onVisible = () => visible() && (refresh(), refreshHost());
    document.addEventListener("visibilitychange", onVisible);
    const unlisten = listen<AppProgress>("app-progress", ({ payload: p }) => {
      busy = patch(busy, p.id, p.state === "running" ? p.op : null);
      if (p.state === "failed") failed = patch(failed, p.id, { op: p.op, ...errorText(p.error) });
      if (p.state !== "running") {
        versions = { ...versions, [p.id]: (versions[p.id] ?? 0) + 1 };
        refresh();
      }
    });
    // After the user approves an AI agent's request, show where its progress appears: the install
    // panel for an install, the app's page for everything else.
    const unlistenApproved = listen<{ id: string; op: Op; app: string }>("mcp-approved", ({ payload: p }) => {
      addFocus = p.op === "install" ? p.app : null;
      select(p.op === "install" ? "add" : p.app);
    });
    return () => {
      clearTimeout(diagTimer);
      clearInterval(diagRefresh);
      clearInterval(timer);
      clearInterval(hostTimer);
      document.removeEventListener("visibilitychange", onVisible);
      unlisten.then((f) => f());
      unlistenApproved.then((f) => f());
    };
  });

  $effect(() => {
    if ((diagReady || host?.ready) && checks === null && !diagnosing && !diagError) diagnose();
  });
</script>

<div class="flex min-h-dvh flex-col">
  <Header
    engineRunning={host ? host.distro.running : null}
    {networkCategory}
    {homeUrl}
    settingsOpen={shown === "settings"}
    onadd={() => select("add")}
    onsettings={() => select("settings")}
  />

  <div class="flex flex-1 flex-wrap items-stretch">
    <Sidebar {apps} selected={shown ?? ""} {host} {checks} {busy} onselect={select} />

    <main bind:this={mainEl} class="flex min-w-0 flex-[999_1_480px] flex-col gap-5 px-4 pt-6 pb-8 sm:px-7">
      <Notices {networkCategory} {firewallMissing} {repairing} {repairError} {webPorts} />

      {#if shown === "add"}
        <AddApp
          {catalog}
          apps={apps ?? []}
          {busy}
          {failed}
          {host}
          {networkCategory}
          portSuffix={portSuffix(webPorts)}
          focus={addFocus}
          onfocused={() => (addFocus = null)}
          oninstall={(id) => run(id, "install")}
          onopen={select}
          ondismiss={(id) => (failed = patch(failed, id, null))}
        />
      {:else if shown === "settings"}
        <SettingsPage apps={apps ?? []} />
      {:else if shown === "host"}
        <HostDetail {host} {checks} {diagnosing} {diagError} appCount={apps?.length ?? 0} {showCert} {summary} {homeUrl} ondiagnose={diagnose} />
      {:else if current && apps}
        <AppDetail
          app={current}
          {apps}
          stats={stats[current.id]}
          busy={busy[current.id]}
          failed={failed[current.id]}
          version={versions[current.id] ?? 0}
          onop={(op) => run(current.id, op)}
          onlogs={() => (logsFor = current)}
          onremove={() => (removeFor = current)}
          ondismiss={() => (failed = patch(failed, current.id, null))}
        />
      {:else}
        <p class="text-sm text-muted" role="status">{t("panel.loading")}</p>
      {/if}
    </main>
  </div>
</div>

<McpConfirm />
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
