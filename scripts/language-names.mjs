// Writes src/host/language-names.json: the English name the platform gives
// every two- and three-letter language code (`Intl.DisplayNames`), which
// Hachidori's languageName (extension/language-names.js) falls back to for a
// language this build has no pack for, and the likely script of each
// (`new Intl.Locale(code).maximize().script`), which its targetLanguage
// (render/glossary.js) reads for a dictionary's target language without a
// pack. Codes the platform does not name, or gives no script, are left out.
//
//   node scripts/language-names.mjs
//
// SPDX-License-Identifier: GPL-3.0-or-later
import { writeFileSync } from "node:fs";

const names = new Intl.DisplayNames(["en"], { type: "language" });
const letters = "abcdefghijklmnopqrstuvwxyz";
const codes = [];
for (const a of letters) for (const b of letters) {
  codes.push(a + b);
  for (const c of letters) codes.push(a + b + c);
}
const table = {};
const scripts = {};
for (const code of codes.sort()) {
  const name = names.of(code);
  if (name !== code) table[code] = name;
  const script = new Intl.Locale(code).maximize().script;
  // By script, the codes space-separated: a table of codes would be six times larger.
  if (script) scripts[script] = scripts[script] ? `${scripts[script]} ${code}` : code;
}
const target = new URL("../src/host/language-names.json", import.meta.url);
writeFileSync(target, `${JSON.stringify({ icu: process.versions.icu, cldr: process.versions.cldr, names: table, scripts }, null, 0)}\n`);
console.log(`${Object.keys(table).length} names, ${Object.values(scripts).join(" ").split(" ").length} scripts (ICU ${process.versions.icu}, CLDR ${process.versions.cldr})`);
