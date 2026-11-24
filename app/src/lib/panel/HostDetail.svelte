<script lang="ts">
  import { t, has } from "$lib/i18n";
  import { formatBytes, hostProblems, type Check, type HostInfo } from "./api";
  import CertHelp from "./CertHelp.svelte";
  import Icon from "./Icon.svelte";
  import Meter from "./Meter.svelte";

  type Summary = { minutes: number; uac_prompts: number; reboots: number; download_mb: number };
  let {
    host,
    checks,
    diagnosing,
    diagError,
    appCount,
    showCert,
    summary,
    homeUrl,
    ondiagnose,
  }: {
    host: HostInfo | null;
    checks: Check[] | null;
    diagnosing: boolean;
    diagError: string | null;
    appCount: number;
    showCert: boolean;
    summary: Summary | null;
    homeUrl: string;
    ondiagnose: () => void;
  } = $props();

  let problems = $derived(checks ? hostProblems(host, checks) : null);
  const checkTone = { ok: "bg-ok-bg text-ok", warn: "bg-warn/15 text-warn", fail: "bg-error/15 text-error" };
  const checkText = { ok: "text-ok", warn: "text-warn", fail: "text-error" };
  const checkIcon ={ ok: "check", warn: "warn", fail: "x" } as const;
  const checkName = (id: string) => (has(`panel.diag.${id}`) ? t(`panel.diag.${id}`) : id);
  const dash = "—";
</script>

<section aria-labelledby="host-title" class="flex flex-col gap-5">
  <div class="flex flex-wrap items-center gap-4">
    <span class="inline-flex size-14 shrink-0 items-center justify-center rounded-2xl bg-icon-tile text-icon-ink"><Icon name="pc" size={28} stroke={1.75} /></span>
    <div class="flex min-w-0 flex-[1_1_280px] flex-col gap-1">
      <div class="flex flex-wrap items-center gap-2.5">
        <h1 id="host-title" class="text-2xl font-bold tracking-tight">{t("panel.host.title")}</h1>
        {#if problems !== null}
          <span class="inline-flex items-center gap-1.5 rounded-full px-2.5 py-0.5 text-xs font-semibold {problems ? checkTone.warn : checkTone.ok}">
            <span aria-hidden="true">{problems ? "!" : "●"}</span>{problems ? t(problems === 1 ? "panel.host.problems_one" : "panel.host.problems", { n: problems }) : t("panel.host.all_ok")}
          </span>
        {/if}
      </div>
      <p class="text-[13px] text-muted">{t("panel.host.subtitle")}</p>
    </div>
  </div>

  <div role="toolbar" aria-label={t("panel.host.actions")} class="flex flex-wrap gap-2 rounded-[10px] border border-border bg-chrome p-2">
    <button type="button" class="btn-primary" disabled={diagnosing} onclick={ondiagnose}><Icon name="pulse" />{diagnosing ? t("panel.host.diagnosing") : t("panel.host.diagnose")}</button>
  </div>

  <div class="grid grid-cols-[repeat(auto-fit,minmax(260px,1fr))] gap-4">
    <div class="card">
      <h2 class="label-caps">{t("panel.host.engine")}</h2>
      <dl class="grid grid-cols-[max-content_minmax(0,1fr)] gap-x-[18px] gap-y-2 text-[13px]">
        <dt class="text-muted">{t("panel.host.environment")}</dt><dd class="font-mono break-all">{host?.distro.name ?? dash}</dd>
        <dt class="text-muted">{t("panel.detail.status")}</dt>
        <dd>
          {#if host}<span class="inline-flex items-center gap-1.5"
              ><span class={host.distro.running ? "text-ok" : "text-error"} aria-hidden="true">●</span>{host.distro.running ? t("panel.status.running") : t("panel.status.stopped")}</span
            >{:else}{dash}{/if}
        </dd>
        <dt class="text-muted">{t("panel.host.apps")}</dt><dd>{t("panel.host.apps_n", { n: appCount })}</dd>
      </dl>
    </div>
    <div class="card">
      <h2 class="label-caps">{t("panel.host.memory_disk")}</h2>
      {#if host?.memory}
        <Meter label={t("panel.host.memory")} value={t("panel.host.of", { used: formatBytes(host.memory.used_bytes), total: formatBytes(host.memory.total_bytes) })} fraction={host.memory.used_bytes / host.memory.total_bytes} />
      {/if}
      {#if host?.disk}
        <Meter label={t("panel.host.disk")} value={t("panel.host.of", { used: formatBytes(host.disk.used_bytes), total: formatBytes(host.disk.total_bytes) })} fraction={host.disk.used_bytes / host.disk.total_bytes} />
      {/if}
      {#if host?.data_drive}
        <Meter label={t("panel.host.data_drive")} value={t("panel.host.free", { free: formatBytes(host.data_drive.free_bytes) })} fraction={1 - host.data_drive.free_bytes / host.data_drive.total_bytes} />
      {/if}
      {#if host && !host.memory && !host.disk}<p class="text-[13px] text-muted">{t("panel.host.no_usage")}</p>{/if}
    </div>
    <div class="card">
      <h2 class="label-caps">{t("panel.detail.network")}</h2>
      <dl class="grid grid-cols-[max-content_minmax(0,1fr)] gap-x-[18px] gap-y-2 text-[13px]">
        <dt class="text-muted">{t("panel.host.profile")}</dt><dd>{host?.network.category ?? dash}</dd>
        <dt class="text-muted">{t("panel.host.adapter")}</dt><dd class="break-words">{host?.network.adapter ?? dash}</dd>
        <dt class="text-muted">{t("panel.host.lan_ip")}</dt><dd class="font-mono">{host?.network.lan_ip ?? dash}</dd>
        <dt class="text-muted">{t("panel.host.firewall")}</dt><dd>{host ? (host.network.firewall_ok ? t("panel.host.firewall_ok") : t("panel.host.firewall_missing")) : dash}</dd>
      </dl>
    </div>
  </div>

  <div class="card">
    <h2 class="label-caps">{t("panel.host.diagnostics")}</h2>
    {#if diagError}
      <p class="text-sm text-error" role="alert">{diagError}</p>
    {:else if checks === null}
      <p class="text-[13px] text-muted" role="status">{diagnosing ? t("panel.host.diagnosing") : t("panel.host.not_run")}</p>
    {:else}
      <ul class="flex flex-col">
        {#each checks as c (c.check)}
          <li class="flex flex-wrap items-start gap-x-3.5 gap-y-1 border-b border-border-soft py-3 last:border-0">
            <span class="inline-flex size-[22px] shrink-0 items-center justify-center rounded-full {checkTone[c.status]}"><Icon name={checkIcon[c.status]} size={13} stroke={3} /></span>
            <span class="min-w-0 flex-[1_1_220px] font-medium">{checkName(c.check)}</span>
            <span class="min-w-0 flex-[2_1_260px] text-[13px] break-words text-muted">
              {c.evidence}
              {#if c.status !== "ok" && c.fix}<span class="mt-1 block text-text-2">{c.fix}</span>{/if}
            </span>
            <span class="text-xs font-semibold {checkText[c.status]}">{t(`panel.host.check_${c.status}`)}</span>
          </li>
        {/each}
      </ul>
    {/if}
  </div>

  {#if showCert}<CertHelp {homeUrl} />{/if}

  {#if summary}
    <div class="card">
      <h2 class="label-caps">{t("summary.title")}</h2>
      <dl class="grid grid-cols-[max-content_minmax(0,1fr)] gap-x-[18px] gap-y-2 text-[13px]">
        <dt class="text-muted">{t("summary.time")}</dt><dd>{t("summary.minutes", { n: summary.minutes.toFixed(1) })}</dd>
        <dt class="text-muted">{t("summary.uac")}</dt><dd>{summary.uac_prompts}</dd>
        <dt class="text-muted">{t("summary.reboots")}</dt><dd>{summary.reboots}</dd>
        <dt class="text-muted">{t("summary.download")}</dt><dd>{t("summary.mb", { n: Math.round(summary.download_mb) })}</dd>
      </dl>
    </div>
  {/if}
  <p class="text-[13px] text-muted">{t("done.tray")}</p>
</section>
