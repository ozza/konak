// Fails when en.json and tr.json do not have the same keys. Run by `npm run check`.
import { readFileSync } from "node:fs";

const load = (lang) => JSON.parse(readFileSync(new URL(`../src/locales/${lang}.json`, import.meta.url), "utf8"));
const keys = (node, prefix = "") =>
  Object.entries(node).flatMap(([k, v]) => (typeof v === "object" && v !== null ? keys(v, `${prefix}${k}.`) : [`${prefix}${k}`]));

const [en, tr] = [new Set(keys(load("en"))), new Set(keys(load("tr")))];
const missing = (a, b) => [...a].filter((k) => !b.has(k));
const problems = [...missing(en, tr).map((k) => `missing in tr.json: ${k}`), ...missing(tr, en).map((k) => `missing in en.json: ${k}`)];
if (problems.length) {
  console.error(problems.join("\n"));
  process.exit(1);
}
console.log(`locales: ${en.size} keys, en and tr match`);
