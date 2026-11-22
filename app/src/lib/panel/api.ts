// Shapes of the Tauri commands the panel uses (see src-tauri/src/lib.rs and apps.rs).
import { t, has } from "$lib/i18n";

export type AppStatus = "running" | "starting" | "unhealthy" | "stopped" | "missing" | "unknown";
export type AppInfo = {
  id: string;
  name: string;
  url: string;
  https: boolean;
  port: number;
  installed_unix: number;
  status: AppStatus;
  containers: { name: string; state: string; health: string | null }[];
};
export type Manifest = { id: string; default_name: string; port: number; https: boolean; min_ram_mb: number };
export type CaInfo = { path: string; fingerprint: string };
export type Op = "install" | "remove" | "rename" | "restart";
export type AppProgress = { id: string; op: Op; state: "running" | "done" | "failed"; error: string | null };
/** A backend error: `message` when it is a known error code, otherwise `details` holds technical text. */
export type ErrorText = { message: string | null; details: string | null };
export type Failure = ErrorText & { op: Op };

export const HOME_URL = "http://konak.local";
export const CERT_URL = `${HOME_URL}/konak-root.crt`;

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
