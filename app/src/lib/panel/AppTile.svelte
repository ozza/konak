<script lang="ts">
  import { appName, tileColor } from "./api";

  // The app's initial on its own color; decorative, the name is always written next to it.
  let { id, size = "md" }: { id: string; size?: "md" | "lg" } = $props();
</script>

<span
  aria-hidden="true"
  class="inline-flex shrink-0 items-center justify-center font-bold text-white {size === 'lg' ? 'size-14 rounded-2xl text-2xl' : 'size-9 rounded-lg text-[15px]'}"
  style:background={tileColor(id)}>{appName(id).charAt(0).toUpperCase()}</span
>
