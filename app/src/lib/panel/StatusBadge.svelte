<script lang="ts">
  import { t } from "$lib/i18n";
  import type { AppStatus } from "./api";

  let { status }: { status: AppStatus } = $props();

  // A glyph plus the text, so the status never relies on color alone.
  const glyph = { running: "●", starting: "◐", unhealthy: "!", stopped: "■", missing: "✕", unknown: "?" };
  const tone = {
    running: "text-ok border-ok/40",
    starting: "text-accent border-accent/40",
    unhealthy: "text-error border-error/40",
    stopped: "text-muted border-border",
    missing: "text-error border-error/40",
    unknown: "text-muted border-border",
  };
</script>

<span class="inline-flex shrink-0 items-center gap-1.5 rounded-full border px-2 py-0.5 text-xs font-medium {tone[status]}">
  <span aria-hidden="true">{glyph[status]}</span>{t(`panel.status.${status}`)}
</span>
