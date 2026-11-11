// SPDX-License-Identifier: GPL-3.0-or-later
// Records how the extension's profile layer treats records with language profiles
// (reader-options.js's profile section and profile-state.js), so the relay's port
// (src/host/settings/profiles.rs) can be checked against it:
//
//   node scripts/profile-cases.mjs ../hachidori
//
// writes src/host/settings/testdata/profiles.json: seeded pseudo-random options
// records, patches, dictionary states and records, each with the extension's own
// result (or the error it throws).

import { writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const source = resolve(process.argv[2] ?? "../hachidori");
const load = (path) => import(pathToFileURL(join(source, path)));
await load("extension/vendor/languages/registry.js");
await load("extension/reader-options.js");
const state = await load("extension/profile-state.js");
const options = globalThis.HDReaderOptions;
const registry = globalThis.HDLanguages;

let seed = 0x0050_0bf1;
const random = () => {
  seed = (seed * 1_103_515_245 + 12_345) % 2 ** 31;
  return seed / 2 ** 31;
};
const pick = (list) => list[Math.floor(random() * list.length)];
const chance = (p) => random() < p;
const clone = (value) => (value === undefined ? undefined : JSON.parse(JSON.stringify(value)));
const some = (list, p) => list.filter(() => chance(p));

const JUNK = [null, true, false, 0, -1, 1.5, 16, "", "x", "16", [], [3], {}, { a: 1 }];
const junk = () => clone(pick(JUNK));
const PROFILE_IDS = ["ja", "en", "ar", "zh", "zz", "x", "", "EN"];
const LANGUAGES = ["ja", "en", "ar", "zh", "ko", "zz", "x1", "", 5];
const DEFAULTS = options.DEFAULT_OPTIONS;
const GLOBAL = options.GLOBAL_OPTION_KEYS;
const PROFILE = options.PROFILE_OPTION_KEYS;
const MASKED = Object.keys(options.OPTION_REQUIREMENTS);
const ANKI = [
  { url: "http://127.0.0.1:8765", apiKey: "k", templates: [{ id: "t1", name: "One", deck: "Mining", model: "Kiku" }, { id: "t2", name: "Two" }] },
  { url: "javascript:alert(1)", apiKey: 5 },
  { deck: "Legacy", model: "Basic", tags: ["a"] },
  { templates: [] },
  "garbage",
];
const PERSONAL = (code) => (code === "ja" ? "e4c2e20a1a964b6cbd4ae3f87643c1f0" : registry.get(code)?.personalDictionary?.id);

// A value for an option key: its default, another valid value, or junk.
function optionValue(key) {
  if (key === "anki") return clone(pick(ANKI));
  // Retired keys, which DEFAULT_OPTIONS lacks.
  if (key === "modifier") return pick(["none", "shift", "ctrl", "alt", "bogus", 3]);
  if (key === "hoverDelayMs") return pick([0, 250, "x"]);
  if (key === "customPopupJavascript") return pick(["console.log(1)", " ", 3]);
  if (key === "definitionBlurEnabled") return pick([true, false, "yes"]);
  if (MASKED.includes(key) && chance(0.5)) return typeof DEFAULTS[key] === "boolean" ? !DEFAULTS[key] : pick(["overline", "Pitch", "underline", "color"]);
  if (chance(0.15)) return junk();
  const value = DEFAULTS[key];
  if (typeof value === "boolean") return !value;
  if (typeof value === "number") return pick([value, value + 1, 0, 3]);
  if (key === "experimental") return { multipleLanguages: chance(0.6), crunchyrollMining: chance(0.5) };
  return clone(value);
}

function sparse(keys, p) {
  return Object.fromEntries(some(keys, p).map((key) => [key, optionValue(key)]));
}

function profile() {
  if (chance(0.08)) return junk();
  const language = pick(LANGUAGES);
  const result = { id: chance(0.85) ? (typeof language === "string" && chance(0.8) ? language : pick(PROFILE_IDS)) : junk(), language };
  if (chance(0.9)) result.settings = chance(0.1) ? junk() : { ...sparse([...PROFILE, "anki", "definitionBlurEnabled", ...GLOBAL.slice(0, 3)], 0.08) };
  if (chance(0.6)) result.packSettings = chance(0.15) ? junk() : { archaicForms: pick([true, false, "yes"]), vowelMarks: pick(["hide", "show", 3]) };
  return result;
}

function optionsRecord() {
  if (chance(0.04)) return junk();
  const record = { ...(chance(0.8) ? { revision: pick([0, 3, 57, -1, 1.5, "7"]) } : {}), ...sparse([...GLOBAL, "modifier", "hoverDelayMs", "customPopupJavascript"], 0.1) };
  if (chance(0.3)) return { ...record, ...sparse([...PROFILE, "anki"], 0.1) };
  if (chance(0.5)) record.anki = clone(pick(ANKI));
  record.profiles = chance(0.05) ? [] : Array.from({ length: 1 + Math.floor(random() * 3) }, profile);
  if (chance(0.2)) record.profiles.push(clone(record.profiles[0]));
  return record;
}

function patch() {
  try {
    return options.validateOptionsPatch(sparse([...GLOBAL.slice(0, 12), ...PROFILE.slice(0, 40), "anki"], 0.06));
  } catch {
    return {};
  }
}

const PACKAGE_IDS = ["p1", "p2", "p3", "p4", PERSONAL("ja"), PERSONAL("en"), PERSONAL("ar")];
function row(id = pick(PACKAGE_IDS)) {
  if (chance(0.04)) return junk();
  return { id, title: `T ${id}`, path: `/dicts/${id}`, ...(chance(0.7) ? { enabled: chance(0.7) } : {}),
    language: pick(["ja", "en", "ar"]), ...(chance(0.4) ? { targetLanguage: pick(["en", "ja"]) } : {}), termCount: 3 };
}
const rows = () => [...new Set(some(PACKAGE_IDS, 0.6))].map((id) => row(id));
const entry = () => (chance(0.05) ? junk() : { id: pick(PACKAGE_IDS), ...(chance(0.8) ? { enabled: chance(0.7) } : {}) });
const groups = () => (chance(0.2) ? junk() : Array.from({ length: Math.floor(random() * 3) },
  (_, index) => ({ id: `g${index}`, name: "G", dictionaryIds: some([...PACKAGE_IDS, "p1"], 0.4) })));

function dictionaryState() {
  if (chance(0.04)) return pick([null, undefined, "x"]);
  if (chance(0.35)) return { schemaVersion: 1, ...(chance(0.9) ? { revision: pick([0, 4, -2]) } : {}), dictionaries: rows(), groups: groups() };
  const profiles = Object.fromEntries(some(["ja", "en", "ar", "zh"], 0.6).map((id) => [id, chance(0.08) ? junk()
    : { dictionaries: Array.from({ length: Math.floor(random() * 5) }, entry), groups: groups() }]));
  return { schemaVersion: chance(0.95) ? 2 : 1, ...(chance(0.9) ? { revision: pick([0, 31]) } : {}), dictionaries: rows().map((r) => (r && typeof r === "object" ? state.packageRow(r) : r)), profiles };
}

function document() {
  return chance(0.3) ? undefined : { revision: 2, semanticRevision: "s", text: pick(["", "猫\tねこ\tcat"]),
    ...(chance(0.5) ? { languages: { en: { revision: 1, semanticRevision: "s", text: pick(["", "cat\t\tneko"]) } } } : {}) };
}

const cases = [];
// Each distinct argument is stored once, in `values`; a case's arguments are
// indexes into it, -1 for undefined.
const values = [];
const interned = new Map();
function intern(value) {
  if (value === undefined) return -1;
  const text = JSON.stringify(value);
  if (!interned.has(text)) {
    interned.set(text, values.length);
    values.push(JSON.parse(text));
  }
  return interned.get(text);
}
// Flat options are recorded as what differs from normaliseOptions({}), as
// scripts/settings-cases.mjs records them.
const BASE = options.normaliseOptions({});
const changed = (flat) => ({ changed: Object.fromEntries(Object.entries(flat)
  .filter(([key, value]) => JSON.stringify(value) !== JSON.stringify(BASE[key]))) });
function record(fn, args, run, { flat = false } = {}) {
  let expected;
  try {
    const result = run();
    expected = result instanceof Set ? { ok: [...result].sort() } : flat ? changed(result) : { ok: clone(result) };
  } catch (error) {
    // A TypeError is upstream reading a junk row it never stores; the relay need not crash alike.
    if (error instanceof TypeError) return;
    expected = { error: error.message, ...(error.code ? { code: error.code } : {}) };
  }
  cases.push({ fn, args: args.map(intern), expected });
}

for (let i = 0; i < 250; i++) {
  const stored = optionsRecord();
  const id = pick(PROFILE_IDS);
  record("profiles", [stored], () => options.profiles(stored));
  record("projectOptions", [stored, id, true], () => options.projectOptions(stored, id), { flat: true });
  record("projectOptions", [stored, id, false], () => options.projectOptions(stored, id, { masked: false }), { flat: true });
  record("projectStoredOptions", [stored], () => options.projectStoredOptions(stored));
  record("normaliseOptions", [stored], () => options.normaliseOptions(stored), { flat: true });
  record("profileDescriptor", [stored, id], () => options.profileDescriptor(stored, id));
  const flat = patch();
  record("applyOptionsPatch", [stored, flat, id], () => options.applyOptionsPatch(stored, flat, id));
  const packPatch = chance(0.1) ? junk() : chance(0.2) ? {} : { archaicForms: pick([true, "no"]), other: 1 };
  record("applyPackSettingsPatch", [stored, packPatch, id], () => options.applyPackSettingsPatch(stored, packPatch, id));
  record("optionsWithProfiles", [stored], () => state.optionsWithProfiles(stored));
  record("optionsWithoutProfiles", [stored], () => state.optionsWithoutProfiles(stored));
  record("applyToEveryProfile", [stored, flat], () => state.applyToEveryProfile(stored, flat));
  const selection = chance(0.2) ? junk() : { revision: pick([1, -1]), activeProfileId: pick(PROFILE_IDS), autoSwitch: pick([true, "yes"]) };
  record("activeProfileId", [stored, selection], () => options.activeProfileId(stored, selection));
  record("normaliseLanguageSelection", [selection], () => options.normaliseLanguageSelection(selection));
  const settings = options.profiles(stored)[0]?.settings;
  const language = pick(["ja", "en", "zh", "zz"]);
  let counter = 0;
  record("newProfileSettings", [settings, language], () => state.newProfileSettings(settings, registry.get(language), () => `id-${++counter}`));
  record("packSettingDefaults", [language], () => state.packSettingDefaults(registry.get(language)));
}

for (let i = 0; i < 400; i++) {
  const stored = dictionaryState();
  const id = pick(PROFILE_IDS);
  record("projectDictionaryState", [stored, id], () => options.projectDictionaryState(stored, id));
  record("allPackages", [stored], () => options.allPackages(stored));
  record("dictionaryStateWithProfiles", [stored], () => state.dictionaryStateWithProfiles(stored));
  record("japaneseDictionaryView", [stored], () => state.japaneseDictionaryView(stored));
  if (!options.hasDictionaryProfiles(stored)) continue;
  const view = { dictionaries: chance(0.1) ? [row("p9")] : some(stored.dictionaries.filter((r) => r && typeof r === "object"), 0.7).map((r) => ({ ...r, enabled: chance(0.6) })),
    ...(chance(0.7) ? { groups: groups() } : {}) };
  const legacy = chance(0.4);
  record("mergeDictionaryView", [stored, id, view, legacy], () => state.mergeDictionaryView(stored, id, view, { legacy }));
  const packages = [...some(stored.dictionaries.filter((r) => r && typeof r === "object"), 0.8).map((r) => ({ ...r, enabled: chance(0.7) })),
    ...some(["p5", PERSONAL("en"), PERSONAL("zh")], 0.3).map((pid) => row(pid))].filter((r) => r && typeof r === "object");
  const joins = some(PACKAGE_IDS, 0.2);
  record("mergeEngineDictionaries", [stored, packages, id, joins], () => state.mergeEngineDictionaries(stored, packages, { profileId: id, joins }));
}

for (let i = 0; i < 300; i++) {
  const records = { options: optionsRecord(), state: dictionaryState(), document: document(),
    lookupStats: chance(0.5) ? { generation: "g", ...(chance(0.5) ? { languages: { en: "g2" } } : {}) } : undefined,
    wordStatusOverrides: chance(0.5) ? { known: some(["猫"], 0.5), ignored: [], ...(chance(0.5) ? { languages: { ar: { known: some(["x"], 0.5), ignored: [] } } } : {}) } : undefined };
  record("heldLanguages", [records], () => state.heldLanguages(records));
  record("singleProfileShape", [records], () => state.singleProfileShape(records));
}

const out = new URL("../src/host/settings/testdata/profiles.json", import.meta.url);
writeFileSync(out, `${JSON.stringify({ values, cases })}\n`);
console.log(`${cases.length} cases`);
