// Shapes of the Tauri commands the panel uses (see src-tauri/src/lib.rs and apps.rs).
import { t, has } from "$lib/i18n";

export type AppStatus = "running" | "starting" | "unhealthy" | "stopped" | "missing" | "unknown";
export type AppInfo = {
  id: string;
  name: string;
  /** The `.local` URL, served on the network only once the app is claimed. */
  url: string;
  /** http://127.0.0.1:<port>, reachable from this PC only; used to finish setup before claiming. */
  local_url: string;
  https: boolean;
  port: number;
  installed_unix: number;
  claimed: boolean;
  /** Stopped by the user; stays stopped across restarts until started again. */
  stopped: boolean;
  status: AppStatus;
  containers: { name: string; state: string; health: string | null }[];
};
export type Manifest = { id: string; default_name: string; port: number; https: boolean; min_ram_mb: number };
export type CaInfo = { path: string; fingerprint: string };
export type Op = "install" | "remove" | "rename" | "restart" | "claim" | "stop" | "start";
export type AppStats = { id: string; cpu_percent: number; mem_bytes: number };
type Usage = { used_bytes: number; total_bytes: number };
export type HostInfo = {
  /** The supervisor runs (fresh heartbeat with keepalive and Caddy up): diagnostics are meaningful. */
  ready?: boolean;
  distro: { name: string; running: boolean };
  /** Inside Konak's distro; null while it is not running. */
  memory: Usage | null;
  disk: Usage | null;
  data_drive: { free_bytes: number; total_bytes: number } | null;
  network: { lan_ip: string | null; adapter: string | null; category: string | null; firewall_ok: boolean; konak_owns_port80: boolean };
};
/** `code` and `args` pick the panel's text from the locale files; `evidence` and `fix` are technical English. */
export type Check = { check: string; status: "ok" | "warn" | "fail"; evidence: string; fix: string; code: string; args?: Record<string, string> };
export type AppProgress = { id: string; op: Op; state: "running" | "done" | "failed"; error: string | null };
/** A backend error: `message` when it is a known error code, otherwise `details` holds technical text. */
export type ErrorText = { message: string | null; details: string | null };
export type Failure = ErrorText & { op: Op };

/** Ports Caddy serves on. `http` is not 80 while another program holds port 80 (`port80_owner`). */
export type WebPorts = { http: number; https: number; port80_owner: string | null };
/** Used until the backend reports the home page URL, which carries the port on the fallback. */
export const DEFAULT_HOME_URL = "http://konak.local";
export const certUrl = (home: string) => `${home}/konak-root.crt`;
export const hostOf = (url: string) => url.replace(/^https?:\/\//, "");
/** ":18080" while Caddy is on its fallback port, "" on port 80. */
export const portSuffix = (ports: WebPorts | null) => (ports && ports.http !== 80 ? `:${ports.http}` : "");

/** Display name from the locale files, falling back to the id for apps without strings yet. */
export function appName(id: string): string {
  const key = `apps.${id}.name`;
  const text = t(key);
  return text === key ? id : text;
}

export function appDescription(id: string): string | null {
  const key = `apps.${id}.description`;
  const text = t(key);
  return text === key ? null : text;
}

/** Catalog category, the key under `panel.category`. */
export const CATEGORY: Record<string, string> = {
  immich: "photos",
  jellyfin: "media",
  nextcloud: "files",
  vaultwarden: "passwords",
  forgejo: "developer",
};

/** Tile color behind an app's initial; white text passes 4.5:1 on each. */
const TILE: Record<string, string> = {
  immich: "#7c3aed",
  jellyfin: "#0e7490",
  nextcloud: "#1d4ed8",
  vaultwarden: "#475569",
  forgejo: "#c2410c",
};
export const tileColor = (id: string) => TILE[id] ?? "#475569";

/** 1536 MiB -> "1.5 GB", 300 MiB -> "300 MB" (binary units, shown with the usual short names). */
export function formatBytes(bytes: number): string {
  const mb = bytes / 1_048_576;
  if (mb < 1024) return `${Math.round(mb)} MB`;
  const gb = mb / 1024;
  return `${gb < 10 ? Math.round(gb * 10) / 10 : Math.round(gb)} GB`;
}

/** Problems for This PC: diagnostics that did not pass, plus missing firewall rules (no diagnostic covers
 *  them). Null until either is known. */
export function hostProblems(host: HostInfo | null, checks: Check[] | null): number | null {
  if (!host && !checks) return null;
  return (checks?.filter((c) => c.status !== "ok").length ?? 0) + (host && !host.network.firewall_ok ? 1 : 0);
}

export function errorText(error: unknown): ErrorText {
  const e = String(error);
  return has(`errors.${e}`) ? { message: t(`errors.${e}`), details: null } : { message: null, details: e };
}

// Same rule as catalog::valid_label, used by apps::validate_name: a lower-case DNS label.
const LABEL = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;

/** Local checks of apps::validate_name; returns an error code or null. */
export function nameError(name: string, id: string, apps: AppInfo[]): string | null {
  if (!LABEL.test(name)) return "invalid_name";
  if (name === "konak") return "name_reserved";
  if (apps.some((a) => a.id !== id && a.name === name)) return "name_in_use";
  return null;
}
