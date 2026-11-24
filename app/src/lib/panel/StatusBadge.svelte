<script lang="ts" module>
  import type { AppStatus } from "./api";

  // A glyph plus the text, so the status never relies on color alone.
  export const glyph: Record<AppStatus, string> = { running: "●", starting: "◐", unhealthy: "!", stopped: "■", missing: "✕", unknown: "?" };
  export const tone: Record<AppStatus, string> = {
    running: "text-ok",
    starting: "text-warn",
    unhealthy: "text-error",
    stopped: "text-muted",
    missing: "text-error",
    unknown: "text-muted",
  };
  const bg: Record<AppStatus, string> = {
    running: "bg-ok-bg",
    starting: "bg-warn/15",
    unhealthy: "bg-error/15",
    stopped: "bg-muted/15",
    missing: "bg-error/15",
    unknown: "bg-muted/15",
  };
</script>

<script lang="ts">
  import { t } from "$lib/i18n";

  let { status }: { status: AppStatus } = $props();
</script>

<span class="inline-flex shrink-0 items-center gap-1.5 rounded-full px-2.5 py-0.5 text-xs font-semibold {tone[status]} {bg[status]}">
  <span aria-hidden="true">{glyph[status]}</span>{t(`panel.status.${status}`)}
</span>
