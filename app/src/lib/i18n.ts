// Minimal i18n: locale follows the Windows display language (WebView2 reports it as
// navigator.language). Turkish if it is Turkish, otherwise English.
import en from "../locales/en.json";
import tr from "../locales/tr.json";

type Dict = { [key: string]: string | Dict };
const dicts: Record<string, Dict> = { en, tr };

export const locale = navigator.language.toLowerCase().startsWith("tr") ? "tr" : "en";
document.documentElement.lang = locale;

function lookup(dict: Dict, key: string): string | undefined {
  let node: string | Dict | undefined = dict;
  for (const part of key.split(".")) {
    if (typeof node !== "object") return undefined;
    node = node[part];
  }
  return typeof node === "string" ? node : undefined;
}

export function t(key: string, vars: Record<string, string | number> = {}): string {
  const text = lookup(dicts[locale], key) ?? lookup(dicts.en, key) ?? key;
  return text.replace(/\{(\w+)\}/g, (_, v) => String(vars[v] ?? `{${v}}`));
}

/** True when an i18n key exists, used to map backend error codes to messages. */
export function has(key: string): boolean {
  return lookup(dicts[locale], key) !== undefined;
}
