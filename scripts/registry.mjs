// SPDX-License-Identifier: GPL-3.0-or-later
// Copies the language registry a Hachidori build ships (extension/vendor/languages/registry.js,
// which its scripts/sync-languages.mjs generates from the Hoshidicts packs) for the relay:
//
//   node scripts/registry.mjs ../hachidori
//
// writes src/host/registry.json, every pack's descriptor in the registry's order, as
// `HDLanguages.get(code)` returns it. Run it when the Hoshidicts submodule moves.

import { readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import vm from "node:vm";

const source = resolve(process.argv[2] ?? "../hachidori");
const context = vm.createContext({});
vm.runInContext(readFileSync(join(source, "extension/vendor/languages/registry.js"), "utf8"), context);
const languages = context.HDLanguages;
const header = readFileSync(join(source, "extension/vendor/languages/registry.js"), "utf8").split(/\r?\n/u)[0].replace(/^\/\/ /u, "");
const packs = languages.codes.map((code) => languages.get(code));
writeFileSync(new URL("../src/host/registry.json", import.meta.url), `${JSON.stringify({ source: header, packs }, null, 1)}\n`);
console.log(`${packs.length} packs`);
