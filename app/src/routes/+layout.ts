// Tauri doesn't have a Node.js server to do proper SSR
// so we use adapter-static with a fallback to index.html to put the site in SPA mode
// See: https://svelte.dev/docs/kit/single-page-apps
// See: https://v2.tauri.app/start/frontend/sveltekit/ for more info
export const ssr = false;

import { applyTheme, loadTheme } from "$lib/theme";

export async function load() {
  applyTheme(loadTheme());
  // `npm run dev` in a plain browser: a mocked backend. Compiled out of production builds.
  if (import.meta.env.DEV && !("__TAURI_INTERNALS__" in window)) (await import("$lib/dev/mock")).installMock();
}
