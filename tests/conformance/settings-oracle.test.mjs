// SPDX-License-Identifier: GPL-3.0-or-later
// Host mode's storage requests checked against the extension itself: the
// extension's service worker (booted in Node with its own smoke-test fakes)
// answers each request a linked browser forwards, through its own
// dispatchSharedRequest, and the relay answers the same request over /link. Each
// reply, and the shared storage after it, must agree. Needs a Hachidori checkout
// (HACHIDORI_SOURCE, by default ../hachidori) and a relay built with host mode.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs, { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { relayBinary, startAnkiRelayServer } from "./anki-relay-server.mjs";
import { connectClient, untilKind } from "./relay-socket.mjs";
import { oracleSkip } from "./oracle-skip.mjs";

const REPO = fileURLToPath(new URL("../..", import.meta.url));
const HACHIDORI = resolve(process.env.HACHIDORI_SOURCE ?? join(REPO, "..", "hachidori"));
const HOST_MODE = execFileSync(relayBinary(), ["--help"], { encoding: "utf8" }).includes("--host");
const missing = [
  ...(HOST_MODE ? [] : ["a relay built with host mode"]),
  ...(existsSync(join(HACHIDORI, "test/extension-smoke/fakes.mjs")) ? [] : [join(HACHIDORI, "test/extension-smoke/fakes.mjs")]),
];
const skip = oracleSkip(missing);

const SHARED = ["dictionaryState", "options", "customDictionarySource", "dictionaryUpdates", "lookupStats", "wordStatusOverrides"];
const CUSTOM_ID = "e4c2e20a1a964b6cbd4ae3f87643c1f0";
const CUSTOM_TITLE = "Hachidori Custom Dictionary";

// The smoke fakes strip a module's imports with LF-only patterns, so a checkout with
// CRLF line endings (Git for Windows' default) is read as LF.
function readSourcesAsLf() {
  const read = fs.readFileSync;
  fs.readFileSync = function readFileSyncLf(path, options) {
    const data = read.call(this, path, options);
    return typeof data === "string" && String(path).startsWith(HACHIDORI) ? data.replaceAll("\r\n", "\n") : data;
  };
  syncBuiltinESMExports();
}

const dictionary = (id, title, counts, extra = {}) => ({
  id, title, enabled: true, path: `/generations/${id}`, revision: "1", termCount: 0, frequencyCount: 0, pitchCount: 0,
  kanjiCount: 0, mediaCount: 0, ...counts, ...extra,
});
const LIBRARY = [
  dictionary("d1", "JMdict", { termCount: 10 }),
  dictionary("d2", "Freq", { frequencyCount: 5 }),
  dictionary("d3", "Kanji", { kanjiCount: 9 }),
];
const SEED = {
  dictionaryState: { schemaVersion: 1, revision: 3, dictionaries: LIBRARY,
    groups: [{ id: "g1", name: "Main", dictionaryIds: ["d1", "d2", "d9"] }] },
  options: { revision: 2, frequencyDictionary: "Freq", kanjiClickDictionary: "Kanji", scanLength: 12,
    popupImageSource: { kind: "dictionary", title: "JMdict" },
    customButtons: [{ id: "keep", type: "link", label: "Jisho", url: "https://jisho.org/search/{term}" },
      { id: "mine", type: "anki", label: "Mine", templateId: "default" }] },
  wordStatusOverrides: { revision: 4, known: ["猫"], ignored: [] },
};

async function bootExtension(seed = SEED) {
  readSourcesAsLf();
  const fakes = await import(pathToFileURL(join(HACHIDORI, "test/extension-smoke/fakes.mjs")));
  fakes.installFakeIndexedDB();
  fakes.installFetch();
  fakes.installNavigator();
  const storage = fakes.makeStorage();
  await storage.api().local.set(structuredClone(seed));
  // The background bundle leaves profile-state.js out; the extension's own
  // profile tests hand its functions in (profiles.test.mjs).
  const profileState = await import(pathToFileURL(join(HACHIDORI, "extension/profile-state.js")));
  const context = fakes.loadBackgroundScript({
    chrome: fakes.makeChrome("sw", fakes.makeBus(), storage, fakes.makeAlarms()), console, fetch: globalThis.fetch, setTimeout,
    clearTimeout, URL, ...Object.fromEntries(Object.entries(profileState).filter(([, value]) => typeof value === "function")),
  });
  const read = async () => {
    const all = await storage.api().local.get(null);
    return Object.fromEntries(Object.entries(all).filter(([key]) => SHARED.includes(key) || key.startsWith("lookupStats:")));
  };
  return { context, read };
}

// Lookup-count generations are random and rows carry the time: each side's are
// named in the order they appear, and times are compared as present.
function comparable(value) {
  const names = new Map();
  const name = (uuid) => {
    if (!names.has(uuid)) names.set(uuid, `generation-${names.size + 1}`);
    return names.get(uuid);
  };
  const UUID = /[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}/gu;
  const walk = (item, key) => {
    if (typeof item === "string") return item.replace(UUID, name);
    if (typeof item === "number" && (key === "firstLookedUpAt" || key === "lastLookedUpAt")) return "time";
    if (Array.isArray(item)) return item.map((entry) => walk(entry));
    if (item && typeof item === "object") {
      return Object.fromEntries(Object.entries(item).map(([field, entry]) => [field.replace(UUID, name), walk(entry, field)]));
    }
    return item;
  };
  return walk(JSON.parse(JSON.stringify(value)));
}

const anki = (fields = {}) => ({
  url: "http://127.0.0.1:8765", apiKey: "", deck: "Default", model: "", tags: ["hachidori"],
  fields: Object.fromEntries(["expression", "reading", "definition", "sentence", "frequency", "pitch", "audio", "screenshot"]
    .map((field) => [field, ""])),
  duplicateScope: "model", duplicateBehavior: "prevent", captureScreenshot: true, fieldTemplates: null, ...fields,
});

// Each step is one request, from the client that predates Templates ("legacy")
// or from a current one. A function step builds its request from what the
// extension holds now.
function steps(context) {
  const worker = (type, fields = {}) => ({ target: "hoshidicts-worker", type, requestId: type, ...fields });
  const custom = async (text, dictionaries) => {
    const { entries } = context.parseCustomDictionary(text);
    const semanticRevision = await context.customDictionarySemanticRevision(entries);
    const pack = entries.length === 0 ? [] : [dictionary(CUSTOM_ID, CUSTOM_TITLE, { termCount: entries.length },
      { revision: semanticRevision, isUpdatable: false, indexUrl: null, downloadUrl: null, language: "ja" })];
    return { text, semanticRevision, ...(dictionaries === undefined ? {} : { dictionaries: [...pack, ...dictionaries] }) };
  };
  const current = [
    worker("hd_state_read"),
    worker("hd_state_cas", { baseRevision: 2, dictionaries: LIBRARY }),
    worker("hd_state_cas", { baseRevision: "3", dictionaries: LIBRARY }),
    worker("hd_state_cas", { baseRevision: 3 }),
    worker("hd_state_cas", { baseRevision: 3, dictionaries: LIBRARY, groups: null }),
    worker("hd_state_cas", { baseRevision: 3, dictionaries: [{ ...LIBRARY[0], updateScheduleOverride: "yearly" }] }),
    worker("hd_state_cas", { baseRevision: 3, dictionaries: [null] }),
    worker("hd_state_cas", { baseRevision: 3, dictionaries: [{ ...LIBRARY[0], updateScheduleOverride: "weekly" }, { ...LIBRARY[1], title: "Freq 2" }] }),
    worker("hd_state_read"),
    worker("hd_options_write", { baseRevision: 0, options: { scanLength: 20 } }),
    worker("hd_options_write", { baseRevision: 3, options: { scanLength: "20" } }),
    worker("hd_options_write", { baseRevision: 3, options: [] }),
    worker("hd_options_write", { baseRevision: 3, options: { scanLength: 20 }, requestId: {} }),
    worker("hd_options_write", { baseRevision: 3, options: { scanLength: 20, frequencyDictionary: "Gone" } }),
    worker("hd_options_write", { baseRevision: 4, options: { scanLength: 20 } }),
    worker("hd_options_write", { baseRevision: 4, options: { popupCss: "x".repeat(1_100_000) } }),
    ["legacy", worker("hd_options_write", { baseRevision: 4, options: { customButtons: [] } })],
    ["legacy", worker("hd_options_write", { baseRevision: 4, options: { anki: { ...anki(), templates: [] } } })],
    ["legacy", worker("hd_options_write", { baseRevision: 4, options: {
      customLinks: [{ label: "Wiki", url: "https://ja.wikipedia.org/wiki/{term}" }, { label: "Jisho", url: "https://jisho.org/search/{term}" }] } })],
    ["legacy", worker("hd_options_write", { baseRevision: 5, options: { anki: anki({ deck: "Mining", tags: ["mined"] }) } })],
    worker("hd_options_write", { baseRevision: 6, options: {
      customButtons: [{ id: "b1", type: "link", label: "Go", url: "https://example.com/{term}" }],
      customLinks: [{ label: "Go", url: "https://example.com/{term}" }] } }),
    worker("hd_custom_read"),
    worker("hd_custom_cas", { baseDocumentRevision: 0, baseRevision: 4, text: "x,y,z", semanticRevision: "0".repeat(64) }),
    async () => worker("hd_custom_cas", { baseDocumentRevision: 1, baseRevision: 4, ...await custom("食べる,たべる,to eat", []) }),
    async () => worker("hd_custom_cas", { baseDocumentRevision: 0, baseRevision: 0, ...await custom("食べる,たべる,to eat", []) }),
    async () => worker("hd_custom_cas", { baseDocumentRevision: 0, baseRevision: 4, ...await custom("食べる,たべる,to eat") }),
    async () => worker("hd_custom_cas", { baseDocumentRevision: 0, baseRevision: 4,
      ...await custom("# mine\n食べる, たべる, to eat\\nconsume\n猫,ねこ,cat", [LIBRARY[0], { ...LIBRARY[1], title: "Freq 2" }]),
      groups: [{ id: "g2", name: "Two", dictionaryIds: [CUSTOM_ID, "d2", "d2"] }] }),
    worker("hd_custom_read"),
    worker("hd_state_cas", { baseRevision: 5, dictionaries: LIBRARY }),
    async () => worker("hd_custom_cas", { baseDocumentRevision: 1, baseRevision: 5, ...await custom("", []) }),
    async () => worker("hd_custom_cas", { baseDocumentRevision: 1, baseRevision: 5, ...await custom("") }),
    async () => worker("hd_custom_cas", { baseDocumentRevision: 1, baseRevision: 5, ...await custom("", [LIBRARY[0]]) }),
    worker("hd_state_read"),
    worker("hd_lookup_stats_read", { term: " 食べる ", reading: "たべる" }),
    worker("hd_lookup_stats_record", { term: "食べる", reading: "たべる" }),
    worker("hd_lookup_stats_record", { term: "食べる", reading: "たべる" }),
    worker("hd_lookup_stats_record", { term: "猫" }),
    worker("hd_lookup_stats_read", { term: "食べる", reading: "たべる" }),
    worker("hd_lookup_stats_record", { term: " ", reading: "" }),
    worker("hd_lookup_stats_read", { term: "食べる", reading: 5 }),
    worker("hd_word_status_override", { headword: "食べる", status: "known" }),
    worker("hd_word_status_override", { headword: "食べる", status: "known" }),
    worker("hd_word_status_override", { headword: "猫", status: "ignored" }),
    worker("hd_word_status_override", { headword: "猫", status: null }),
    worker("hd_word_status_override", { headword: "猫", status: "learning" }),
    worker("hd_word_status_override", { headword: "", status: "known" }),
    worker("hd_word_status_override", { headword: "猫" }),
    // Settings' pasted known words (0.5.1): appended once, moved out of ignored.
    worker("hd_word_status_override", { headword: "鳥", status: "ignored" }),
    worker("hd_known_words_add", { text: " 食べる \r\n鳥\r\n\n学校\n学校\n " }),
    worker("hd_known_words_add", { text: "食べる\n学校" }),
    worker("hd_known_words_add", { text: "" }),
    worker("hd_known_words_add", { text: 3 }),
    worker("hd_known_words_add", { text: "犬", profileId: "en" }),
    // A request naming a profile: on records without profiles only Japanese is one.
    worker("hd_word_status_override", { headword: "犬", status: "known", profileId: "ja" }),
    worker("hd_word_status_override", { headword: "犬", status: "ignored", profileId: "en" }),
    worker("hd_word_status_override", { headword: "犬", status: "ignored", profileId: "" }),
    worker("hd_lookup_stats_record", { term: "猫", profileId: "ja" }),
    worker("hd_lookup_stats_record", { term: "猫", profileId: "en" }),
    worker("hd_options_write", { baseRevision: 7, options: { scanLength: 12 }, profileId: "en" }),
    worker("hd_custom_read", { profileId: "en" }),
    worker("hd_custom_cas", { baseDocumentRevision: 1, baseRevision: 5, text: "", profileId: "en" }),
    async () => worker("hd_custom_cas", { baseDocumentRevision: 1, baseRevision: 5, ...await custom(""), profileId: "en" }),
    async () => worker("hd_custom_cas", { baseDocumentRevision: 1, baseRevision: 5, ...await custom(""), language: "fr" }),
    worker("hd_state_cas", { baseRevision: 0, dictionaries: LIBRARY, profileId: "en" }),
    worker("hd_state_cas", { baseRevision: 6, dictionaries: LIBRARY, profileId: "en" }),
    worker("hd_options_write", { baseRevision: 7, options: { showLookupCounts: false } }),
    worker("hd_lookup_stats_record", { term: "食べる", reading: "たべる" }),
    worker("hd_bogus"),
  ];
  return current.map((step) => (Array.isArray(step) ? step : ["current", step]));
}

// Both sides over the same stored records: each step's reply, and the shared
// storage after it, must agree. Returns the outcomes the steps reached.
async function compare(t, seed, build) {
  const extension = await bootExtension(seed);
  const dir = mkdtempSync(join(tmpdir(), "hachidori-relay-settings-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  writeFileSync(join(dir, "storage.json"), JSON.stringify(seed));
  const relay = await startAnkiRelayServer({ extraArgs: ["--host", "--data-dir", dir] });
  t.after(() => relay.close());
  // A reader that predates Templates says linked-anki-v1 (the extension's sharing smoke test).
  const capabilities = { legacy: ["linked-anki-v1"], current: ["linked-anki-v2"] };
  const sockets = {};
  for (const [kind, list] of Object.entries(capabilities)) {
    sockets[kind] = await connectClient(t, relay.port, "/link");
    sockets[kind].send(JSON.stringify({ kind: "hello", protocol: 1, version: "0.9.0", name: kind, capabilities: list }));
    await untilKind(sockets[kind], "hello");
  }
  const relayStorage = () => {
    const stored = JSON.parse(readFileSync(join(dir, "storage.json"), "utf8"));
    return Object.fromEntries(Object.entries(stored).filter(([key]) => SHARED.includes(key) || key.startsWith("lookupStats:")));
  };

  let counter = 0;
  const outcomes = new Set();
  const replies = [];
  for (const [client, step] of build(extension)) {
    const message = typeof step === "function" ? await step() : step;
    const expected = await extension.context.dispatchSharedRequest(structuredClone(message), client, capabilities[client]);
    counter += 1;
    sockets[client].send(JSON.stringify({ kind: "request", id: counter, message }));
    const reply = await untilKind(sockets[client], "reply");
    assert.equal(reply.id, counter);
    const label = `${client} ${JSON.stringify(message).slice(0, 160)}`;
    assert.deepEqual(comparable(reply.response), comparable(expected), label);
    assert.deepEqual(comparable(relayStorage()), comparable(await extension.read()), `storage after ${label}`);
    replies.push(expected);
    outcomes.add(expected.ok === false ? (expected.conflict || expected.stale || expected.protected ? "refused" : "failed") : "ok");
  }
  return { outcomes, extension, replies };
}

test("the relay keeps settings as the extension's service worker does", { skip, timeout: 120_000 }, async (t) => {
  const { outcomes, extension } = await compare(t, SEED, (booted) => steps(booted.context));
  // The sequence covers commits, refusals and errors.
  assert.deepEqual([...outcomes].sort(), ["failed", "ok", "refused"]);
  const final = await extension.read();
  assert.equal(final.dictionaryState.revision, 6);
  assert.ok(Object.keys(final).some((key) => key.startsWith("lookupStats:")));
});

// Records an earlier release stored, or a damaged one: upstream's migration
// cases at d41c37a (reader-options, custom-button-settings,
// experimental-features, anki and word-status-overrides tests) as the stored
// state the next write starts from.
const LEGACY_ANKI = {
  url: "http://127.0.0.1:8765", apiKey: "secret", deck: "Mining::Japanese", model: "Kiku", tags: ["hachidori", "custom"],
  fields: { expression: "Expression", reading: "", definition: "", sentence: "", frequency: "", pitch: "", audio: "", screenshot: "Picture" },
  duplicateScope: "deck", duplicateBehavior: "overwrite", captureScreenshot: true,
  fieldTemplates: { Expression: { value: "{expression}", overwriteMode: "coalesce" }, Picture: { value: "{screenshot}", overwriteMode: "overwrite" } },
};
const EARLIER = [
  { options: { revision: 2, hoverDelayMs: 250, definitionBlurEnabled: true,
    experimental: { mediaMining: true, youtubeMining: true, googleDocs: false }, customPopupJavascript: "popup.dataset.x = 1;",
    mediaCapture: { enabled: true }, anki: LEGACY_ANKI, customLinks: [{ label: "Jisho", url: "https://jisho.org/search/%w" }] },
  wordStatusOverrides: "garbage" },
  { options: { revision: 1, definitionBlurEnabled: true, definitionBlurCountEnabled: false, anki: { url: "javascript:alert(1)" } },
    wordStatusOverrides: { revision: -1, known: "猫", ignored: null } },
  { options: { revision: 1, anki: { ...LEGACY_ANKI, url: "https://user:password@anki.example/" } },
    wordStatusOverrides: { revision: 4, known: ["猫", "", 7, "猫", "犬"], ignored: ["犬", "さん"] } },
  { wordStatusOverrides: { revision: 2, known: ["__proto__"], ignored: ["constructor"],
    languages: { en: { known: ["cat", 3], ignored: "x" }, fr: "garbage", de: { known: [], ignored: [] } } } },
];

test("the relay reads an earlier release's records as the extension does", { skip, timeout: 120_000 }, async (t) => {
  const worker = (type, fields = {}) => ({ target: "hoshidicts-worker", type, requestId: type, ...fields });
  const runs = [];
  for (const seed of EARLIER) {
    runs.push(await compare(t, seed, (booted) => {
      const write = (options) => async () => worker("hd_options_write",
        { baseRevision: (await booted.read()).options?.revision ?? 0, options });
      const { url, ...withoutUrl } = LEGACY_ANKI;
      return [
        ["current", worker("hd_state_read")],
        ["current", worker("hd_custom_read")],
        ["current", write({ scanLength: 13 })],
        ["legacy", write({ scanLength: 14 })],
        ["current", write({ definitionBlurEnabled: false })],
        ["current", write({ definitionBlurEnabled: "yes" })],
        ["current", write({ anki: { ...LEGACY_ANKI, url: " https://anki.example:443/connect?profile=Japanese#ignored " } })],
        ["current", write({ anki: { ...LEGACY_ANKI, url: "http://localhost:8765/" } })],
        ["current", write({ anki: withoutUrl })],
        ["current", write({ anki: { ...LEGACY_ANKI, url: "file:///tmp/anki" } })],
        ["legacy", write({ anki: { ...LEGACY_ANKI, deck: "Default" } })],
        ["current", worker("hd_word_status_override", { headword: "鳥", status: "known" })],
        ["current", worker("hd_word_status_override", { headword: "犬", status: "ignored" })],
        ["current", worker("hd_word_status_override", { headword: "__proto__", status: null })],
        ["current", worker("hd_word_status_override", { headword: "constructor", status: "known" })],
      ];
    }));
  }
  // The migrations ran: the first write over the oldest record renames, drops and
  // moves its settings; the URL cases normalise or refuse; the damaged word marks
  // start over at their normalised revision.
  const [oldest] = runs;
  const migrated = oldest.replies[2].options;
  assert.equal(oldest.replies[2].ok, true, JSON.stringify(oldest.replies[2]));
  assert.equal(migrated.definitionBlurCountEnabled, true);
  for (const key of ["hoverDelayMs", "definitionBlurEnabled", "mediaCapture"]) assert.equal(Object.hasOwn(migrated, key), false, key);
  assert.equal(Object.hasOwn(migrated.experimental, "mediaMining"), false);
  // 0.5.1: the popup script becomes one user script; the old experimental video
  // and Docs switches are not features any more.
  assert.equal(Object.hasOwn(migrated, "customPopupJavascript"), false);
  assert.equal(migrated.userScripts.length, 1);
  assert.deepEqual(Object.keys(migrated.experimental).sort(), ["crunchyrollMining", "multipleLanguages", "netflixMining"]);
  assert.equal(migrated.anki.templates.length, 1);
  assert.equal(migrated.anki.templates[0].deck, "Mining::Japanese");
  assert.deepEqual(migrated.customButtons.map((button) => button.id), ["legacy-link-1"]);
  assert.equal(oldest.replies[4].options.definitionBlurCountEnabled, false);
  assert.equal(oldest.replies[5].ok, false);
  assert.equal(oldest.replies[6].options.anki.url, "https://anki.example/connect?profile=Japanese");
  assert.equal(oldest.replies[7].options.anki.url, "http://localhost:8765");
  assert.equal(oldest.replies[8].options.anki.url, "http://127.0.0.1:8765");
  assert.equal(oldest.replies[9].ok, false);
  assert.equal(runs[1].replies[2].options.anki.url, "");
  assert.equal(runs[1].replies[2].options.definitionBlurCountEnabled, false);
  assert.equal(oldest.replies[11].revision, 1);
  assert.equal(runs[2].replies[11].revision, 5);
  assert.equal((await runs[2].extension.read()).wordStatusOverrides.known.join(), "猫,鳥,constructor");
});
