<script lang="ts">
  import type { Snippet } from "svelte";

  // Modal <dialog>: showModal() makes the page behind inert, Escape closes it, Tab wraps inside it,
  // and focus goes back to the opener on close. `drawer` docks it to the right edge. While
  // `dismissable` is false (work is running that the dialog reports on), Escape and a click on the
  // backdrop do not close it.
  let {
    open,
    title,
    onclose,
    drawer = false,
    dismissable = true,
    children,
    actions,
  }: { open: boolean; title: string; onclose: () => void; drawer?: boolean; dismissable?: boolean; children: Snippet; actions?: Snippet } = $props();

  const titleId = $props.id();
  let el: HTMLDialogElement;
  let opener: HTMLElement | null = null;

  $effect(() => {
    if (open && !el.open) {
      opener = document.activeElement as HTMLElement | null;
      el.showModal();
    } else if (!open && el.open) {
      el.close();
    }
  });

  function closed() {
    // A `close` event is queued as its own task; if the dialog was reopened for something else
    // before it ran, it belongs to the previous content and must not close (or deny) the new one.
    if (el.open) return;
    // WebView2 may close a modal on a repeated Escape even when `cancel` is prevented; show it again.
    if (open && !dismissable) return el.showModal();
    onclose();
    opener?.focus();
  }

  function trapTab(e: KeyboardEvent) {
    if (e.key !== "Tab") return;
    const items = [...el.querySelectorAll<HTMLElement>("button:not([disabled]), [href], input:not([disabled]), [tabindex]:not([tabindex='-1'])")];
    if (items.length === 0) return;
    const [first, last] = [items[0], items[items.length - 1]];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  }
</script>

<dialog
  bind:this={el}
  aria-labelledby={titleId}
  onclose={closed}
  oncancel={(e) => !dismissable && e.preventDefault()}
  onkeydown={trapTab}
  onclick={(e) => e.target === el && dismissable && el.close()}
  class="border border-border bg-panel text-text backdrop:bg-black/50 {drawer
    ? 'fixed inset-y-0 right-0 left-auto m-0 h-dvh max-h-dvh w-full max-w-2xl'
    : 'm-auto w-[calc(100%-2rem)] max-w-md rounded-md'}"
>
  <div class="flex h-full flex-col gap-4 p-5">
    <h2 id={titleId} class="text-lg font-semibold break-words">{title}</h2>
    <div class="flex min-h-0 flex-1 flex-col gap-3">{@render children()}</div>
    {#if actions}<div class="flex flex-wrap justify-end gap-2">{@render actions()}</div>{/if}
  </div>
</dialog>
