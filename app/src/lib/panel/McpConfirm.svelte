<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount } from "svelte";
  import { t } from "$lib/i18n";
  import { appName } from "./api";
  import Dialog from "./Dialog.svelte";
  import Icon from "./Icon.svelte";

  // A request from an AI agent through Konak's MCP server. Nothing runs until the user approves this
  // exact request; Escape, closing the dialog and the backend's timeout all deny it.
  type Request = {
    id: string;
    requester: string | null;
    op: "install" | "remove" | "start" | "stop" | "restart" | "rename" | "claim";
    app: string;
    name: string | null;
    delete_data: boolean;
    expires_unix: number;
  };

  let pending = $state<Request | null>(null);
  let now = $state(Date.now());
  let label = $derived(pending ? appName(pending.app) : "");
  let destructive = $derived(pending?.op === "remove" && pending.delete_data);
  let effectKey = $derived(destructive ? "remove_delete" : pending?.op);
  let left = $derived(pending ? Math.max(0, Math.round(pending.expires_unix - now / 1000)) : 0);
  let countdown = $derived(`${Math.floor(left / 60)}:${String(left % 60).padStart(2, "0")}`);

  /** Sends the answer once; the dialog closing afterwards finds no pending request and does nothing. */
  function answer(approve: boolean) {
    if (!pending) return;
    const id = pending.id;
    pending = null;
    invoke("mcp_answer", { id, approve }).catch((e) => console.warn("mcp_answer", e));
  }

  onMount(() => {
    // The window may have been closed when the request arrived; ask for it once on start.
    invoke<Request | null>("mcp_pending").then((r) => r && (pending = r));
    const offOpen = listen<Request>("mcp-confirm", ({ payload }) => (pending = payload));
    const offClose = listen<{ id: string }>("mcp-confirm-closed", ({ payload }) => {
      if (pending?.id === payload.id) pending = null;
    });
    const timer = setInterval(() => (now = Date.now()), 1000);
    return () => {
      clearInterval(timer);
      offOpen.then((f) => f());
      offClose.then((f) => f());
    };
  });
</script>

<Dialog open={!!pending} title={pending ? t(`panel.mcp.title.${pending.op}`, { app: label }) : ""} onclose={() => answer(false)}>
  {#if pending}
    <p class="flex items-start gap-2 text-sm">
      <span class="mt-0.5 text-link"><Icon name="pulse" /></span>
      <span>
        {t("panel.mcp.intro")}
        {#if pending.requester}{t("panel.mcp.requester", { requester: pending.requester })}{/if}
      </span>
    </p>
    <p
      class="rounded-lg border p-3 text-sm {destructive ? 'border-error-border bg-error-bg' : 'border-border bg-raised'}"
      role={destructive ? "alert" : undefined}
    >
      {#if destructive}<span aria-hidden="true" class="mr-1">⚠</span>{/if}{t(`panel.mcp.effect.${effectKey}`, { app: label, name: pending.name ?? "" })}
    </p>
    <p class="text-sm font-medium">{t("panel.mcp.check")}</p>
    <p class="text-xs text-muted" aria-live="off">{t("panel.mcp.countdown", { time: countdown })}</p>
  {/if}
  {#snippet actions()}
    <!-- Deny takes the initial focus, so a stray Enter never approves. -->
    <!-- svelte-ignore a11y_autofocus -->
    <button type="button" class="btn" autofocus onclick={() => answer(false)}>{t("panel.mcp.deny")}</button>
    <button type="button" class={destructive ? "btn-danger" : "btn-primary"} onclick={() => answer(true)}>{t("panel.mcp.approve")}</button>
  {/snippet}
</Dialog>
