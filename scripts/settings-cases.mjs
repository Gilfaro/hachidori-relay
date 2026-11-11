// SPDX-License-Identifier: GPL-3.0-or-later
// Records how the extension's reader-options.js treats options, so the relay's port
// (src/host/settings/options.rs) can be checked against it:
//
//   node scripts/settings-cases.mjs ../hachidori
//
// writes src/host/settings/testdata/options.json: seeded pseudo-random stored
// options, write patches, libraries and Anki configurations, each with the
// extension's own result (or the error it throws).

import { writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const source = resolve(process.argv[2] ?? "../hachidori");
await import(pathToFileURL(join(source, "extension/reader-options.js")));
const options = globalThis.HDReaderOptions;
const custom = await import(pathToFileURL(join(source, "extension/custom-dictionary.js")));

let seed = 0x5eed_0b75;
const random = () => {
  seed = (seed * 1_103_515_245 + 12_345) % 2 ** 31;
  return seed / 2 ** 31;
};
const pick = (list) => list[Math.floor(random() * list.length)];
const chance = (p) => random() < p;
const clone = (value) => (value === undefined ? undefined : JSON.parse(JSON.stringify(value)));
const some = (list, p) => list.filter(() => chance(p));

const JUNK = [null, true, false, 0, -1, 1.5, 16, 1e9, -0.5, "", " ", "x", "16", " 7 ", "0x10", "1e2", "\u0001bad",
  [], [3], ["4"], [1, 2], {}, { a: 1 }, "Infinity", "　"];
const URLS = ["http://127.0.0.1:8765", "http://127.0.0.1:8765/", "https://anki.example/path?q=1#frag", " http://localhost:8765 ",
  "ftp://example.com", "http://user:pass@example.com", "http://user@example.com", "not a url", "http://a/?", "HTTP://EXAMPLE.COM:80/",
  "http://例え.jp/", "http://a\u0001b", "", 5, null];
const TITLES = ["JMdict", "Freq", "Pitch", "Kanji", "Mixed", "Gone", "", "Hachidori Custom Dictionary"];
const ACTIONS = ["", "close", "nextEntry", "previousEntry", "playAudioFromSource", "toggleOption", "addNote", "searchRowUp", "bogus", 5];

function junk() {
  return clone(pick(JUNK));
}

function keybind() {
  if (chance(0.1)) return junk();
  const bind = {};
  if (chance(0.95)) bind.action = pick(ACTIONS);
  if (chance(0.8)) bind.argument = pick(["", "1", "3", "0", "01", "12", "x", "hoverEnabled", "lowMemoryMode", "scanLength", 5, null]);
  if (chance(0.8)) bind.key = pick(["Escape", "KeyA", "", null, 7]);
  if (chance(0.8)) bind.modifiers = chance(0.8) ? some(["shift", "alt", "ctrl", "meta", "bogus", 1], 0.4) : junk();
  if (chance(0.8)) bind.scopes = chance(0.8) ? some(["web", "popup", "search", "x"], 0.4) : junk();
  if (chance(0.8)) bind.enabled = pick([true, false, "yes", null]);
  return bind;
}

function exact(list) {
  // A list already in normal form, which a strict write accepts.
  return list;
}

function audioSource() {
  if (chance(0.1)) return junk();
  const value = {};
  if (chance(0.9)) value.id = pick(["a", "b", "", 3, "default-tts"]);
  if (chance(0.9)) value.type = pick(["custom", "custom-json", "text-to-speech", "text-to-speech-reading", "bogus"]);
  if (chance(0.7)) value.enabled = pick([true, false, 1]);
  if (chance(0.7)) value.url = pick(["https://x/{term}", "", 4]);
  if (chance(0.5)) value.voice = pick(["ja-JP", "", null]);
  return value;
}

function link() {
  if (chance(0.1)) return junk();
  const value = {};
  if (chance(0.9)) value.label = pick(["Jisho", "  ", "", "a\u0007b", 4, "Wiki"]);
  if (chance(0.9)) value.url = pick(["https://jisho.org/search/{term}", " ", "", 3]);
  if (chance(0.2)) value.extra = 1;
  return value;
}

function button() {
  if (chance(0.1)) return junk();
  const value = {};
  if (chance(0.9)) value.id = pick(["b1", "b2", "", "x".repeat(257), "a\u0001", 7]);
  if (chance(0.9)) value.type = pick(["link", "anki", "other"]);
  if (chance(0.9)) value.label = pick(["Go", " ", "Mine", 5]);
  if (chance(0.7)) value.url = pick(["https://x", "", " "]);
  if (chance(0.6)) value.templateId = pick(["default", "t2", "", null]);
  return value;
}

function userScript() {
  if (chance(0.1)) return junk();
  const value = {};
  if (chance(0.9)) value.id = pick(["s1", "s2", "", "x".repeat(257), "a\u0001", 7]);
  if (chance(0.9)) value.name = pick(["Jimaku Player", " ", "", "a\u0007b", 4]);
  if (chance(0.9)) value.enabled = pick([true, false, "yes"]);
  if (chance(0.9)) value.javascript = pick(["", "console.log(1)", "  ", 3]);
  if (chance(0.9)) value.css = pick(["", "body { color: red }", null]);
  if (chance(0.3)) value.store = pick([{ slug: "jimaku", version: "1.0.0" }, { slug: "", version: "1" }, { slug: "x" },
    { slug: "x", version: "2", extra: 1 }, "x", null]);
  if (chance(0.1)) value.extra = 1;
  return value;
}

function fieldTemplates() {
  return pick([null, undefined, {}, { Front: { value: "{expression}", overwriteMode: "coalesce" } },
    { Front: { value: 3, overwriteMode: "coalesce" } }, { "": { value: "x", overwriteMode: "skip" } },
    { Back: { value: "{glossary}", overwriteMode: "bogus" } }, "x", []]);
}

function templateConfig(value = {}) {
  if (chance(0.7)) value.deck = pick(["Default", "Mining", 4, ""]);
  if (chance(0.7)) value.model = pick(["", "Basic", null]);
  if (chance(0.6)) value.tags = chance(0.8) ? some(["hachidori", "mined", 3, "x"], 0.5) : junk();
  if (chance(0.6)) value.fields = chance(0.8) ? Object.fromEntries(some(options.ANKI_FIELDS, 0.4).map((field) => [field, pick(["{expression}", "", 1])])) : junk();
  if (chance(0.5)) value.duplicateScope = pick(["model", "deck", "all", "deck-root", "collection", "x"]);
  if (chance(0.2)) value.duplicateScopeCheckAllModels = pick([true, false]);
  if (chance(0.5)) value.duplicateBehavior = pick(["prevent", "new", "overwrite", "x"]);
  if (chance(0.2)) value.checkForDuplicates = pick([true, false]);
  if (chance(0.5)) value.captureScreenshot = pick([true, false, "yes"]);
  if (chance(0.5)) {
    const templates = fieldTemplates();
    if (templates !== undefined) value.fieldTemplates = templates;
  }
  return value;
}

function anki() {
  if (chance(0.05)) return junk();
  // A normalized configuration, as Settings sends it, often valid as a write.
  if (chance(0.35)) {
    const base = options.normaliseAnki(chance(0.5) ? {} : templateConfig());
    if (chance(0.5)) {
      base.templates.push({ id: pick(["t2", "default", "x"]), name: pick(["Second", "", " "]), ...templateConfig() });
      return clone(chance(0.5) ? options.normaliseAnki(base) : base);
    }
    if (chance(0.3)) base.url = pick(URLS);
    return clone(base);
  }
  const value = templateConfig();
  if (chance(0.6)) value.url = pick(URLS);
  if (chance(0.5)) value.apiKey = pick(["", "secret", 3]);
  if (chance(0.4)) {
    value.templates = chance(0.85) ? Array.from({ length: Math.floor(random() * 3) }, (_, index) => {
      const template = templateConfig({});
      if (chance(0.8)) template.id = pick(["default", `t${index}`, "", "dup"]);
      if (chance(0.8)) template.name = pick(["Default", "Two", "", "a\u0002"]);
      return template;
    }) : junk();
  }
  return value;
}

function selection() {
  return pick(["", "JMdict", "Kanji", "Gone", { title: "Kanji", kind: "kanji" }, { title: "JMdict", kind: "term" },
    { title: "Mixed" }, { kind: "tabGroup", id: "g1" }, { kind: "tabGroup", id: "" }, { title: "", kind: "term" }, 4, null, []]);
}

// A value for each option: mostly its kind of value, sometimes anything at all.
function value(key) {
  const fallback = options.DEFAULT_OPTIONS[key];
  if (chance(0.15)) return junk();
  if (chance(0.2)) return clone(fallback);
  if (Object.hasOwn(options.NUMBER_RANGES, key)) {
    const [min, max] = options.NUMBER_RANGES[key];
    return pick([min, max, min - 1, max + 1, Math.floor((min + Math.min(max, 1e6)) / 2), 2.5, "12", null]);
  }
  if (typeof fallback === "boolean") return pick([true, false]);
  switch (key) {
    case "lookupMode": return pick([...options.LOOKUP_MODES, "x"]);
    case "definitionLookupMode": return pick([...options.DEFINITION_LOOKUP_MODES, "y"]);
    case "activationKey": return pick(["Shift", "shift", "a", " ", "F12", "MouseBack", "mouseforward", "Nope", "ß", "MouseRight",
      "mouseright", "", "KeyQ"]);
    case "videoAudioFormat": return pick(["mp3", "opus", "wav"]);
    case "videoImageFormat": return pick(["gif", "webp", "avif", "png"]);
    case "popupTheme": return pick(["default", "crystal", "nord", "auto", "missing"]);
    case "popupThemeVariant": return pick(["auto", "grey", "blue", "red"]);
    case "popupToolbarPosition": return pick(["auto", "top", "bottom", "left"]);
    case "frequencyOrder": return pick(options.FREQUENCY_ORDERS.concat("x"));
    case "dictionaryEntryStorage": case "dictionaryIndexStorage": return pick(["auto", "paged", "resident", "disk"]);
    case "kanjiClickDictionary": return selection();
    case "popupImageSource": return pick([null, { kind: "dictionary", title: "JMdict" }, { kind: "dictionary", title: "" },
      { kind: "tabGroup", id: "g1" }, { kind: "x" }, "JMdict", []]);
    case "audioSources": return chance(0.3) ? exact(options.normaliseOptions({ audioSources: [audioSource(), audioSource()] }).audioSources)
      : Array.from({ length: Math.floor(random() * 4) }, audioSource);
    case "customLinks": return chance(0.3) ? clone(options.normaliseOptions({ customLinks: [link(), link()] }).customLinks)
      : Array.from({ length: Math.floor(random() * 4) }, link);
    case "customButtons": return chance(0.3) ? clone(options.normaliseCustomButtons([button(), button(), button()]))
      : Array.from({ length: Math.floor(random() * 4) }, button);
    case "userScripts": return chance(0.3) ? clone(options.normaliseUserScripts([userScript(), userScript(), userScript()]))
      : Array.from({ length: Math.floor(random() * 4) }, userScript);
    case "keybinds": return chance(0.3) ? clone(options.normaliseOptions({ keybinds: [keybind(), keybind()] }).keybinds)
      : Array.from({ length: Math.floor(random() * 4) }, keybind);
    case "anki": return anki();
    case "experimental": return chance(0.5) ? clone(options.normaliseExperimental({ crunchyrollMining: chance(0.5) }))
      : { googleDocs: pick([true, "x"]), ...(chance(0.5) ? { netflixMining: true } : {}), ...(chance(0.3) ? { crunchyrollMining: pick([true, 1]) } : {}),
        ...(chance(0.2) ? { bogus: true } : {}), ...(chance(0.2) ? { youtubeMining: true } : {}),
        ...(chance(0.3) ? { smallerAnkiCards: pick([true, false]) } : {}) };
    default: return pick(["", "JMdict", "Freq", "body { color: red }", 3, null]);
  }
}

const KEYS = Object.keys(options.DEFAULT_OPTIONS);
function optionsObject(size) {
  const result = {};
  for (const key of some(KEYS, size)) {
    const item = value(key);
    if (item !== undefined) result[key] = item;
  }
  if (chance(0.08)) result.modifier = pick(["none", "shift", "ctrl", "alt", "meta", 1]);
  if (chance(0.08)) result.definitionBlurEnabled = pick([true, false, "x"]);
  if (chance(0.05)) result.hoverDelayMs = 100;
  if (chance(0.08)) result.customPopupJavascript = pick(["console.log(1)", "  ", "", 3]);
  if (chance(0.05)) result.revision = pick([0, 3, -1, 2.5]);
  if (chance(0.05)) result.notAnOption = 1;
  return result;
}

function dictionary(title, index) {
  const counts = pick([
    { termCount: 10, frequencyCount: 0, pitchCount: 0, kanjiCount: 0 },
    { termCount: 0, frequencyCount: 5, pitchCount: 0, kanjiCount: 0 },
    { termCount: 0, frequencyCount: 0, pitchCount: 4, kanjiCount: 0 },
    { termCount: 0, frequencyCount: 0, pitchCount: 0, kanjiCount: 9 },
    { termCount: 0, frequencyCount: 0, pitchCount: 0, kanjiCount: 0 },
    { termCount: "3" },
    {},
  ]);
  return { id: `id${index}`, title, enabled: pick([true, true, false, undefined]), ...counts };
}

const cases = [];
const record = (fn, input, run, extra = {}) => {
  let expected;
  try {
    expected = { ok: run() };
  } catch (error) {
    expected = { error: error.message };
  }
  cases.push({ fn, ...(input === undefined ? {} : { input: clone(input) }), ...extra, expected: clone(expected) });
};

for (const input of [undefined, null, [], "x", 3, {}, options.DEFAULT_OPTIONS]) {
  record("normaliseOptions", input, () => options.normaliseOptions(input));
  record("projectStoredOptions", input, () => options.projectStoredOptions(input));
  record("validateOptionsPatch", input, () => options.validateOptionsPatch(input));
}
for (let index = 0; index < 1000; index += 1) {
  const stored = optionsObject(0.15);
  record("normaliseOptions", stored, () => options.normaliseOptions(stored));
  record("projectStoredOptions", stored, () => options.projectStoredOptions(stored));
  // A write patch: a few keys, half of them exactly as Settings would send them.
  const patch = optionsObject(0.04);
  if (chance(0.5)) {
    const normal = options.normaliseOptions(optionsObject(0.1));
    for (const key of some(KEYS, 0.05)) patch[key] = clone(normal[key]);
  }
  record("validateOptionsPatch", patch, () => options.validateOptionsPatch(patch));
}
for (let index = 0; index < 600; index += 1) {
  const dictionaries = some(TITLES, 0.6).map(dictionary);
  if (chance(0.03)) dictionaries.push(null);
  const groups = chance(0.7) ? [{ id: "g1", name: "One", dictionaryIds: ["id0"] }] : chance(0.5) ? [] : [{ id: "g2" }];
  const selected = { frequencyDictionary: pick(["", ...TITLES]), revision: 1, ...(chance(0.7) ? { kanjiClickDictionary: selection() } : {}) };
  const input = chance(0.05) ? pick([null, [], "x"]) : selected;
  record("normaliseDictionarySelections", input, () => options.normaliseDictionarySelections(input, dictionaries, groups),
    { dictionaries, groups });
}
for (let index = 0; index < 300; index += 1) {
  const input = anki();
  const templateId = pick([undefined, null, "default", "t2", "dup", "x"]);
  record("ankiTemplateConfig", input, () => options.ankiTemplateConfig(input, templateId) ?? null,
    templateId === undefined ? {} : { templateId });
}

// Custom dictionary sources: their entries, line errors and semantic revision.
const PIECES = ["食べる", " たべる ", "to eat", String.raw`\n`, String.raw`\\`, ",", "#", " ", "\t", "　", "﻿", "x", "",
  "\r", "\n", "\r\n", "𠮷", " ", "\u0085", "a\"b", "\u0001"];
for (let index = 0; index < 400; index += 1) {
  const text = Array.from({ length: Math.floor(random() * 24) }, () => pick(PIECES)).join("");
  const parsed = custom.parseCustomDictionary(text);
  let revision;
  try {
    revision = await custom.customDictionarySemanticRevision(parsed.entries);
  } catch (error) {
    revision = { error: error.message };
  }
  cases.push({ fn: "parseCustomDictionary", input: text, expected: { ok: { ...parsed, revision } } });
}

// normaliseOptions results are recorded as the fields that differ from the defaults.
const defaults = JSON.stringify(Object.keys(options.DEFAULT_OPTIONS).sort());
for (const item of cases) {
  const result = item.expected.ok;
  if (item.fn !== "normaliseOptions") continue;
  if (JSON.stringify(Object.keys(result).sort()) !== defaults) throw new Error("normaliseOptions changed its keys");
  item.expected = { changed: Object.fromEntries(Object.entries(result)
    .filter(([key, field]) => JSON.stringify(field) !== JSON.stringify(options.DEFAULT_OPTIONS[key]))) };
}
const out = resolve("src/host/settings/testdata/options.json");
writeFileSync(out, `${JSON.stringify(cases)}\n`);
const errors = cases.filter((item) => item.expected.error).length;
console.log(`${cases.length} cases (${errors} errors) written to ${out}`);
