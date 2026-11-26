<script lang="ts">
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { t } from "$lib/i18n";
  import { hostOf } from "./api";
  import Icon from "./Icon.svelte";

  let {
    engineRunning,
    networkCategory,
    homeUrl,
    settingsOpen,
    onadd,
    onsettings,
  }: {
    engineRunning: boolean | null;
    networkCategory: string | null;
    homeUrl: string;
    settingsOpen: boolean;
    onadd: () => void;
    onsettings: () => void;
  } = $props();
  let homeHost = $derived(hostOf(homeUrl));
</script>

<header class="flex flex-wrap items-center gap-x-4 gap-y-3 border-b border-border bg-chrome px-5 py-2.5">
  <div class="flex items-center gap-2.5">
    <span class="inline-flex size-[30px] items-center justify-center rounded-lg bg-accent text-on-accent"><Icon name="home" size={18} /></span>
    <span class="text-base font-bold tracking-tight">Konak</span>
  </div>
  {#if engineRunning !== null}
    <span class="inline-flex items-center gap-2 rounded-full border border-border bg-raised px-2.5 py-1 text-xs text-text-2">
      <span class={engineRunning ? "text-ok" : "text-error"} aria-hidden="true">●</span>
      {engineRunning ? t("panel.header.engine_running") : t("panel.header.engine_stopped")}
      {#if networkCategory}· {t(`panel.header.network_${networkCategory === "Public" ? "public" : "private"}`)}{/if}
    </span>
  {/if}
  <div class="flex-1"></div>
  <button type="button" class="btn" aria-label={t("panel.home_link_label", { host: homeHost })} onclick={() => openUrl(homeUrl)}>
    <span class="font-mono">{homeHost}</span><Icon name="external" size={14} />
  </button>
  <button type="button" class="btn-primary" onclick={onadd}><Icon name="plus" />{t("panel.header.add")}</button>
  <button
    type="button"
    class="btn px-2.5 {settingsOpen ? 'border-accent bg-accent/10' : ''}"
    aria-label={t("panel.header.settings")}
    aria-current={settingsOpen ? "page" : undefined}
    onclick={onsettings}
  >
    <Icon name="settings" />
  </button>
</header>
