// Dev-only stand-in for the Tauri backend, so the UI can be viewed in a plain browser with `npm run dev`.
// Loaded from +layout.ts only when import.meta.env.DEV is true and the page is not inside Tauri; the
// production build drops it. URL flags: ?setup (first-run flow), ?public (Public network notice),
// ?empty (no apps installed). Forgejo starts unclaimed; newly installed apps are unclaimed too. Rename to "taken" to see the LAN conflict error.
import { emit } from "@tauri-apps/api/event";
import { mockIPC } from "@tauri-apps/api/mocks";
import type { AppInfo, Manifest, Op } from "$lib/panel/api";

const q = new URLSearchParams(location.search);
const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

const catalog: Manifest[] = [
  { id: "jellyfin", default_name: "jellyfin", port: 47101, https: false, min_ram_mb: 2048 },
  { id: "immich", default_name: "photos", port: 47102, https: false, min_ram_mb: 4096 },
  { id: "vaultwarden", default_name: "vault", port: 47103, https: true, min_ram_mb: 256 },
  { id: "nextcloud", default_name: "cloud", port: 47104, https: false, min_ram_mb: 1536 },
  { id: "forgejo", default_name: "git", port: 47105, https: false, min_ram_mb: 1024 },
];

function info(id: string, name: string, status: AppInfo["status"], claimed = true): AppInfo {
  const m = catalog.find((c) => c.id === id)!;
  const url = `${m.https ? "https" : "http"}://${name}.local`;
  const local_url = `http://127.0.0.1:${m.port}`;
  return { id, name, url, local_url, https: m.https, port: m.port, installed_unix: 1763800000, claimed, status, containers: [] };
}

let apps: AppInfo[] = q.has("empty")
  ? []
  : [info("jellyfin", "movies", "running"), info("nextcloud", "cloud", "starting"), info("forgejo", "git", "running", false), info("immich", "photos", "stopped")];

const logs = [
  "jellyfin  | [10:41:02] [INF] Jellyfin version: 10.11.2",
  "jellyfin  | [10:41:03] [INF] Startup complete 0:00:04.21",
  "jellyfin  | [10:41:03] [INF] Kestrel is listening on http://0.0.0.0:8096",
  "jellyfin  | [10:42:17] [INF] Scanning library /media: 1,284 items",
  "jellyfin  | [10:42:19] [WRN] Slow query detected in library scan (412 ms)",
  "jellyfin  | [10:43:00] [INF] API_KEY=[redacted] loaded from environment",
].join("\n");

let firewallMissing = q.has("firewall");
let repairing = false;
const steps = ["preflight", "admin", "reboot", "distro", "systemd", "engine", "network"];
const status = () => ({
  steps: steps.map((id) => ({ id, state: q.has("setup") ? "pending" : "done" })),
  running: false,
  error: null,
  reboot_needed: false,
  done: !q.has("setup"),
  url: "http://konak.local",
  network_category: q.has("public") ? "Public" : "Private",
  firewall_missing: firewallMissing,
  repairing,
  repair_error: null,
  summary: q.has("setup") ? null : { minutes: 11.4, uac_prompts: 1, reboots: 1, download_mb: 612 },
});

function op(id: string, name: Op, work: () => string | null, ms = 1500) {
  emit("app-progress", { id, op: name, state: "running", error: null });
  wait(ms).then(() => {
    const error = work();
    emit("app-progress", { id, op: name, state: error ? "failed" : "done", error });
  });
}

export function installMock() {
  mockIPC(
    async (cmd, args) => {
      const a = (args ?? {}) as Record<string, any>;
      switch (cmd) {
        case "get_status":
          return status();
        case "refresh_network":
          emit("status", status());
          return null;
        case "repair_network":
          repairing = true;
          emit("status", status());
          await wait(1500);
          repairing = false;
          firewallMissing = false;
          emit("status", status());
          return null;
        case "list_catalog":
          return catalog;
        case "list_apps":
          return apps;
        case "app_logs":
          await wait(300);
          return logs;
        case "ca_info":
          return {
            path: "C:\\Users\\dev\\AppData\\Local\\Konak\\ca\\root.crt",
            fingerprint: "3A:F1:9C:42:7B:E0:55:1D:C8:0A:64:9F:2E:B7:13:D6:8C:41:F0:A9:5E:72:3B:C6:0D:94:E8:1F:A3:57:26:BD",
          };
        case "trust_ca_on_this_pc":
          await wait(800);
          return null;
        case "install_app":
          return op(a.id, "install", () => {
            const m = catalog.find((c) => c.id === a.id)!;
            apps = [...apps, info(a.id, m.default_name, "starting", false)];
            return null;
          });
        case "restart_app":
          return op(a.id, "restart", () => null);
        case "claim_app":
          return op(a.id, "claim", () => ((apps = apps.map((x) => (x.id === a.id ? { ...x, claimed: true } : x))), null));
        case "remove_app":
          return op(a.id, "remove", () => ((apps = apps.filter((x) => x.id !== a.id)), null));
        case "rename_app":
          return op(a.id, "rename", () => {
            if (a.name === "taken") return "name_taken_on_lan";
            apps = apps.map((x) => (x.id === a.id ? info(x.id, a.name, x.status, x.claimed) : x));
            return null;
          });
        case "plugin:opener|open_url":
          console.info("[mock] open", a.url);
          return null;
        default:
          console.warn("[mock] unhandled command", cmd, a);
          return null;
      }
    },
    { shouldMockEvents: true },
  );
}
