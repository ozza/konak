<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount } from "svelte";
  import { t, has } from "$lib/i18n";
  import StepList from "$lib/StepList.svelte";
  import Panel from "$lib/panel/Panel.svelte";

  type Step = { id: string; state: "pending" | "running" | "done" | "skipped" | "failed" };
  type Status = {
    steps: Step[];
    running: boolean;
    error: string | null;
    reboot_needed: boolean;
    done: boolean;
    url: string | null;
    network_category: string | null;
    firewall_missing: boolean;
    repairing: boolean;
    repair_error: string | null;
    summary: { minutes: number; uac_prompts: number; reboots: number; download_mb: number } | null;
  };

  let status = $state<Status | null>(null);
  let setPrivate = $state(true);

  // Backend errors are either an i18n code (e.g. "uac_declined") or technical text shown as details.
  let errorMessage = $derived(status?.error && has(`errors.${status.error}`) ? t(`errors.${status.error}`) : null);
  let started = $derived(status?.steps.some((s) => s.state !== "pending") ?? false);

  onMount(() => {
    invoke<Status>("get_status").then((s) => (status = s));
    const unlisten = listen<Status>("status", (e) => (status = e.payload));
    return () => void unlisten.then((f) => f());
  });

  function install() {
    invoke("start_install", { setPrivate: status?.network_category === "Public" && setPrivate });
  }
</script>

{#if status?.done}
  <Panel
    networkCategory={status.network_category}
    firewallMissing={status.firewall_missing}
    repairing={status.repairing}
    repairError={status.repair_error}
    summary={status.summary}
  />
{:else}
<main class="mx-auto flex max-w-xl flex-col gap-5 px-6 py-6">
  <header>
    <h1 class="text-2xl font-semibold">Konak</h1>
    <p class="text-sm text-muted">{t("app.tagline")}</p>
  </header>

  {#if status}
    {#if status.reboot_needed}
      <section class="rounded-md border border-warn-border bg-warn-bg p-4" aria-labelledby="reboot-title">
        <h2 id="reboot-title" class="font-semibold">{t("reboot.title")}</h2>
        <p class="mt-1 text-sm">{t("reboot.body")}</p>
        <button
          class="mt-3 rounded-md bg-accent px-4 py-2 text-sm font-medium text-on-accent hover:bg-accent-strong focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
          onclick={() => invoke("restart_pc")}>{t("reboot.button")}</button
        >
      </section>
    {:else}
      <section class="rounded-md border border-border bg-panel p-4" aria-labelledby="install-title">
        <h2 id="install-title" class="text-lg font-semibold">{t("install.title")}</h2>
        <p class="mt-1 text-sm text-muted">{t("install.intro")}</p>

        {#if status.network_category === "Public" && !started}
          <div class="mt-3 rounded-md border border-warn-border bg-warn-bg p-3">
            <p class="text-sm font-semibold">{t("network.public_title")}</p>
            <p class="mt-1 text-sm">{t("network.public_body")}</p>
            <label class="mt-2 flex items-center gap-2 text-sm">
              <input type="checkbox" bind:checked={setPrivate} class="h-4 w-4 accent-accent" />
              {t("network.switch_label")}
            </label>
          </div>
        {/if}

        {#if status.running}
          <p class="mt-3 text-sm" role="status">{t("install.running")}</p>
        {:else}
          <button
            class="mt-3 rounded-md bg-accent px-4 py-2 text-sm font-medium text-on-accent hover:bg-accent-strong focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
            onclick={install}>{status.error ? t("install.retry") : t("install.button")}</button
          >
          {#if !started}<p class="mt-2 text-xs text-muted">{t("install.admin_note")}</p>{/if}
        {/if}
      </section>
    {/if}

    {#if status.error}
      <section class="rounded-md border border-error-border bg-error-bg p-4" role="alert">
        <h2 class="font-semibold text-error">{t("errors.title")}</h2>
        {#if errorMessage}
          <p class="mt-1 text-sm">{errorMessage}</p>
        {:else}
          <details class="mt-1 text-sm">
            <summary class="cursor-pointer">{t("errors.details")}</summary>
            <pre class="mt-2 text-xs whitespace-pre-wrap">{status.error}</pre>
          </details>
        {/if}
      </section>
    {/if}

    {#if started}
      <StepList steps={status.steps} />
    {/if}
  {/if}
</main>
{/if}
