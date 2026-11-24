<script lang="ts" module>
  // Stroke icons on a 24-unit grid, drawn in currentColor. Always decorative: the control next to
  // or around the icon carries the text or aria-label.
  const PATHS = {
    home: "M3 11l9-7 9 7M5 10v10h14V10M10 20v-6h4v6",
    pc: "M5 4h14a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2zM8 20h8M12 16v4",
    external: "M14 4h6v6M20 4l-9 9M19 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1h5",
    plus: "M12 5v14M5 12h14",
    settings: "M4 6h9M17 6h3M4 12h3M11 12h9M4 18h11M19 18h1M13 6a2 2 0 1 0 4 0a2 2 0 1 0-4 0M7 12a2 2 0 1 0 4 0a2 2 0 1 0-4 0M15 18a2 2 0 1 0 4 0a2 2 0 1 0-4 0",
    search: "M4 11a7 7 0 1 0 14 0a7 7 0 1 0-14 0M20 20l-4-4",
    stop: "M7.5 6h9A1.5 1.5 0 0 1 18 7.5v9a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 6 16.5v-9A1.5 1.5 0 0 1 7.5 6z",
    play: "M8 5v14l11-7z",
    restart: "M20 11a8 8 0 1 0-2.3 5.7M20 4v7h-7",
    logs: "M4 6h16M4 12h16M4 18h10",
    rename: "M4 20h4L19 9l-4-4L4 16z",
    trash: "M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13",
    pulse: "M3 12h4l3-7 4 14 3-7h4",
    lock: "M7 11h10a2 2 0 0 1 2 2v5a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2v-5a2 2 0 0 1 2-2zM8 11V8a4 4 0 0 1 8 0v3",
    check: "M5 12l5 5 9-10",
    warn: "M12 9v4M12 17h.01M10.3 3.9L2.4 18a2 2 0 0 0 1.7 3h15.8a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z",
    x: "M6 6l12 12M18 6L6 18",
    back: "M15 18l-6-6 6-6",
  };
  export type IconName = keyof typeof PATHS;
</script>

<script lang="ts">
  let { name, size = 16, stroke = 2 }: { name: IconName; size?: number; stroke?: number } = $props();
</script>

<svg
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width={stroke}
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
  class="shrink-0"><path d={PATHS[name]} /></svg
>
