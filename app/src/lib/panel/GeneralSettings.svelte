<script lang="ts">
  import { t } from "$lib/i18n";
  import { applyTheme, loadTheme, THEMES, type Theme } from "$lib/theme";
  import type { Language, Settings } from "./api";

  // Theme lives in this browser profile (localStorage); language and autostart are Konak settings.
  let { settings, onsave }: { settings: Settings; onsave: (next: Settings) => Promise<boolean> } = $props();

  let theme = $state<Theme>(loadTheme());
  $effect(() => applyTheme(theme));

  async function setLanguage(language: Language) {
    // The new language applies on reload, so every text in the window switches at once.
    if (!(await onsave({ ...settings, language }))) return;
    try {
      sessionStorage.setItem("konak.returnView", "settings");
    } catch {
      // Without storage the window reopens on its default page; the language still switches.
    }
    location.reload();
  }
</script>

<section aria-labelledby="general-title" class="card gap-5">
  <h2 id="general-title" class="text-base font-semibold">{t("panel.settings.general")}</h2>

  <fieldset class="flex flex-col gap-1.5">
    <legend class="mb-1.5 text-sm font-medium">{t("panel.header.theme")}</legend>
    <div class="flex flex-wrap gap-2">
      {#each THEMES as option (option)}
        <label class="flex min-h-10 cursor-pointer items-center gap-2 rounded-lg border border-control px-3 text-sm has-[:checked]:border-accent has-[:checked]:bg-accent/10">
          <input type="radio" name="theme" value={option} bind:group={theme} class="size-4 accent-accent" />
          {t(`panel.header.theme_${option}`)}
        </label>
      {/each}
    </div>
  </fieldset>

  <div class="flex flex-col gap-1.5">
    <label for="language" class="text-sm font-medium">{t("panel.settings.language")}</label>
    <select
      id="language"
      value={settings.language}
      onchange={(e) => setLanguage((e.currentTarget as HTMLSelectElement).value as Language)}
      class="h-10 w-full max-w-xs rounded-lg border border-control bg-surface px-3 text-sm focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-link"
    >
      <option value="system">{t("panel.settings.language_system")}</option>
      <option value="tr">Türkçe</option>
      <option value="en">English</option>
    </select>
    <p class="text-xs text-muted">{t("panel.settings.language_hint")}</p>
  </div>

  <label class="flex items-start gap-3 text-sm">
    <input
      type="checkbox"
      checked={settings.autostart}
      onchange={(e) => onsave({ ...settings, autostart: (e.currentTarget as HTMLInputElement).checked })}
      class="mt-0.5 size-4 shrink-0 accent-accent"
    />
    <span class="flex flex-col gap-0.5">
      <span class="font-medium">{t("panel.settings.autostart")}</span>
      <span class="text-xs text-muted">{t("panel.settings.autostart_hint")}</span>
    </span>
  </label>
</section>
