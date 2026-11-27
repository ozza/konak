// Dev-only stand-in for the Tauri backend, so the UI can be viewed in a plain browser with `npm run dev`.
// Loaded from +layout.ts only when import.meta.env.DEV is true and the page is not inside Tauri; the
// production build drops it. URL flags: ?setup (first-run flow), ?public (Public network notice),
// ?empty (no apps installed), ?port80 (port 80 taken, fallback port), ?firewall (missing firewall rules), ?stale (a failing diagnostic), ?mcp=<op> (an AI agent request waiting for confirmation, e.g. ?mcp=remove). Forgejo starts unclaimed; newly installed apps are unclaimed too. Rename to "taken" to see the LAN conflict error.
import { emit } from "@tauri-apps/api/event";
import { mockIPC } from "@tauri-apps/api/mocks";
import type { AppInfo, Manifest, Op } from "$lib/panel/api";

const q = new URLSearchParams(location.search);
const port = q.has("port80") ? ":18080" : "";
const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

const catalog: Manifest[] = [
  { id: "jellyfin", default_name: "jellyfin", port: 47101, https: false, min_ram_mb: 2048 },
  { id: "immich", default_name: "photos", port: 47102, https: false, min_ram_mb: 4096 },
  { id: "vaultwarden", default_name: "vault", port: 47103, https: true, min_ram_mb: 256 },
  { id: "nextcloud", default_name: "cloud", port: 47104, https: false, min_ram_mb: 1536 },
  { id: "forgejo", default_name: "git", port: 47105, https: false, min_ram_mb: 1024 },
];

// Container names as compose shows them; health only where the image defines a check.
const containers: Record<string, AppInfo["containers"]> = {
  jellyfin: [{ name: "jellyfin-1", state: "running", health: "healthy" }],
  immich: [
    { name: "server-1", state: "exited", health: null },
    { name: "machine-learning-1", state: "exited", health: null },
    { name: "database-1", state: "exited", health: null },
    { name: "redis-1", state: "exited", health: null },
  ],
  nextcloud: [
    { name: "app-1", state: "running", health: "starting" },
    { name: "db-1", state: "running", health: "healthy" },
    { name: "redis-1", state: "running", health: null },
    { name: "cron-1", state: "running", health: null },
  ],
  forgejo: [{ name: "server-1", state: "running", health: "healthy" }],
};

const GB = 1024 ** 3;
const hostInfo = () => ({
  ready: true,
  distro: { name: "konak-podman", running: true },
  memory: { used_bytes: 7.4 * GB, total_bytes: 64 * GB },
  disk: { used_bytes: 118 * GB, total_bytes: 900 * GB },
  data_drive: { free_bytes: 412 * GB, total_bytes: 1863 * GB },
  network: { lan_ip: "192.168.1.20", adapter: "Ethernet", category: q.has("public") ? "Public" : "Private", firewall_ok: !firewallMissing, konak_owns_port80: true },
});
const checks = () => [
  { check: "forward_policy", status: "ok", evidence: "no FORWARD chain with a DROP policy", fix: "", code: "forward_policy.ok" },
  { check: "stale_netavark", status: q.has("stale") ? "fail" : "ok", evidence: q.has("stale") ? "DNAT jump for port 47104 points at a dead network" : "every netavark rule belongs to a live network", fix: "In Konak's distro as root, delete only that network's rules, then run `podman network reload --all`.", code: q.has("stale") ? "stale_netavark.foreign" : "stale_netavark.ok", args: q.has("stale") ? { ports: "47104" } : { containers: "6", open: "5" } },
  { check: "keepalive", status: "ok", evidence: "keepalive pid 1234 alive, distro konak-podman running", fix: "", code: "keepalive.ok" },
  { check: "port80_owner", status: "ok", evidence: "Konak's caddy (pid 4321) listens on 192.168.1.20:80", fix: "", code: "port80_owner.ok" },
  { check: "lan_adapter", status: "ok", evidence: "LAN IP 192.168.1.20 on Ethernet (Intel Ethernet)", fix: "", code: "lan_adapter.ok", args: { ip: "192.168.1.20", adapter: "Ethernet" } },
];

function info(id: string, name: string, status: AppInfo["status"], claimed = true): AppInfo {
  const m = catalog.find((c) => c.id === id)!;
  const url = m.https ? `https://${name}.local${port && ":18443"}` : `http://${name}.local${port}`;
  const local_url = `http://127.0.0.1:${m.port}`;
  return { id, name, url, local_url, https: m.https, port: m.port, installed_unix: 1763800000, claimed, stopped: status === "stopped", status, containers: status === "stopped" ? [] : (containers[id] ?? []) };
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
  url: `http://konak.local${port}`,
  web_ports: port ? { http: 18080, https: 18443, port80_owner: "httpd.exe (pid 4120)" } : { http: 80, https: 443, port80_owner: null },
  network_category: q.has("public") ? "Public" : "Private",
  firewall_missing: firewallMissing,
  repairing,
  repair_error: null,
  summary: q.has("setup") ? null : { minutes: 11.4, uac_prompts: 1, reboots: 1, download_mb: 612 },
});

let settings = {
  language: "system",
  autostart: true,
  backup: {
    location: q.has("nobackup") ? null : "D:\\Backups\\Konak",
    schedule: { kind: "daily", time: "03:00", weekday: 1 },
    keep: 7,
    include: { apps: [] as string[], photos: true, media: false },
    password_set: !q.has("nobackup"),
  },
};
const day = 86400;
let backups = [
  { file: "konak-20261125-030000.konakbackup", created_unix: 1795575600, size_bytes: 3.2 * GB, apps: ["jellyfin", "nextcloud", "forgejo"], status: "ok", error: null },
  { file: "konak-20261124-030000.konakbackup", created_unix: 1795575600 - day, size_bytes: 3.1 * GB, apps: ["jellyfin", "nextcloud"], status: "failed", error: "backup_no_space" },
];

function progress(event: string, steps: [string, string | null][], ms = 600) {
  steps.forEach(([step, app], i) =>
    setTimeout(() => emit(event, { state: "running", step, app, done_bytes: i, total_bytes: steps.length, error: null }), i * ms),
  );
  setTimeout(() => emit(event, { state: "done", step: "done", app: null, done_bytes: null, total_bytes: null, error: null }), steps.length * ms);
}

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
        case "app_stats":
          return apps
            .filter((x) => x.status !== "stopped")
            .map((x, i) => ({ id: x.id, cpu_percent: [2.1, 31.4, 0.8, 6.3][i % 4], mem_bytes: [620, 410, 180, 1900][i % 4] * 1_048_576 }));
        case "host_info":
          return hostInfo();
        case "diagnose":
          await wait(900);
          return checks();
        case "stop_app":
          return op(a.id, "stop", () => ((apps = apps.map((x) => (x.id === a.id ? { ...x, stopped: true, status: "stopped", containers: [] } : x))), null));
        case "start_app":
          return op(a.id, "start", () => ((apps = apps.map((x) => (x.id === a.id ? { ...x, stopped: false, status: "running" } : x))), null));
        case "mcp_pending": {
          const op = q.get("mcp");
          if (!op) return null;
          return { id: "req-1", requester: "claude.exe", op, app: "jellyfin", name: op === "rename" ? "films" : null, delete_data: q.has("delete"), expires_unix: Date.now() / 1000 + 300 };
        }
        case "mcp_answer":
          console.info("[mock] mcp answer", a.id, a.approve);
          return null;
        case "get_settings":
          return settings;
        case "save_settings":
          if (a.settings.backup.keep < 1 || a.settings.backup.keep > 100) throw "settings_keep";
          settings = { ...a.settings, backup: { ...a.settings.backup, password_set: settings.backup.password_set } };
          return settings;
        case "set_backup_password":
          await wait(300);
          if (settings.backup.password_set && a.current !== "correct horse battery") throw "password_current_wrong";
          settings = { ...settings, backup: { ...settings.backup, password_set: true } };
          return null;
        case "list_backups":
          return backups;
        case "backup_now":
          progress("backup-progress", [["prepare", null], ["app", "jellyfin"], ["app", "nextcloud"], ["photos", null], ["verify", null]]);
          setTimeout(() => (backups = [{ ...backups[0], file: "konak-new.konakbackup", created_unix: Date.now() / 1000 }, ...backups]), 3100);
          return null;
        case "inspect_backup":
          await wait(400);
          if (a.password && a.password !== "correct horse battery") throw "backup_wrong_password";
          return {
            format: 1,
            konak_version: "0.1.0",
            created_unix: 1795575600,
            apps: [
              { id: "jellyfin", name: "movies", claimed: true, stopped: false, size_bytes: 120 * 1_048_576 },
              { id: "vaultwarden", name: "vault", claimed: true, stopped: false, size_bytes: 3 * 1_048_576 },
            ],
            photos_bytes: 2.4 * GB,
            media_bytes: null,
            konak_files: ["ca/root.crt", "ca/root.key", "state.json", "settings.json"],
            ca_fingerprint: "3F:9A:12:C4:7B:E0:55:A1:9D:2C:44:6E:B8:01:7F:93:AA:5D:0C:E2:71:48:B6:3E:9F:22:D0:6A:85:1B:C7:E4",
            ca_valid: true,
          };
        case "restore_backup":
          progress("restore-progress", (a.apps as string[]).map((id) => ["restore_app", id] as [string, string | null]), 3000);
          return null;
        case "plugin:dialog|open":
          return a.options?.directory ? "E:\\KonakBackups" : "D:\\Backups\\Konak\\konak-20261125-030000.konakbackup";
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
