// SPDX-License-Identifier: GPL-3.0-or-later
// Host mode's Anki requests checked against the extension itself. One fake
// AnkiConnect (anki-fake.mjs) holds a small collection; the extension's service
// worker (booted in Node with its own smoke-test fakes) and the relay both use it,
// and each answers the same linked-browser requests: status, view, browse,
// maturity, word status, preflight, submission, and Settings' discovery and setup
// check. Each reply, the notes each side asked Anki to check, the collection and
// media a submission leaves (each side writes from the same start), and the
// stored duplicate index after it, must agree. Needs a Hachidori checkout (HACHIDORI_SOURCE, by default
// ../hachidori), a relay built with host mode, and jsdom for the extension's field
// rendering, found where Hachidori's own tests look (HACHIDORI_JSDOM, its
// test/tooling, or ~/.cache/hachidori-e2e).

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs, { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire, syncBuiltinESMExports } from "node:module";
import { homedir, tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { startAnkiConnect } from "./anki-fake.mjs";
import { relayBinary, startAnkiRelayServer } from "./anki-relay-server.mjs";
import { connectClient, untilKind } from "./relay-socket.mjs";

const REPO = fileURLToPath(new URL("../..", import.meta.url));
const HACHIDORI = resolve(process.env.HACHIDORI_SOURCE ?? join(REPO, "..", "hachidori"));
const HOST_MODE = execFileSync(relayBinary(), ["--help"], { encoding: "utf8" }).includes("--host");
const JSDOM_PATHS = [process.env.HACHIDORI_JSDOM, join(HACHIDORI, "test/tooling"), join(homedir(), ".cache/hachidori-e2e")].filter(Boolean);
const jsdomEntry = (() => {
  try {
    return createRequire(import.meta.url).resolve("jsdom", { paths: JSDOM_PATHS });
  } catch {
    return null;
  }
})();
const missing = [
  ...(HOST_MODE ? [] : ["a relay built with host mode"]),
  ...(jsdomEntry ? [] : ["jsdom"]),
  ...["test/extension-smoke/fakes.mjs", "test/anki-connect-fake.mjs"].map((path) => join(HACHIDORI, path)).filter((path) => !existsSync(path)),
];
const skip = missing.length > 0 && `needs ${missing.join(", ")}`;
const INDEX_KEY = "ankiDuplicateIndex";

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

const template = (value, overwriteMode = "coalesce") => ({ value, overwriteMode });
const ankiOptions = (url, fields = {}) => ({
  url, apiKey: "", deck: "Mining", model: "Kiku", tags: ["hachidori"],
  fields: Object.fromEntries(["expression", "reading", "definition", "sentence", "frequency", "pitch", "audio", "screenshot"]
    .map((field) => [field, ""])),
  duplicateScope: "all", duplicateBehavior: "prevent", captureScreenshot: true,
  fieldTemplates: {
    Expression: template("{expression}"), ExpressionReading: template("{reading}"), MainDefinition: template("{main-definition}"),
    Sentence: template("{cloze-prefix}<b>{cloze-body}</b>{cloze-suffix}"), Glossary: template("{glossary}"),
  },
  ...fields,
});

// The extension's engine as preflight reads it: the relay's generation, no styles.
async function bootExtension(seed, ankiUrl, engineState) {
  readSourcesAsLf();
  const fakes = await import(pathToFileURL(join(HACHIDORI, "test/extension-smoke/fakes.mjs")));
  const { createAnkiGateway } = await import(pathToFileURL(join(HACHIDORI, "extension/anki.js")));
  const { fetchAnkiIndex } = await import(pathToFileURL(join(HACHIDORI, "extension/anki-index.js")));
  const { buildAnkiResourceFields } = await import(pathToFileURL(join(HACHIDORI, "extension/anki-resources.js")));
  const { exportAnkiAudio } = await import(pathToFileURL(join(HACHIDORI, "extension/anki-audio.js")));
  const { createAudioRepository } = await import(pathToFileURL(join(HACHIDORI, "extension/audio-repository.js")));
  const { JSDOM } = createRequire(import.meta.url)(jsdomEntry);
  const dom = new JSDOM("<!doctype html><html><body></body></html>");
  const realFetch = globalThis.fetch;
  fakes.installFakeIndexedDB();
  fakes.installFetch();
  fakes.installNavigator();
  fakes.existingOffscreenStage();
  const fakeFetch = globalThis.fetch;
  const fetch = (url, init) => (String(url).startsWith(ankiUrl) ? realFetch(url, init) : fakeFetch(url, init));
  const storage = fakes.makeStorage();
  await storage.api().local.set(structuredClone(seed));
  const bus = fakes.makeBus();
  // The offscreen document's index refresh (`refreshAnkiIndex`), over the same gateway.
  const gateway = createAnkiGateway({ fetch: realFetch });
  const answer = (message, sendResponse, work) => {
    Promise.resolve().then(work).then(
      (result) => sendResponse({ type: `${message.type}_result`, requestId: message.requestId, ok: true, ...result }),
      (error) => sendResponse({ type: `${message.type}_result`, requestId: message.requestId, ok: false, error: String(error.message) }),
    );
    return true;
  };
  bus.addListener("engine", (message, sender, sendResponse) => {
    if (message?.target !== "hoshidicts-offscreen") return false;
    if (message.type === "hd_status") {
      return answer(message, sendResponse, () => ({ ready: true, loading: false, generation: engineState.generation }));
    }
    if (message.type === "hd_styles") return answer(message, sendResponse, () => ({ generation: engineState.generation, styles: [] }));
    return answer(message, sendResponse, () => { throw new Error(`no engine request ${message.type} in this oracle`); });
  });
  // The offscreen document's pronunciation export. Node cannot decode audio, so the
  // stand-in `Audio` accepts every recording, as a browser would these.
  class Audio {
    load() { if (this.src) setTimeout(() => this.onloadeddata?.()); }
    pause() {}
    removeAttribute() { this.src = ""; }
  }
  class FileReader {
    readAsDataURL(blob) {
      blob.arrayBuffer().then((bytes) => {
        this.result = `data:${blob.type || "application/octet-stream"};base64,${Buffer.from(bytes).toString("base64")}`;
        this.onload?.();
      }, (error) => { this.error = error; this.onerror?.(); });
    }
    abort() {}
  }
  const audioWindow = { Audio, FileReader, URL };
  const repository = createAudioRepository({ window: audioWindow, fetch: realFetch });
  bus.addListener("anki-audio", (message, sender, sendResponse) => {
    if (message?.target !== "hachidori-anki-render" || message.type !== "hd_anki_audio") return false;
    return answer(message, sendResponse, () => exportAnkiAudio(audioWindow, repository, message, AbortSignal.timeout(30_000)));
  });
  // The offscreen document's field rendering (`createAnkiOffscreenService`), in jsdom.
  bus.addListener("anki-fields", (message, sender, sendResponse) => {
    if (message?.target !== "hachidori-anki-render" || message.type !== "hd_anki_fields") return false;
    return answer(message, sendResponse, () => buildAnkiResourceFields(message.request, message.templates, {
      document: dom.window.document, dictionaryPaths: message.dictionaryPaths, audio: message.audio,
      compactGlossary: message.compactGlossary === true,
      styles: async () => {
        if (engineState.generation !== message.request.generation) throw new Error("Dictionary styles changed during Anki preparation.");
        return [];
      },
    }));
  });
  bus.addListener("anki-render", (message, sender, sendResponse) => {
    if (message?.target !== "hachidori-anki-render" || message.type !== "hd_anki_index_refresh") return false;
    const { source } = message;
    const invoke = (action, params, timeoutMs) => gateway.invoke(action, params, source.apiKey, timeoutMs, source.url);
    fetchAnkiIndex(invoke, source).then(
      (rows) => sendResponse({ type: "hd_anki_index_refresh_result", requestId: message.requestId, ok: true, rows }),
      (error) => sendResponse({ type: "hd_anki_index_refresh_result", requestId: message.requestId, ok: false, error: String(error.message) }),
    );
    return true;
  });
  const context = fakes.loadBackgroundScript({
    chrome: fakes.makeChrome("sw", bus, storage, fakes.makeAlarms()), console, fetch, setTimeout, clearTimeout, URL,
    // This realm's errors, so the worker's `instanceof Error` sees the modules' throws.
    Error, TypeError, Promise, AbortController,
  });
  return { context, close: () => dom.window.close(), index: async () => (await storage.api().local.get(INDEX_KEY))[INDEX_KEY] ?? null };
}

// A host start's random linked-key prefix and the configuration digest (whose JSON
// key order is the engine's own) are each side's; times are compared as present.
function comparable(value) {
  const walk = (item, key) => {
    if (typeof item === "string") return item.replace(/^linked:[0-9a-f-]{36}:[0-9a-f]{64}$/u, "linked-key");
    if (typeof item === "number" && ["refreshedAt", "startedAt", "finishedAt"].includes(key)) return "time";
    if (Array.isArray(item)) return item.map((entry) => walk(entry));
    if (item && typeof item === "object") return Object.fromEntries(Object.entries(item).map(([field, entry]) => [field, walk(entry, field)]));
    return item;
  };
  return walk(JSON.parse(JSON.stringify(value ?? null)));
}

// Waits until a side's stored index stops moving and no pull it reserved is open.
async function settled(read) {
  let previous;
  let quiet = 0;
  for (let attempt = 0; attempt < 400 && quiet < 4; attempt += 1) {
    await new Promise((resolveTimer) => setTimeout(resolveTimer, 25));
    const current = JSON.stringify(await read());
    const open = current !== "null" && JSON.parse(current).attempt && JSON.parse(current).attempt.finishedAt === undefined;
    quiet = current === previous && !open ? quiet + 1 : 0;
    previous = current;
  }
  return JSON.parse(previous ?? "null");
}

const anki = (type, request, extra = {}) => ({ target: "hachidori-anki", type, requestId: type, ...(request === undefined ? {} : { request }), ...extra });
const worker = (type, fields = {}) => ({ target: "hoshidicts-worker", type, requestId: type, ...fields });
const term = (expression) => ({ term: { expression, reading: "", glossary: ["dropped"] } });
const structured = (content) => JSON.stringify([{ type: "structured-content", content }]);
// A dictionary result as a hover mines it: senses from two dictionaries (one
// structured), frequencies, a pitch accent and the sentence it was found in.
const mined = (expression, reading, generation, fields = {}) => ({
  generation, sentence: `海に${expression}がいる。`, matchOffset: 2, matched: expression,
  dictionaryAliases: { B: "Bee <dictionary>" }, trace: [{ name: "polite" }, { name: "past" }],
  term: {
    expression, reading, rules: "n", termTags: ["news"],
    glossaries: [
      { dictionary: "A", definitionTags: "common n", termTags: "", glossary: JSON.stringify(["a whale", "<b>literal</b>\nsecond line"]) },
      { dictionary: "B", definitionTags: "", termTags: "P", glossary: structured([
        { tag: "div", data: { content: "sense" }, style: { fontWeight: "bold", marginLeft: "1em" }, content: [
          { tag: "span", content: "大きい" }, { tag: "ruby", content: [expression, { tag: "rt", content: reading }] }] },
        { tag: "ul", content: [{ tag: "li", content: "one" }, { tag: "li", content: [{ tag: "i", content: "two" }] }] },
        { tag: "a", href: "?query=海&wildcards=off", content: "海" },
        { tag: "table", content: [{ tag: "tr", content: [{ tag: "th", content: "x" }, { tag: "td", colSpan: 2, content: "y" }] }] },
        { tag: "details", content: [{ tag: "summary", content: "more" }, "hidden"] },
      ]) },
      { dictionary: "A", definitionTags: "", termTags: "", glossary: structured("plain text\nwith a break") },
    ],
    frequencies: [
      { dictionary: "F", frequencyMode: "rank-based", frequencies: [{ value: 1200, displayValue: "1200" }, { value: 30, displayValue: "30㋕" }] },
      { dictionary: "G", frequencyMode: "occurrence-based", frequencies: [{ value: 88, displayValue: null }] },
    ],
    pitches: [{ dictionary: "P", pitches: [{ position: 0, nasal: [], devoice: [], tags: [] }, { position: 2, nasal: [1], devoice: [], tags: ["n"] }], transcriptions: [] }],
  },
  ...fields,
});
const preflight = (expression, reading, fields = {}) => (side) => anki("hd_anki_preflight",
  { configKey: side.key, ...mined(expression, reading, side.generation, fields) });
// A submission and the media the linked browser sends with it (none: no envelope).
const submit = (expression, reading, fields = {}, clientMedia = {}) => (side) => ({
  ...anki("hd_anki_submit", { configKey: side.key, ...mined(expression, reading, side.generation, typeof fields === "function" ? fields(side) : fields) }),
  ...(clientMedia === null ? {} : { clientMedia }),
});
const SHOT = { token: "shot-1", filename: "hachidori-screenshot-0f8fad5b-d9cb-469f-a165-70867728950e.jpg" };
const JPEG = "/9j/4AAQSkZJRg==";
const source = (id, type, url) => ({ id, type, enabled: true, url, voice: "" });
// A source as a linked page names it: JSON in the key order its host serves it in.
const sourceKey = (side, value) => JSON.stringify(side.name === "relay"
  ? Object.fromEntries(Object.entries(value).sort(([left], [right]) => (left < right ? -1 : 1))) : value);
const markers = {
  Expression: template("{expression}"), ExpressionFurigana: template("{furigana}"), ExpressionReading: template("{reading}"),
  MainDefinition: template("{glossary-first}"), SelectionText: template("{glossary-plain-no-dictionary}"),
  Sentence: template("{cloze-prefix}<b>{cloze-body}</b>{cloze-suffix}"), Glossary: template("{glossary}"),
  Hint: template("{glossary-brief}"), MiscInfo: template("{tags} {part-of-speech} {dictionary} {dictionary-alias}"),
  PitchPosition: template("{pitch-position}"), PitchCategories: template("{pitch-accent-categories}"),
  DefinitionPicture: template("{pitch-accent-graphs}"), Frequency: template("{frequencies}"),
  FreqSort: template("{frequency-harmonic-rank}|{frequency-average-occurrence}"), Picture: template("{screenshot}"),
};

// Notes written, updated and refused, with screenshots and pronunciations.
// `mutates`: each side writes to the collection as it was before the step.
function writes(fake) {
  const optionsWrite = (patch) => (side) => worker("hd_options_write", { baseRevision: side.revision, options: patch(side) });
  const audio = source("audio", "custom", `${fake.url}/audio/{term}.mp3`);
  const list = source("list", "custom-json", `${fake.url}/list/{reading}`);
  const missing = source("gone", "custom", `${fake.url}/missing/{term}`);
  const templates = (fields = {}) => ({ ...markers, ExpressionAudio: template("{audio}"), ...fields });
  const configure = (audioSources, fields = {}) => ({
    message: optionsWrite(() => ({ audioSources, anki: ankiOptions(fake.url, { fieldTemplates: templates(), ...fields }) })), revision: true, key: true });
  return [
    configure([audio]),
    { message: preflight("鯨", "くじら") },
    { message: submit("鯨", "くじら", { screenshot: SHOT }, { screenshot: { ...SHOT, data: JPEG } }), mutates: true },
    { message: submit("鯨", "くじら"), mutates: true },
    { message: preflight("鯨", "くじら") },
    { message: submit("鯉", "こい", { screenshot: SHOT }), mutates: true },
    { message: submit("鯉", "こい", {}, null), mutates: true },
    { message: submit("鯉", "こい", { screenshot: SHOT }, { screenshot: { ...SHOT, data: "nope" } }), mutates: true },
    { message: submit("鯉", "こい", { generation: 999 }), mutates: true },
    { message: (side) => ({ ...submit("鯉", "こい")(side), request: { ...submit("鯉", "こい")(side).request, configKey: "linked:other:abc" } }), mutates: true },
    { message: submit("鯉", "こい", { screenshot: SHOT, captureUnavailable: ["screenshot"] }), mutates: true },
    // Another client adds the note between the check and the write: Anki refuses it.
    { before: () => { fake.state.racing.push("鰹"); },
      message: submit("鰹", "かつお", { screenshot: SHOT }, { screenshot: { ...SHOT, data: JPEG } }), mutates: true },
    // A pronunciation list whose first recording is missing, then a source that fails.
    configure([list]),
    { message: submit("鮫", "さめ"), mutates: true },
    { message: submit("鮪", "まぐろ", (side) => ({ audioSelection: { sourceId: "list", sourceKey: sourceKey(side, list), expression: "鮪",
      reading: "まぐろ", index: 1, url: `${fake.url}/audio/${encodeURIComponent("まぐろ")}.mp3`, name: "Speaker" } })), mutates: true },
    { message: submit("鱒", "ます", (side) => ({ audioSelection: { sourceId: "list", sourceKey: sourceKey(side, list), expression: "鱒",
      reading: "ます", index: 0, url: "http://elsewhere/x", name: "Gone" } })), mutates: true },
    configure([missing]),
    { message: submit("鰯", "いわし"), mutates: true },
    // The first field's pronunciation is part of the note's identity.
    configure([audio], { fieldTemplates: templates({ Expression: template("{expression}{audio}") }) }),
    { message: preflight("鰻", "うなぎ") },
    { message: submit("鰻", "うなぎ"), mutates: true },
    { message: submit("鰻", "うなぎ"), mutates: true },
    // Overwrite the indexed duplicate, then add another beside it.
    configure([audio], { duplicateBehavior: "overwrite", fieldTemplates: templates({ MiscInfo: template("{tags}", "append") }) }),
    { message: submit("猫", "ねこ", { screenshot: SHOT }, { screenshot: { ...SHOT, data: JPEG } }), mutates: true },
    { message: submit("鯨", "くじら"), mutates: true },
    configure([audio], { duplicateBehavior: "new" }),
    { message: submit("猫", "ねこ"), mutates: true },
    { message: submit("猫", "ねこ", { allowDuplicate: true }), mutates: true },
  ];
}

// Each step builds its request from the side's own linked configuration key and
// options revision; `before` changes the collection once, before both sides run.
function steps(fake) {
  const browse = (fields) => (side) => anki("hd_anki_browse", { configKey: side.key, ...fields });
  const optionsWrite = (patch) => (side) => worker("hd_options_write", { baseRevision: side.revision, options: patch(side) });
  const note = (id) => fake.state.notes.find((entry) => entry.noteId === id);
  return [
    { message: anki("hd_anki_status"), key: true },
    { message: anki("hd_anki_status", undefined, { templateId: "default" }) },
    { message: anki("hd_anki_status", undefined, { templateId: "nope" }) },
    { message: anki("hd_anki_status", undefined, { templateId: "" }) },
    { message: anki("hd_anki_view", term("猫")) },
    { message: anki("hd_anki_view", term("犬")) },
    { message: anki("hd_anki_view", term("abc")) },
    { message: anki("hd_anki_view", term("a<b")) },
    { message: anki("hd_anki_view", term("魚")) },
    { message: anki("hd_anki_view", { term: {} }) },
    { message: anki("hd_anki_view", {}) },
    { message: anki("hd_anki_view", { ...term("猫"), templateId: "nope" }) },
    { message: anki("hd_anki_maturity", term("猫")) },
    { message: anki("hd_anki_word_status", { headwords: ["猫", "犬", "魚", "ABC", "鳥", ""] }) },
    { message: anki("hd_anki_word_status", { headwords: ["猫", 1] }) },
    { message: anki("hd_anki_word_status", {}) },
    { message: anki("hd_anki_screenshot") },
    { message: { target: "hachidori-anki", type: "hd_anki_status", requestId: { nested: true } } },
    { message: worker("hd_anki_discover", { model: "Kiku" }) },
    { message: worker("hd_anki_discover", { model: "Gone", url: "http://elsewhere", apiKey: "secret" }) },
    { message: worker("hd_anki_discover", { model: 5 }) },
    { message: worker("hd_anki_setup") },
    { message: worker("hd_anki_setup", { templateId: "nope" }) },
    { message: worker("hd_anki_setup", { templateId: 3 }) },
    { message: browse({ noteIds: [1, 3], expression: "猫" }) },
    { message: browse({ expression: "a<b" }) },
    { message: browse({ noteIds: [7, 7, 1] }) },
    { message: browse({ noteIds: [0] }) },
    { message: (side) => anki("hd_anki_browse", { configKey: `${side.key.slice(0, -64)}${"0".repeat(64)}`, expression: "猫" }) },
    { message: anki("hd_anki_browse", { configKey: "linked:other:abc", expression: "猫" }) },
    { message: anki("hd_anki_browse", { expression: "猫" }) },
    { message: anki("hd_anki_preflight", { ...term("猫"), configKey: "linked:other:abc" }) },
    // Preflight with the seeded Templates: new, a duplicate, and refusals.
    { message: preflight("鯨", "くじら") },
    { message: preflight("猫", "ねこ") },
    { message: preflight("鯨", "くじら", { generation: 999 }) },
    { message: preflight("鯨", "くじら", { generation: -1 }) },
    { message: preflight("鯨", "くじら", { term: { expression: "", reading: "" } }) },
    { message: preflight("鯨", "くじら", { term: { expression: "鯨" } }) },
    { message: (side) => anki("hd_anki_preflight", { ...mined("鯨", "くじら", side.generation) }) },
    { message: (side) => anki("hd_anki_preflight", { configKey: side.key }) },
    // Every kind of marker, then Smaller Anki cards.
    { message: optionsWrite(() => ({ anki: ankiOptions(fake.url, { fieldTemplates: markers }) })), revision: true, key: true },
    { message: preflight("鯨", "くじら") },
    { message: preflight("猫", "ねこ") },
    { message: preflight("あ", "") },
    { message: optionsWrite(() => ({ smallerAnkiCards: true })), revision: true, key: true },
    { message: preflight("鯨", "くじら") },
    { message: optionsWrite(() => ({ smallerAnkiCards: false })), revision: true, key: true },
    // The first field renders nothing.
    { message: optionsWrite(() => ({ anki: ankiOptions(fake.url, { fieldTemplates: { ...markers, Expression: template("{onyomi}") } }) })),
      revision: true, key: true },
    { message: preflight("鯨", "くじら") },
    // A first field the word index cannot key: Anki's own duplicate check decides.
    ...["prevent", "overwrite", "new"].flatMap((duplicateBehavior) => [
      { message: optionsWrite(() => ({ anki: ankiOptions(fake.url, { duplicateBehavior,
        fieldTemplates: { ...markers, Expression: template("{expression}{onyomi}") } }) })), revision: true, key: true },
      { message: preflight("猫", "ねこ") },
      { message: preflight("鯨", "くじら") },
    ]),
    { message: optionsWrite(() => ({ anki: ankiOptions(fake.url) })), revision: true, key: true },
    // 犬 leaves the collection: a repair finds nothing, and its row goes.
    { before: () => { note(2).fields.Expression = "狗"; }, message: browse({ noteIds: [2], expression: "犬" }), wordStatus: true },
    { message: anki("hd_anki_word_status", { headwords: ["犬", "猫"] }) },
    { message: anki("hd_anki_view", term("犬")) },
    // 鳥 gains a note: a repair records it.
    { before: () => { note(4).fields.Front = "x"; fake.state.notes.push({ ...structuredClone(note(7)), noteId: 8, mature: false }); },
      message: browse({ noteIds: [7], expression: "鳥" }), wordStatus: true },
    { message: anki("hd_anki_view", term("鳥")) },
    { message: optionsWrite(() => ({ definitionBlurAnkiMature: true })), revision: true },
    { message: anki("hd_anki_maturity", term("猫")) },
    { message: anki("hd_anki_maturity", term("狗")) },
    { message: anki("hd_anki_maturity", {}) },
    // Deck scope: another source, so the index is announced without a revision and pulled again.
    { message: optionsWrite((side) => ({ anki: ankiOptions(fake.url, { duplicateScope: "deck" }) })), revision: true, key: true, wordStatus: true },
    { message: anki("hd_anki_view", term("猫")) },
    { message: preflight("猫", "ねこ") },
    { message: preflight("ABC", "") },
    { message: anki("hd_anki_word_status", { headwords: ["猫", "ABC"] }) },
    { message: optionsWrite(() => ({ anki: ankiOptions(fake.url, { duplicateBehavior: "overwrite" }) })), revision: true, key: true },
    { message: anki("hd_anki_view", term("猫")) },
    { message: preflight("猫", "ねこ") },
    { message: preflight("鯨", "くじら") },
    { message: optionsWrite(() => ({ anki: ankiOptions(fake.url, { duplicateBehavior: "new" }) })), revision: true, key: true },
    { message: preflight("猫", "ねこ") },
    // Unconfigured: setup proposes the busiest recognised note type and its busiest deck.
    { message: optionsWrite(() => ({ anki: ankiOptions(fake.url, { model: "", fieldTemplates: null }) })), revision: true, key: true },
    { message: anki("hd_anki_status") },
    { message: anki("hd_anki_word_status", { headwords: ["猫"] }) },
    { message: worker("hd_anki_setup") },
    { message: anki("hd_anki_view", term("猫")) },
    { message: preflight("猫", "ねこ") },
    // AnkiConnect refuses every request.
    { before: () => { fake.state.refuse = true; }, message: optionsWrite(() => ({ anki: ankiOptions(fake.url) })), revision: true, key: true },
    { message: anki("hd_anki_status") },
    { message: worker("hd_anki_discover", { model: "Kiku" }) },
    { message: worker("hd_anki_setup") },
    { message: browse({ expression: "猫" }) },
    { message: preflight("鯨", "くじら") },
    { before: () => { fake.state.refuse = false; }, message: worker("hd_anki_setup") },
    ...writes(fake),
  ];
}

test("the relay answers Anki requests as the extension's service worker does", { skip, timeout: 180_000 }, async (t) => {
  const { answerAnkiConnect, AnkiConnectError } = await import(pathToFileURL(join(HACHIDORI, "test/anki-connect-fake.mjs")));
  const fake = await startAnkiConnect(answerAnkiConnect, AnkiConnectError);
  t.after(() => fake.close());
  const seed = { options: { revision: 1, anki: ankiOptions(fake.url) } };
  const engineState = { generation: 0 };
  const extension = await bootExtension(seed, fake.url, engineState);
  t.after(() => extension.close());
  const dir = mkdtempSync(join(tmpdir(), "hachidori-relay-anki-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  writeFileSync(join(dir, "storage.json"), JSON.stringify(seed));
  const relay = await startAnkiRelayServer({ extraArgs: ["--host", "--data-dir", dir] });
  t.after(() => relay.close());
  const socket = await connectClient(t, relay.port, "/link");
  socket.send(JSON.stringify({ kind: "hello", protocol: 1, version: "0.9.0", name: "linked", capabilities: ["linked-anki-v2"] }));
  await untilKind(socket, "hello");
  // The relay's dictionary generation, which the extension's fake engine reports too.
  socket.send(JSON.stringify({ kind: "request", id: 0, message: { target: "hoshidicts-offscreen", type: "hd_status", requestId: "generation" } }));
  const statusFrame = await untilKind(socket, "reply");
  engineState.generation = statusFrame.response.generation;
  assert.equal(typeof engineState.generation, "number", JSON.stringify(statusFrame));
  const relayIndex = async () => JSON.parse(readFileSync(join(dir, "storage.json"), "utf8"))[INDEX_KEY] ?? null;
  // The relay's frames up to its reply, and the word-status revisions among them.
  const untilReply = async () => {
    const announced = [];
    let frame = await socket.json("a reply frame");
    while (frame.kind !== "reply") {
      if (frame.kind === "word-status") announced.push(frame.revision);
      frame = await socket.json("a reply frame");
    }
    return { frame, announced };
  };

  const sides = {
    extension: { name: "extension", key: "", revision: 1, generation: engineState.generation, index: extension.index },
    relay: { name: "relay", key: "", revision: 1, generation: engineState.generation, index: relayIndex },
  };
  // Both pull the seeded Template's index at start.
  const initial = await settled(extension.index);
  assert.ok(initial?.snapshot?.rows.length > 0, JSON.stringify(initial));
  assert.deepEqual(comparable(await settled(relayIndex)), comparable(initial), "the index each side pulled at start");

  let counter = 0;
  const seen = [];
  const checks = [];
  let announcements = 0;
  const browsed = (from) => fake.state.log.slice(from).filter((entry) => entry.action === "guiBrowse").map((entry) => entry.params.query);
  // The notes a side asked Anki to check: the fields its Templates rendered.
  const checked = (from) => fake.state.log.slice(from).filter((entry) => entry.action.startsWith("canAddNotes")).map((entry) => entry.params);
  for (const step of steps(fake)) {
    step.before?.();
    const replies = {};
    const queries = {};
    const notes = {};
    const collection = () => structuredClone({ notes: fake.state.notes, media: fake.state.media });
    const start = step.mutates ? collection() : null;
    let written = null;
    for (const [name, side] of Object.entries(sides)) {
      const message = typeof step.message === "function" ? step.message(side) : step.message;
      const from = fake.state.log.length;
      if (name === "relay" && start) {
        written = collection();
        Object.assign(fake.state, structuredClone(start));
      }
      if (name === "extension") {
        replies[name] = await extension.context.dispatchSharedRequest(structuredClone(message), "linked", ["linked-anki-v2"]);
      } else {
        counter += 1;
        socket.send(JSON.stringify({ kind: "request", id: counter, message }));
        const { frame, announced } = await untilReply();
        announcements += announced.length;
        assert.equal(frame.id, counter);
        replies[name] = frame.response;
        if (step.wordStatus) {
          // The announcement may follow the reply when a pull runs behind it.
          const stored = await settled(relayIndex);
          assert.ok(announced.length > 0 || step.revision, `a word-status frame for ${JSON.stringify(message).slice(0, 120)}`);
          for (const revision of announced) assert.ok(revision === null || revision <= stored.rowRevision);
        }
      }
      if (step.key) side.key = "";
      if (step.revision && replies[name].ok) side.revision = replies[name].options.revision;
      queries[name] = browsed(from);
      notes[name] = checked(from);
      side.stored = await settled(side.index);
      if (step.key && !side.key) {
        // A Template change makes a new configuration key; read it as a linked page would.
        const status = name === "extension"
          ? await extension.context.dispatchSharedRequest(anki("hd_anki_status"), "linked", ["linked-anki-v2"])
          : await (async () => {
            counter += 1;
            socket.send(JSON.stringify({ kind: "request", id: counter, message: anki("hd_anki_status") }));
            return (await untilReply()).frame.response;
          })();
        side.key = status.configKey ?? "";
      }
    }
    const label = JSON.stringify(typeof step.message === "function" ? step.message(sides.relay) : step.message).slice(0, 160);
    assert.deepEqual(comparable(replies.relay), comparable(replies.extension), label);
    seen.push(replies.extension);
    assert.deepEqual(queries.relay, queries.extension, `Anki browser queries for ${label}`);
    assert.deepEqual(notes.relay, notes.extension, `the notes checked for ${label}`);
    if (written) assert.deepEqual(collection(), written, `the collection after ${label}`);
    checks.push(...notes.extension);
    assert.deepEqual(comparable(sides.relay.stored), comparable(sides.extension.stored), `the stored index after ${label}`);
  }
  // The sequence reaches what matters: cached duplicates, repairs, Anki's browser,
  // word status, a setup proposal and refusals, and the relay announced index changes.
  if (process.env.ANKI_ORACLE_DUMP) console.log(JSON.stringify(seen, null, 1));
  assert.ok(seen.filter((reply) => reply.type === "hd_options_write_result").every((reply) => reply.ok), "every options write commits");
  const of = (type) => seen.filter((reply) => reply.type === `${type}_result`);
  assert.ok(of("hd_anki_view").some((reply) => reply.state === "duplicate" && reply.mature === true && reply.noteIds.length > 1));
  assert.ok(of("hd_anki_browse").some((reply) => reply.opened === true && reply.repaired === true));
  assert.ok(of("hd_anki_browse").some((reply) => reply.opened === false && reply.repaired === true));
  assert.ok(of("hd_anki_word_status").some((reply) => reply.statuses?.includes("known") && reply.statuses.includes("learning")));
  assert.ok(of("hd_anki_maturity").some((reply) => reply.mature === true));
  assert.ok(of("hd_anki_setup").some((reply) => reply.proposal?.status === "configured" && reply.proposal.deck === "Mining"));
  assert.ok(of("hd_anki_setup").some((reply) => reply.outcome?.status === "already-configured"));
  assert.ok(of("hd_anki_status").some((reply) => reply.ok && reply.available === false && reply.error.includes("denied permission")));
  assert.ok(of("hd_anki_discover").some((reply) => reply.connected === true && reply.fields.length > 0));
  assert.ok(announcements > 0);
  assert.ok(of("hd_anki_preflight").some((reply) => reply.state === "addable" && reply.canAdd === true));
  assert.ok(of("hd_anki_preflight").some((reply) => reply.state === "duplicate" && reply.canAdd === false && reply.noteIds?.length > 0));
  assert.ok(of("hd_anki_preflight").some((reply) => reply.action === "overwrite" && reply.canAdd === true));
  assert.ok(of("hd_anki_preflight").some((reply) => reply.ok === false && reply.error.includes("is empty for this result")));
  assert.ok(of("hd_anki_preflight").some((reply) => reply.ok === false && reply.error.includes("generation changed")));
  assert.ok(of("hd_anki_preflight").some((reply) => reply.screenshot === true));
  const submitted = of("hd_anki_submit");
  for (const state of ["added", "updated", "duplicate"]) assert.ok(submitted.some((reply) => reply.state === state), state);
  assert.ok(submitted.some((reply) => reply.state === "duplicate" && reply.error?.startsWith("Anki already has a note")));
  assert.ok(submitted.some((reply) => reply.ok === false && reply.error.includes("client-media")));
  assert.ok(submitted.some((reply) => reply.warnings?.some((warning) => warning.startsWith("Pronunciation: "))));
  assert.ok(submitted.some((reply) => reply.state === "added" && reply.warnings?.length === 0));
  assert.ok(Object.keys(fake.state.media).some((name) => name.startsWith("hachidori-screenshot-")), "a screenshot was stored");
  assert.ok(Object.keys(fake.state.media).some((name) => /^hachidori_[0-9a-f]{64}\.mp3$/u.test(name)), "a pronunciation was stored");
  const rendered = checks.flatMap((params) => params.notes).map((note) => note.fields);
  assert.ok(rendered.some((fields) => fields.Glossary?.includes("<ul") && fields.DefinitionPicture?.includes("<svg")), "rich fields were rendered");
  assert.ok(rendered.some((fields) => fields.Glossary?.includes("<ruby") && !fields.Glossary.includes("gsm-hoshidicts-glossary-content")),
    "a compact glossary was rendered");
});
