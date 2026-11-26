<script lang="ts">
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { t } from "$lib/i18n";
  import { applyTheme, loadTheme, THEMES, type Theme } from "$lib/theme";
  import { hostOf } from "./api";
  import Icon from "./Icon.svelte";

  let {
    engineRunning,
    networkCategory,
    homeUrl,
    onadd,
  }: { engineRunning: boolean | null; networkCategory: string | null; homeUrl: string; onadd: () => void } = $props();
  let homeHost = $derived(hostOf(homeUrl));

  let theme = $state<Theme>(loadTheme());
  let menuOpen = $state(false);
  let menu = $state<HTMLElement>();
  const menuId = $props.id();

  $effect(() => applyTheme(theme));

  let menuButton = $state<HTMLButtonElement>();

  function closeOutside(e: MouseEvent) {
    if (menuOpen && menu && !menu.contains(e.target as Node)) menuOpen = false;
  }

  // Escape returns focus to the settings button, so keyboard users do not land on the page start.
  function closeOnEscape(e: KeyboardEvent) {
    if (e.key !== "Escape" || !menuOpen) return;
    menuOpen = false;
    menuButton?.focus();
  }

  // Tabbing out of the menu closes it instead of leaving it open over the page.
  function closeOnFocusOut(e: FocusEvent) {
    if (menuOpen && menu && !menu.contains(e.relatedTarget as Node | null)) menuOpen = false;
  }
</script>

<svelte:window onclick={closeOutside} onkeydown={closeOnEscape} />

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
  <div class="relative" bind:this={menu} onfocusout={closeOnFocusOut}>
    <button type="button" class="btn px-2.5" bind:this={menuButton} aria-label={t("panel.header.settings")} aria-expanded={menuOpen} aria-controls={menuId} onclick={() => (menuOpen = !menuOpen)}>
      <Icon name="settings" />
    </button>
    {#if menuOpen}
      <div id={menuId} class="absolute top-full right-0 z-20 mt-2 w-56 rounded-xl border border-border bg-panel p-3 shadow-lg">
        <fieldset class="flex flex-col gap-1">
          <legend class="label-caps mb-2">{t("panel.header.theme")}</legend>
          {#each THEMES as option (option)}
            <label class="flex min-h-9 cursor-pointer items-center gap-2.5 rounded-md px-2 text-sm hover:bg-raised">
              <input type="radio" name="theme" value={option} bind:group={theme} class="size-4 accent-accent" />
              {t(`panel.header.theme_${option}`)}
            </label>
          {/each}
        </fieldset>
      </div>
    {/if}
  </div>
</header>
