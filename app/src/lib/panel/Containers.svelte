<script lang="ts">
  import { t } from "$lib/i18n";
  import type { AppInfo } from "./api";

  let { containers }: { containers: AppInfo["containers"] } = $props();

  const stateTone = (s: string) => (s === "running" ? "text-ok" : s === "exited" || s === "stopped" ? "text-muted" : "text-warn");
  const healthTone = (h: string | null) => (h === "healthy" ? "text-ok" : h === "unhealthy" ? "text-error" : h === "starting" ? "text-warn" : "text-muted");
</script>

<div class="card">
  <h2 class="label-caps">{t("panel.detail.containers")}</h2>
  {#if containers.length === 0}
    <p class="text-[13px] text-muted">{t("panel.detail.no_containers")}</p>
  {:else}
    <div class="overflow-x-auto">
      <table class="w-full border-collapse text-[13px]">
        <thead>
          <tr class="border-b border-border text-left text-xs text-muted">
            <th scope="col" class="px-3 py-2 font-semibold">{t("panel.detail.col_name")}</th>
            <th scope="col" class="px-3 py-2 font-semibold">{t("panel.detail.col_state")}</th>
            <th scope="col" class="px-3 py-2 font-semibold">{t("panel.detail.col_health")}</th>
          </tr>
        </thead>
        <tbody>
          {#each containers as c (c.name)}
            <tr class="border-b border-border-soft last:border-0">
              <td class="px-3 py-2.5 font-mono">{c.name}</td>
              <td class="px-3 py-2.5 {stateTone(c.state)}"><span class="inline-flex items-center gap-1.5"><span aria-hidden="true">●</span>{c.state}</span></td>
              <td class="px-3 py-2.5 {healthTone(c.health)}">{c.health ?? "—"}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</div>
