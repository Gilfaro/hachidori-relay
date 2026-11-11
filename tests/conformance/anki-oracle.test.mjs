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
import { oracleSkip } from "./oracle-skip.mjs";

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
const skip = oracleSkip(missing);
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

// Chromium serialises a scoped rule's `[data-dictionary=ident]` as a quoted
// string, as the relay writes it; jsdom keeps the text the extension set.
const browserCss = (value) => JSON.parse(JSON.stringify(value)
  .replace(/\[data-dictionary=([A-Za-z_][\w-]*)\]/gu, (_, name) => `[data-dictionary=\\"${name}\\"]`));

// The extension's engine as preflight reads it: the relay's generation, styles and media.
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
    if (message.type === "hd_styles") return answer(message, sendResponse, () => engineState.styles());
    // Dictionary images: the relay's engine holds the same dictionaries.
    if (message.type === "hd_media") return answer(message, sendResponse, () => engineState.ask(message));
    // The sentence's words for {sentence-furigana}, split by the same engine.
    if (message.type === "hd_segment") return answer(message, sendResponse, () => engineState.ask(message));
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
      compactGlossary: message.compactGlossary === true, compactGlossaries: message.compactGlossaries === true,
      styles: async () => {
        const reply = await engineState.styles();
        if (reply.generation !== message.request.generation) throw new Error("Dictionary styles changed during Anki preparation.");
        return reply.styles;
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
  // The background bundle leaves profile-state.js out; the extension's own
  // profile tests hand its functions in (profiles.test.mjs).
  const profileState = await import(pathToFileURL(join(HACHIDORI, "extension/profile-state.js")));
  const { profileTemplateKey } = await import(pathToFileURL(join(HACHIDORI, "extension/anki-worker.js")));
  const context = fakes.loadBackgroundScript({
    chrome: fakes.makeChrome("sw", bus, storage, fakes.makeAlarms()), console, fetch, setTimeout, clearTimeout, URL,
    // This realm's errors, so the worker's `instanceof Error` sees the modules' throws.
    Error, TypeError, Promise, AbortController,
    ...Object.fromEntries(Object.entries(profileState).filter(([, value]) => typeof value === "function")),
    profileTemplateKey,
  });
  return { context, storage, close: () => dom.window.close(), index: async () => (await storage.api().local.get(INDEX_KEY))[INDEX_KEY] ?? null };
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
// A glossary nested past serde_json's default limit (128) but within what jsdom's
// recursive serialiser can stand in for a browser's: the extension has a node
// limit and no depth limit (deeper cases: glossary.rs tests).
const nested = (depth) => {
  let content = ["before <&>", { tag: "p", content: "paragraph" }, { tag: "a", href: "https://example.com/", content: "link" }, " after"];
  for (let level = 0; level < depth; level += 1) content = { tag: level % 2 ? "span" : "div", style: { marginLeft: "1px" }, content };
  return { glossaries: [{ dictionary: "B", definitionTags: "", termTags: "", glossary: structured(content) }] };
};
const preflight = (expression, reading, fields = {}) => (side) => anki("hd_anki_preflight",
  { configKey: side.key, ...mined(expression, reading, side.generation, fields) });
// A submission and the media the linked browser sends with it (none: no envelope).
const submit = (expression, reading, fields = {}, clientMedia = {}) => (side) => ({
  ...anki("hd_anki_submit", { configKey: side.key, ...mined(expression, reading, side.generation, typeof fields === "function" ? fields(side) : fields) }),
  ...(clientMedia === null ? {} : { clientMedia }),
});
const deepPreflight = (side) => {
  const message = preflight("鱈", "たら")(side);
  message.request.term = { ...message.request.term, ...nested(600) };
  return message;
};
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
  Hint: template("{glossary-brief}"), MiscInfo: template("{tags} {part-of-speech} {dictionary} {dictionary-alias} {miscInfo}"),
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
    // Model scope: Anki's add is the duplicate check, and media goes with the note
    // (#683): a bare screenshot and pronunciation as attachments, one in rich
    // HTML stored first.
    configure([audio], { duplicateScope: "model" }),
    { message: preflight("鱸", "すずき") },
    { message: submit("鱸", "すずき", { screenshot: SHOT }, { screenshot: { ...SHOT, data: JPEG } }), mutates: true },
    { message: submit("鱸", "すずき"), mutates: true },
    { message: preflight("鱸", "すずき") },
    configure([audio], { duplicateScope: "model", fieldTemplates: templates({ Picture: template("<div>{screenshot}</div>") }) }),
    { message: submit("鰈", "かれい", { screenshot: SHOT }, { screenshot: { ...SHOT, data: JPEG } }), mutates: true },
    { message: submit("鮭", "さけ", { screenshot: SHOT }), mutates: true },
    configure([missing], { duplicateScope: "model" }),
    { message: submit("鯖", "さば"), mutates: true },
    // Faults (anki-fake.mjs): what Anki did is known only from what it holds.
    configure([audio]),
    ...faultSteps(fake),
    ...imageSteps(fake),
    ...templateSteps(fake),
    ...presetSteps(fake),
  ];
}

// The Senren and Lapis presets as upstream's applyAnkiPreset fills them, a
// setup proposal for Senren, the page's selection, title, address and search
// in the fields that name them, the sentence with furigana, and a kanji entry
// mined as a note.
let PRESETS;
const SENREN_FIELDS = ["word", "reading", "sentence", "sentenceFurigana", "sentenceTranslation", "sentenceCard", "audioCard", "notes",
  "hint", "picture", "wordAudio", "sentenceAudio", "selectionText", "definition", "glossary", "pitchAccents", "pitchPositions",
  "pitchCategories", "frequencies", "freqSort", "miscInfo", "dictionaryPreference"];
const PAGE = { popupSelectionText: "海に<鯱>\nがいる", documentTitle: "Fish & <whales>", pageUrl: "https://example.com/?a=1&b=<2>",
  searchQuery: "鯱 \"q\"" };
const KANJI_ENTRY = { dictionary: "KANJIDIC", onyomi: "コ", kunyomi: "しゃち しゃちほこ", tags: "jinmeiyou",
  definitions: ["killer whale", "grampus"], stats: [{ name: "strokes", value: "19" }, { name: "grade", value: "9" }] };
function presetSteps(fake) {
  const optionsWrite = (patch) => (side) => worker("hd_options_write", { baseRevision: side.revision, options: patch(side) });
  const sorted = (record) => Object.fromEntries(Object.entries(record).sort(([left], [right]) => (left < right ? -1 : 1)));
  const preset = (model, family, deck) => ({ message: optionsWrite(() => ({ anki: ankiOptions(fake.url, { model, deck,
    fieldTemplates: sorted(PRESETS.applyAnkiPreset({}, fake.state.models[model].fields, family).fieldTemplates) }) })),
  revision: true, key: true });
  const page = (expression, reading) => submit(expression, reading, PAGE);
  const kanji = (character, fields = {}) => (side) => {
    const message = submit(character, "", fields)(side);
    message.request = { ...message.request, matched: character, sentence: `${character}の字`, matchOffset: 0, trace: [],
      term: { expression: character, reading: "", rules: "", score: 0, frequencies: [], pitches: [],
        glossaries: [{ dictionary: KANJI_ENTRY.dictionary, definitionTags: "", termTags: "", kanji: KANJI_ENTRY,
          glossary: structured([{ tag: "ol", content: KANJI_ENTRY.definitions.map((content) => ({ tag: "li", content })) }]) }] } };
    return message;
  };
  return [
    // Senren holds the most notes: unconfigured, setup proposes it with its preset.
    { before: () => {
      fake.state.models.Senren = { id: 1004, fields: SENREN_FIELDS };
      const counts = Object.keys(fake.state.models).map((model) => fake.state.notes.filter((note) => note.modelName === model).length);
      for (let index = 0; index <= Math.max(...counts); index += 1) {
        fake.state.notes.push({ noteId: 5000 + index, modelName: "Senren", deckName: "Mining::Sub", mature: false,
          fields: Object.fromEntries(SENREN_FIELDS.map((field, position) => [field, position === 0 ? `語${index}` : ""])) });
      }
    }, message: optionsWrite(() => ({ anki: ankiOptions(fake.url, { model: "", fieldTemplates: null }) })), revision: true, key: true },
    { message: worker("hd_anki_setup") },
    preset("Senren", "senren", "Mining::Sub"),
    { message: preflight("鯱", "しゃち", PAGE) },
    { message: page("鯱", "しゃち"), mutates: true },
    { message: page("鯱", "しゃち"), mutates: true },
    { message: submit("鰯", "いわし"), mutates: true },
    preset("Lapis v1", "lapis", "Mining"),
    { message: page("鰹節", "かつおぶし"), mutates: true },
    // A kanji entry: the character, its readings, stroke count and tags.
    { message: optionsWrite(() => ({ anki: ankiOptions(fake.url, { fieldTemplates: sorted({
      Expression: template("{character}"), ExpressionReading: template("{onyomi}"), MainDefinition: template("{kunyomi}"),
      Hint: template("{stroke-count}"), Glossary: template("{glossary}"), MiscInfo: template("{tags}|{part-of-speech}|{conjugation}"),
      Sentence: template("{sentence}"), ExpressionFurigana: template("{furigana}") }) }) })), revision: true, key: true },
    { message: kanji("鰐"), mutates: true },
    { message: kanji("鰐"), mutates: true },
  ];
}

// Two Templates: a second one, on another note type, deck and duplicate
// scope, chosen by `templateId` in every request that takes one, with its own
// configuration key; the first one's key does not reach it.
function templateSteps(fake) {
  const optionsWrite = (patch) => (side) => worker("hd_options_write", { baseRevision: side.revision, options: patch(side) });
  // A written Template must be its normalised form: field templates in order.
  const sorted = (record) => Object.fromEntries(Object.entries(record).sort(([left], [right]) => (left < right ? -1 : 1)));
  const first = ankiOptions(fake.url, { fieldTemplates: sorted(markers) });
  const { url, apiKey, ...firstTemplate } = first;
  const basic = {
    id: "basic", name: "Basic cards", deck: "Default", model: "Basic", tags: ["basic"], duplicateScope: "deck",
    duplicateBehavior: "prevent", captureScreenshot: false, fields: firstTemplate.fields,
    fieldTemplates: { Back: template("{glossary-first}"), Front: template("{expression}") },
  };
  const templates = [{ id: "default", name: "Default", ...firstTemplate }, basic];
  const forTemplate = (id, build) => (side) => {
    const message = build({ ...side, key: side.keys?.[id] ?? "" });
    message.request = { ...message.request, templateId: id };
    return message;
  };
  return [
    // A Templates write carries the first one's fields too, as the extension's projection does.
    { message: optionsWrite(() => ({ anki: { url, apiKey, templates, ...firstTemplate } })), revision: true, key: true,
      templateKeys: ["basic", "default"] },
    { message: anki("hd_anki_status") },
    { message: anki("hd_anki_status", undefined, { templateId: "basic" }) },
    { message: anki("hd_anki_status", undefined, { templateId: "default" }) },
    { message: anki("hd_anki_view", { ...term("猫"), templateId: "basic" }) },
    { message: anki("hd_anki_screenshot", undefined, { templateId: "basic" }) },
    { message: worker("hd_anki_discover", { model: "Basic" }) },
    { message: worker("hd_anki_setup", { templateId: "basic" }) },
    { message: forTemplate("basic", preflight("猫", "ねこ")) },
    { message: forTemplate("basic", preflight("蛸", "たこ")) },
    { message: forTemplate("basic", submit("蛸", "たこ")), mutates: true },
    { message: forTemplate("basic", submit("蛸", "たこ")), mutates: true },
    { message: forTemplate("default", preflight("蛸", "たこ")) },
    { message: forTemplate("default", submit("蛸", "たこ")), mutates: true },
    // A Template's request with the other one's key, or a key without a Template.
    { message: (side) => {
      const message = preflight("蛸", "たこ")({ ...side, key: side.keys.default });
      message.request.templateId = "basic";
      return message;
    } },
    { message: forTemplate("nope", preflight("蛸", "たこ")) },
    { message: (side) => anki("hd_anki_browse", { configKey: side.keys.basic, expression: "蛸", templateId: "basic" }) },
    { message: (side) => anki("hd_anki_browse", { configKey: side.keys.basic, expression: "蛸" }) },
  ];
}

// Dictionary images (the fixture's media/kanji.png): refused by Anki, then
// stored with a later note and found already stored, missing from the
// dictionary, and with model scope (#683).
function imageSteps(fake) {
  const image = (path) => ({ glossaries: [
    { dictionary: "hachidori-fixture", definitionTags: "", termTags: "", glossary: structured([
      { tag: "img", path, width: 16, height: 16, title: "kanji glyph", alt: "kanji", collapsible: false }, "a glyph"]) },
    { dictionary: "A", definitionTags: "", termTags: "", glossary: JSON.stringify(["plain"]) }] });
  const pictured = (expression, reading, path = "media/kanji.png") => (side) => {
    const message = submit(expression, reading)(side);
    message.request.term = { ...message.request.term, ...image(path) };
    return message;
  };
  const checked = (expression, reading) => (side) => {
    const message = preflight(expression, reading)(side);
    message.request.term = { ...message.request.term, ...image("media/kanji.png") };
    return message;
  };
  const isImage = (params) => params?.filename?.endsWith(".png");
  const audio = source("audio", "custom", `${fake.url}/audio/{term}.mp3`);
  const optionsWrite = (patch) => (side) => worker("hd_options_write", { baseRevision: side.revision, options: patch(side) });
  const configure = (fields) => ({ message: optionsWrite(() => ({ audioSources: [audio], anki: ankiOptions(fake.url, {
    fieldTemplates: { ...markers, ExpressionAudio: template("{audio}") }, ...fields }) })), revision: true, key: true });
  return [
    configure({}),
    { message: checked("字", "じ") },
    { faults: () => [{ action: "storeMediaFile", kind: "error", message: "media folder is read-only", filter: isImage }],
      message: pictured("漢", "かん"), mutates: true },
    { message: pictured("字", "じ"), mutates: true },
    { message: pictured("文字", "もじ"), mutates: true },
    { message: pictured("漢方", "かんぽう"), mutates: true },
    { message: pictured("綴", "つづり", "media/none.png"), mutates: true },
    configure({ duplicateScope: "model" }),
    { message: checked("書", "しょ") },
    { message: pictured("書", "しょ"), mutates: true },
    { message: pictured("画", "が"), mutates: true },
  ];
}

// AnkiConnect failing part-way: lost acknowledgements, undispatched writes,
// refusals, a failed readback, add-on rewrites, lost media, and the collection
// changing under an overwrite.
function faultSteps(fake) {
  // Its own file: a failed write removes the screenshot it stored.
  const own = { token: "shot-2", filename: "hachidori-screenshot-1b4e28ba-2fa1-41d2-883f-0016d3cca427.jpg" };
  const shot = (expression, reading) => submit(expression, reading, { screenshot: own }, { screenshot: { ...own, data: JPEG } });
  const lastNote = (state) => state.notes.at(-1);
  const noteOf = (state, params) => state.notes.find((note) => note.noteId === params.note.id);
  const audio = source("audio", "custom", `${fake.url}/audio/{term}.mp3`);
  const optionsWrite = (patch) => (side) => worker("hd_options_write", { baseRevision: side.revision, options: patch(side) });
  return [
    { faults: () => [{ action: "addNote", kind: "drop-after" }], message: submit("鰆", "さわら"), mutates: true },
    { faults: () => [{ action: "addNote", kind: "drop-before" }], message: submit("鰤", "ぶり"), mutates: true },
    { faults: () => [{ action: "addNote", kind: "error", message: "collection is not available" }], message: submit("鯛", "たい"), mutates: true },
    { faults: () => [{ action: "notesInfo", after: "addNote", kind: "error", message: "readback failed" }], message: submit("鮎", "あゆ"), mutates: true },
    { faults: () => [{ action: "addNote", kind: "edit", when: "after", edit: (state) => {
      const note = lastNote(state);
      note.fields.Glossary = note.fields.Glossary.replace(/<[^>]+>/gu, "");
    } }], message: submit("鰊", "にしん"), mutates: true },
    { faults: () => [{ action: "storeMediaFile", kind: "lost-store" }], message: shot("鰰", "はたはた"), mutates: true },
    { faults: () => [{ action: "storeMediaFile", kind: "drop-after" }], message: shot("鱧", "はも"), mutates: true },
    { faults: () => [{ action: "storeMediaFile", kind: "error", message: "media folder is read-only" }], message: shot("鮃", "ひらめ"), mutates: true },
    { message: optionsWrite(() => ({ audioSources: [audio], anki: ankiOptions(fake.url, { duplicateBehavior: "overwrite",
      fieldTemplates: { ...markers, ExpressionAudio: template("{audio}") } }) })), revision: true, key: true },
    { faults: () => [{ action: "updateNoteFields", kind: "edit", edit: (state, params) => {
      noteOf(state, params).fields.Sentence = "edited in Anki";
    } }], message: submit("犬", "いぬ"), mutates: true },
    { faults: () => [{ action: "updateNoteFields", kind: "edit", edit: (state, params) => {
      state.notes = state.notes.filter((note) => note.noteId !== params.note.id);
    } }], message: submit("鳥", "とり"), mutates: true },
    { faults: () => [{ action: "updateNoteFields", kind: "drop-after" }], message: submit("鯨", "くじら"), mutates: true },
    { faults: () => [{ action: "updateNoteFields", kind: "drop-before" }], message: submit("鯉", "こい"), mutates: true },
    // #698: an open Anki editor refuses the update before writing; once it is closed the update goes through.
    { faults: () => [{ action: "updateNoteFields", kind: "error",
      message: "Close the affected editor before this API write, even if its content is saved." }], message: submit("犬", "いぬ"), mutates: true },
    { message: submit("犬", "いぬ"), mutates: true },
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
    // A request naming a profile: on records without profiles only Japanese is one.
    { message: anki("hd_anki_status", undefined, { profileId: "ja" }) },
    { message: anki("hd_anki_status", undefined, { profileId: "en" }) },
    { message: anki("hd_anki_status", undefined, { profileId: "" }) },
    { message: anki("hd_anki_view", term("猫"), { profileId: "en" }) },
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
    { message: deepPreflight },
    { message: optionsWrite(() => ({ smallerAnkiCards: true })), revision: true, key: true },
    { message: preflight("鯨", "くじら") },
    { message: deepPreflight },
    { message: optionsWrite(() => ({ smallerAnkiCards: false })), revision: true, key: true },
    // Compact popup and Anki (#673), alone and with Smaller Anki cards.
    { message: optionsWrite(() => ({ glossaryLayoutMode: "compact-popup-anki" })), revision: true, key: true },
    { message: preflight("鯨", "くじら") },
    { message: optionsWrite(() => ({ smallerAnkiCards: true })), revision: true, key: true },
    { message: preflight("鯨", "くじら") },
    { message: optionsWrite(() => ({ smallerAnkiCards: false, glossaryLayoutMode: "default" })), revision: true, key: true },
    // Model scope over every marker: preflight renders only what Anki's check needs.
    { message: optionsWrite(() => ({ anki: ankiOptions(fake.url, { duplicateScope: "model", fieldTemplates: markers }) })),
      revision: true, key: true, wordStatus: true },
    { message: preflight("鯨", "くじら") },
    { message: preflight("猫", "ねこ") },
    { message: anki("hd_anki_status") },
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
    // No deck chosen yet: a status that names no profile carries no emptyDeck hint.
    { message: optionsWrite(() => ({ anki: ankiOptions(fake.url, { deck: "" }) })), revision: true, key: true },
    { message: anki("hd_anki_status") },
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
  PRESETS = await import(pathToFileURL(join(HACHIDORI, "extension/anki-templates.js")));
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
  // The fixture dictionary, with its image, installed in the relay; the extension
  // lists the same dictionaries (an image's filename is named by its path) and
  // reads their media from the relay's engine.
  const media = await connectClient(t, relay.port, "/link");
  media.send(JSON.stringify({ kind: "hello", protocol: 1, version: "0.9.0", name: "media", capabilities: ["linked-anki-v2", "linked-import-v1"] }));
  await untilKind(media, "hello");
  const ask = async (message) => {
    media.send(JSON.stringify({ kind: "request", id: "media", message }));
    return (await untilKind(media, "reply")).response;
  };
  // `uploadDictionary`: the archive in the chunks the relay asks for.
  const archive = readFileSync(join(HACHIDORI, "test/fixtures/hachidori-fixture.zip"));
  const target = "hachidori-linked-import";
  const begun = await ask({ target, type: "hd_import_begin", requestId: "begin", fileName: "hachidori-fixture.zip", size: archive.length, replace: false });
  assert.equal(typeof begun.token, "string", JSON.stringify(begun));
  for (let offset = 0; offset < archive.length; offset += begun.chunkBytes) {
    const chunk = await ask({ target, type: "hd_import_chunk", requestId: "chunk", token: begun.token, offset,
      data: archive.subarray(offset, offset + begun.chunkBytes).toString("base64") });
    assert.notEqual(chunk.ok, false, JSON.stringify(chunk));
  }
  const imported = await ask({ target, type: "hd_import_commit", requestId: "commit", token: begun.token });
  assert.equal(imported.ok, true, JSON.stringify(imported));
  socket.send(JSON.stringify({ kind: "request", id: "state", message: worker("hd_state_read") }));
  const { state } = (await untilKind(socket, "reply")).response;
  assert.equal(state.dictionaries.length, 1);
  // Committing the dictionary normalised the stored options, as the
  // extension's state commit does (legacy Anki fields become Templates): both
  // sides start from the relay's records.
  const importedOptions = JSON.parse(readFileSync(join(dir, "storage.json"), "utf8")).options;
  await extension.storage.api().local.set({ dictionaryState: state, options: importedOptions });
  let mediaAsked = Promise.resolve();
  engineState.ask = (message) => {
    const asked = mediaAsked.then(async () => {
      const response = await ask(message);
      if (!response.ok) throw new Error(response.error);
      return response;
    });
    mediaAsked = asked.catch(() => {});
    return asked;
  };
  engineState.styles = async () => {
    const response = await engineState.ask({ target: "hoshidicts-offscreen", type: "hd_styles", requestId: "styles" });
    return { generation: response.generation, styles: response.styles };
  };
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
    extension: { name: "extension", key: "", revision: importedOptions.revision, generation: engineState.generation, index: extension.index },
    relay: { name: "relay", key: "", revision: importedOptions.revision, generation: engineState.generation, index: relayIndex },
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
      // Each side meets the same faults.
      fake.state.faults = step.faults ? step.faults() : [];
      fake.state.ran = [];
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
      assert.deepEqual(fake.state.faults, [], `every fault fired for ${name}`);
      if (step.key) side.key = "";
      if (step.revision && replies[name].ok) side.revision = replies[name].options.revision;
      queries[name] = browsed(from);
      notes[name] = checked(from);
      side.stored = await settled(side.index);
      if (step.key && !side.key) {
        // A Template change makes a new configuration key; read it as a linked page
        // would, for the first Template and each other one the step names.
        const status = (extra) => (name === "extension"
          ? extension.context.dispatchSharedRequest(anki("hd_anki_status", undefined, extra), "linked", ["linked-anki-v2"])
          : (async () => {
            counter += 1;
            socket.send(JSON.stringify({ kind: "request", id: counter, message: anki("hd_anki_status", undefined, extra) }));
            return (await untilReply()).frame.response;
          })());
        side.key = (await status()).configKey ?? "";
        side.keys = {};
        for (const id of step.templateKeys ?? []) side.keys[id] = (await status({ templateId: id })).configKey ?? "";
      }
    }
    const label = JSON.stringify(typeof step.message === "function" ? step.message(sides.relay) : step.message).slice(0, 160);
    assert.deepEqual(comparable(replies.relay), comparable(replies.extension), label);
    seen.push(replies.extension);
    assert.deepEqual(queries.relay, queries.extension, `Anki browser queries for ${label}`);
    assert.deepEqual(notes.relay, browserCss(notes.extension), `the notes checked for ${label}`);
    if (written) assert.deepEqual(collection(), browserCss(written), `the collection after ${label}`);
    checks.push(...notes.extension);
    assert.deepEqual(comparable(sides.relay.stored), comparable(sides.extension.stored), `the stored index after ${label}`);
  }
  // The sequence reaches what matters: cached duplicates, repairs, Anki's browser,
  // word status, a setup proposal and refusals, and the relay announced index changes.
  if (process.env.ANKI_ORACLE_DUMP) console.log(JSON.stringify(seen, null, 1));
  assert.ok(seen.filter((reply) => reply.type === "hd_options_write_result").every((reply) => reply.ok), JSON.stringify(seen.filter((reply) => reply.type === "hd_options_write_result" && !reply.ok)));
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
  // The fault paths: unconfirmed writes, a failed readback, an add-on rewrite and lost media.
  assert.ok(submitted.filter((reply) => reply.state === "uncertain").length >= 5);
  assert.ok(submitted.some((reply) => reply.warnings?.some((warning) => warning.includes("readback failed"))));
  assert.ok(submitted.some((reply) => reply.warnings?.some((warning) => warning.includes("saved with different content"))));
  assert.ok(submitted.some((reply) => reply.warnings?.some((warning) => warning.startsWith("Screenshot: AnkiConnect: media folder"))));
  // The second Template mines into its own note type, deck and tags.
  assert.ok(fake.state.notes.some((note) => note.modelName === "Basic" && note.deckName === "Default"
    && note.tags?.includes("basic") && note.fields.Front === "蛸"));
  // The presets: a Senren proposal, and notes with the page's details, the
  // sentence with furigana and a kanji entry's readings.
  assert.ok(of("hd_anki_setup").some((reply) => reply.proposal?.status === "configured" && reply.proposal.model === "Senren"
    && reply.proposal.deck === "Mining::Sub" && reply.proposal.fieldTemplates?.selectionText?.value === "{popup-selection-text}"));
  const senren = fake.state.notes.find((note) => note.modelName === "Senren" && note.fields.word === "鯱");
  assert.ok(senren, "a Senren note");
  assert.equal(senren.fields.selectionText, "海に&lt;鯱&gt;<br>\nがいる");
  assert.equal(senren.fields.miscInfo, "Fish &amp; &lt;whales&gt;");
  assert.ok(/<b>.*しゃち.*<\/b>/u.test(senren.fields.sentenceFurigana), senren.fields.sentenceFurigana);
  assert.ok(fake.state.notes.some((note) => note.modelName === "Lapis v1" && note.fields.Expression === "鰹節"
    && note.fields.SelectionText.includes("<br>")));
  assert.ok(fake.state.notes.some((note) => note.fields.Expression === "鰐" && note.fields.ExpressionReading === "コ"
    && note.fields.MainDefinition === "しゃち, しゃちほこ" && note.fields.Hint === "19"), "a kanji entry's note");
  // Dictionary images: stored once under their path's name, a refused store and a missing image fail the write.
  assert.equal(Object.keys(fake.state.media).filter((name) => name.endsWith(".png")).length, 1);
  assert.ok(submitted.some((reply) => reply.ok === false && reply.error === "AnkiConnect: media folder is read-only"));
  assert.ok(submitted.some((reply) => reply.ok === false && reply.error === "The dictionary image is no longer available or is malformed."));
  assert.ok(Object.keys(fake.state.media).some((name) => name.startsWith("hachidori-screenshot-")), "a screenshot was stored");
  assert.ok(Object.keys(fake.state.media).some((name) => /^hachidori_[0-9a-f]{64}\.mp3$/u.test(name)), "a pronunciation was stored");
  const rendered = checks.flatMap((params) => params.notes).map((note) => note.fields);
  assert.ok(rendered.some((fields) => fields.Glossary?.includes("<ul") && fields.DefinitionPicture?.includes("<svg")), "rich fields were rendered");
  assert.ok(rendered.some((fields) => fields.Glossary?.includes("<ruby") && !fields.Glossary.includes("gsm-hoshidicts-glossary-content")),
    "a compact glossary was rendered");
});
