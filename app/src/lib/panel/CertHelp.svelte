<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { encode } from "uqr";
  import { t } from "$lib/i18n";
  import { CERT_URL, errorText, type CaInfo, type ErrorText } from "./api";
  import Icon from "./Icon.svelte";

  let ca = $state<CaInfo | null>(null);
  let caError = $state<ErrorText | null>(null);
  let trust = $state<"idle" | "waiting" | "done" | "failed">("idle");
  let trustError = $state<string | null>(null);

  $effect(() => {
    invoke<CaInfo>("ca_info").then(
      (c) => (ca = c),
      (e) => (caError = errorText(e)),
    );
  });

  // One SVG path of dark modules; dark on white in both themes so every phone camera reads it.
  const qr = encode(CERT_URL, { ecc: "M", border: 2 });
  const qrPath = qr.data.flatMap((row, y) => row.map((dark, x) => (dark ? `M${x} ${y}h1v1h-1z` : ""))).join("");

  const oses = { ios: 3, android: 3, windows: 2, macos: 3 };

  async function trustHere() {
    trust = "waiting";
    trustError = null;
    try {
      await invoke("trust_ca_on_this_pc");
      trust = "done";
    } catch (e) {
      trust = "failed";
      trustError = String(e);
    }
  }
</script>

<section aria-labelledby="cert-title" class="card gap-4">
  <h2 id="cert-title" class="flex items-center gap-2 text-base font-semibold"><Icon name="lock" />{t("panel.cert.title")}</h2>
  <p class="text-sm">{t("panel.cert.what")}</p>

  <div class="flex flex-col gap-4 sm:flex-row sm:items-start">
    <svg
      viewBox="0 0 {qr.size} {qr.size}"
      role="img"
      aria-label={t("panel.cert.qr_label", { url: CERT_URL })}
      class="h-36 w-36 shrink-0 rounded-sm"
      shape-rendering="crispEdges"
    >
      <rect width={qr.size} height={qr.size} class="fill-white" />
      <path d={qrPath} class="fill-black" />
    </svg>
    <div class="flex min-w-0 flex-col gap-3">
      <div>
        <p class="text-sm">{t("panel.cert.download")}</p>
        <p class="mt-1 font-mono text-sm break-all select-all">{CERT_URL}</p>
      </div>
      <div class="rounded-md border-2 border-warn-border bg-warn-bg p-3">
        <p class="text-sm font-semibold">{t("panel.cert.fingerprint")}</p>
        {#if ca}
          <!-- Line breaks only after a colon, never inside a byte. -->
          <p class="mt-1 font-mono text-sm leading-relaxed font-semibold tracking-wide select-all">
            {#each ca.fingerprint.split(":") as byte, i (i)}{i ? ":" : ""}<wbr />{byte}{/each}
          </p>
        {:else if caError}
          <p class="mt-1 text-sm text-error" role="alert">{caError.message ?? caError.details}</p>
        {:else}
          <p class="mt-1 text-sm text-muted" role="status">{t("panel.cert.loading")}</p>
        {/if}
        <p class="mt-2 text-sm">{t("panel.cert.fingerprint_check")}</p>
      </div>
    </div>
  </div>

  <div>
    <h3 class="font-semibold">{t("panel.cert.steps_title")}</h3>
    <div class="mt-2 flex flex-col divide-y divide-border rounded-md border border-border">
      {#each Object.entries(oses) as [os, count] (os)}
        <details class="group px-3 py-2">
          <summary class="cursor-pointer rounded text-sm font-medium focus-visible:outline-2 focus-visible:outline-accent">{t(`panel.cert.${os}`)}</summary>
          <ol class="mt-2 mb-1 ml-5 list-decimal space-y-1 text-sm">
            {#each { length: count }, i (i)}
              <li>{t(`panel.cert.${os}_${i + 1}`)}</li>
            {/each}
          </ol>
        </details>
      {/each}
    </div>
  </div>

  <div>
    <h3 class="font-semibold">{t("panel.cert.this_pc_title")}</h3>
    <p class="mt-1 text-sm">{t("panel.cert.this_pc_body")}</p>
    <div class="mt-2 flex flex-wrap items-center gap-3">
      <button type="button" class="btn" disabled={trust === "waiting" || !ca} onclick={trustHere}>{t("panel.cert.trust")}</button>
      <p class="text-sm" role="status">
        {#if trust === "waiting"}{t("panel.cert.trusting")}{:else if trust === "done"}<span class="text-ok">✓ {t("panel.cert.trusted")}</span>{/if}
      </p>
    </div>
    {#if trust === "failed"}
      <p class="mt-2 text-sm text-error" role="alert">{t("panel.cert.trust_failed")} <span class="text-xs break-all">{trustError}</span></p>
    {/if}
  </div>
</section>
