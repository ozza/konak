<script lang="ts">
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { t } from "$lib/i18n";
  import type { AppInfo } from "./api";
  import Icon from "./Icon.svelte";

  // An unclaimed app is reachable from this PC only until its admin account exists.
  let { app, disabled, onclaim }: { app: AppInfo; disabled: boolean; onclaim: () => void } = $props();
</script>

<div class="flex flex-wrap items-center gap-4 rounded-xl border border-warn-border bg-warn-bg px-[18px] py-4">
  <span class="inline-flex size-9 shrink-0 items-center justify-center rounded-full bg-warn-icon text-warn-title"><Icon name="lock" size={18} /></span>
  <div class="flex min-w-0 flex-[1_1_320px] flex-col gap-0.5">
    <p class="font-semibold text-warn-title">{t("panel.claim.title")}</p>
    <p class="text-[13px] text-warn-text">{t("panel.claim.notice")} {t("panel.claim.local_only", { url: app.local_url })}</p>
  </div>
  <div class="flex flex-wrap gap-2">
    <button type="button" class="btn-primary" onclick={() => openUrl(app.local_url)}>{t("panel.claim.open_setup")}</button>
    <button type="button" class="btn" {disabled} onclick={onclaim}>{t("panel.claim.done")}</button>
  </div>
</div>
