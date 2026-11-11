// Host mode against the extension itself: the real Hachidori engine service (its
// wasm Hoshidicts, its background worker, the smoke suite's fakes for the browser)
// imports the extension's fixture dictionaries and is the browser host the relay
// mirrors. Every lookup request is then sent both to that engine and, through a
// linked browser, to the relay's native engine, and the replies must agree.
//
// Not from upstream. Needs a binary built with `--features host` and a Hachidori
// checkout with its wasm built and fixtures generated (`node test/make-fixture.mjs`)
// at $HACHIDORI_SOURCE or ../hachidori; it is skipped otherwise.
// SPDX-License-Identifier: GPL-3.0-or-later
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import test from "node:test";
import { startAnkiRelayServer } from "./anki-relay-server.mjs";
import { FIXTURES, HACHIDORI, bootExtension, jsdomEntry, realFetch, serveAsBrowserHost, skip } from "./extension-host.mjs";
import { connectClient, untilKind } from "./relay-socket.mjs";

// What may differ between the two, by design (docs/host-mode.md): the generation
// counts each side's own loads, and the status describes each side's own engine.
const LOCAL = new Set(["generation", "requestId", "storageBackend", "threaded", "lowMemory", "lastLoadPath",
  "hashIndexStorage", "residentHashBudgetBytes"]);
const comparable = (reply) => Object.fromEntries(Object.entries(reply).filter(([key]) => !LOCAL.has(key)));

const REQUESTS = [
  ...["食べる", "食べました", "漢字の読み方", "ありがとう", "読まなかった", "食", "ない", "", "x".repeat(5000), "a\u0000b"]
    .map((text) => ({ type: "hd_lookup", text })),
  { type: "hd_lookup", text: "食べる", maxResults: 1 },
  { type: "hd_lookup", text: "食べる", maxResults: "0", scanLength: null },
  { type: "hd_lookup", text: "食べる", options: { frequencyDictionary: "hachidori-fixture", frequencyOrder: "descending", primaryReading: "たべる" } },
  { type: "hd_lookup", text: "食べる", options: { personalDictionary: false, metadataOnlyTerms: true } },
  { type: "hd_lookup", text: "食べる", requestId: {} },
  { type: "hd_lookup_dictionary", text: "食べる", dictionary: "hachidori-fixture" },
  { type: "hd_lookup_dictionary", text: "食べる", dictionary: "no such dictionary" },
  { type: "hd_segment", chunks: [{ id: 1, text: "漢字を食べました。ありがとう" }, { id: "b", text: "" }, { text: "読む" }] },
  { type: "hd_segment", chunks: "nope" },
  { type: "hd_segment", chunks: [{ id: 1, text: "読む" }], options: { personalDictionary: false }, scanLength: 2 },
  ...["食", "", "x", "食べる"].map((character) => ({ type: "hd_kanji", character })),
  { type: "hd_styles" },
  { type: "hd_status" },
];

test("the relay's engine answers as the extension's own engine does", { skip, timeout: 120_000 }, async (t) => {
  const extension = await bootExtension();
  const dir = mkdtempSync(join(tmpdir(), "hachidori-relay-oracle-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const relay = await startAnkiRelayServer({ apiPort: 0, extraArgs: ["--host", "--data-dir", dir] });
  t.after(() => relay.close());
  const linked = await connectClient(t, relay.port, "/link");
  linked.send(JSON.stringify({ kind: "hello", protocol: 1, version: "0.9.0", name: "Firefox", capabilities: [] }));
  const { snapshot } = await untilKind(linked, "hello");
  await serveAsBrowserHost(t, relay.port, extension);
  // Linked browsers see the dictionary list once every dictionary in it is installed here.
  // `snapshot` follows the changes, as the linked browser's mirror does.
  let state = null;
  while (state === null) {
    const { changes } = await untilKind(linked, "storage");
    Object.assign(snapshot, changes);
    state = changes.dictionaryState ?? null;
  }
  assert.equal(state.dictionaries.length, FIXTURES.length);

  let counter = 0;
  const viaRelay = async (message) => {
    counter += 1;
    linked.send(JSON.stringify({ kind: "request", id: `r${counter}`, message: { target: "hoshidicts-offscreen", ...message } }));
    const reply = await untilKind(linked, "reply");
    assert.equal(reply.id, `r${counter}`);
    return reply.response;
  };
  const viaExtension = (message) => extension.engine({ target: "hoshidicts-offscreen", requestId: "q", relayed: true, ...message });

  const answers = [];
  for (const message of REQUESTS) {
    const request = { requestId: "q", ...message };
    const [expected, actual] = [await viaExtension(request), await viaRelay(request)];
    assert.deepEqual(comparable(actual), comparable(expected), JSON.stringify(message).slice(0, 200));
    assert.equal(actual.requestId, expected.requestId);
    answers.push(actual);
  }
  // The agreement covers real answers: definitions with their tags, and furigana.
  const results = answers.flatMap((reply) => reply.results ?? []);
  assert.ok(results.some((result) => result.term.glossaries.some((glossary) => glossary.tags.length > 0)));
  assert.ok(results.some((result) => Array.isArray(result.term.furigana)));
  assert.ok(answers.some((reply) => reply.kanji?.entries?.length > 0));
  assert.ok(answers.some((reply) => reply.segments?.[0]?.spans?.length > 0));

  // A request the sharing host does not forward is refused there, as dispatchSharedRequest does.
  const bogus = await viaRelay({ type: "hd_bogus", requestId: "b" });
  assert.deepEqual(bogus, { type: "hd_bogus_result", requestId: "b", ok: false,
    error: 'unsupported shared request "hoshidicts-offscreen" "hd_bogus"', generation: 0 });

  // A lookup naming a profile is resolved by the host's worker before the engine
  // (resolveLookupProfile): on records without profiles Japanese is the request
  // without it, and any other is refused.
  const japanese = { type: "hd_lookup", text: "食べる", requestId: "p" };
  assert.deepEqual(comparable(await viaRelay({ ...japanese, profileId: "ja" })), comparable(await viaExtension(japanese)));
  for (const profileId of ["en", "", 7]) {
    assert.deepEqual(await viaRelay({ ...japanese, profileId }), { type: "hd_lookup_result", requestId: "p", ok: false,
      error: "This language was removed in another Hachidori page.", generation: 0, errorCode: "profile-missing" });
  }

  // The language opt-in is answered whatever the records (answerLanguageOptIn),
  // from the records the hello carried.
  const languages = await import(pathToFileURL(join(HACHIDORI, "extension/sharing-languages.js")));
  const optIn = async (message) => {
    counter += 1;
    const request = { target: "hoshidicts-worker", type: "hd_language_profiles_opt_in", requestId: "o", ...message };
    linked.send(JSON.stringify({ kind: "request", id: `r${counter}`, message: request }));
    return (await untilKind(linked, "reply")).response;
  };
  for (const codes of [[], ["en", "fr"]]) {
    const missing = languages.missingLanguages(languages.snapshotLanguages(snapshot), languages.optInLanguages({ languages: codes }));
    assert.deepEqual(missing, []);
    assert.deepEqual(await optIn({ languages: codes }), { type: "hd_language_profiles_opt_in_result", requestId: "o", ok: true, error: null,
      // packCodes: Japanese, then every pack the extension ships, as the relay ships them.
      optIn: "accepted", snapshot, missingLanguages: missing, hostLanguages: languages.packCodes() });
    assert.ok(languages.packCodes().length > 20, JSON.stringify(languages.packCodes()));
  }
  for (const codes of [undefined, ["EN"]]) {
    assert.equal(languages.optInLanguages({ languages: codes }), null);
    assert.deepEqual(await optIn({ languages: codes }), { type: "hd_language_profiles_opt_in_result", requestId: "o", ok: false,
      error: "The language opt-in named no language packs.", generation: 0 });
  }

  // Media is asked for with the generation of the reply that named it.
  const media = async (via, path) => {
    const { generation } = await via({ type: "hd_lookup", text: "漢字" });
    return via({ type: "hd_media", dictionary: "hachidori-fixture", path, generation });
  };
  for (const path of ["media/kanji.png", "media%2Fkanji.png", "media/none.png", ""]) {
    assert.deepEqual(comparable(await media(viaRelay, path)), comparable(await media(viaExtension, path)), path);
  }
  const stale = { type: "hd_media", dictionary: "hachidori-fixture", path: "media/kanji.png", generation: 999 };
  assert.deepEqual(comparable(await viaRelay(stale)), comparable(await viaExtension(stale)));

  // Memory describes each engine's own heap; only its shape is shared.
  const memory = await viaRelay({ type: "hd_memory" });
  assert.equal(memory.ok, true);
  assert.deepEqual(memory.dictionaries.map((entry) => entry.title), state.dictionaries.map((entry) => entry.title));
  assert.ok(Number.isFinite(memory.heapBytes));
  const total = await viaRelay({ type: "hd_memory_total" });
  assert.deepEqual(Object.keys(total).sort(), ["bytes", "heapBytes", "ok", "requestId", "type"]);

  // The Yomitan API's host side (api-host.js), answered from the same dictionaries.
  // In rendered fields, each side's own generation differs, and so does a dictionary
  // style's scope: a browser's CSSOM writes its attribute value as a string, as the
  // relay does, while jsdom keeps the extension's CSS.escape spelling.
  const rendered = (reply) => JSON.parse(JSON.stringify(reply)
    .replace(/data-hoshidicts-generation=\\"\d+\\"/gu, "data-hoshidicts-generation")
    .replace(/\[data-dictionary=\\"([^"\\]*)\\"\]/gu, "[data-dictionary=$1]"));
  for (const message of [
    { type: "hd_api_term_entries", terms: ["食べる", "食べました", "漢字の", "ありがとう", "", "読まない", "x"] },
    { type: "hd_api_term_entries", terms: "食べる" },
    { type: "hd_api_kanji_entries", characters: ["食", "食べ", "x", ""] },
    { type: "hd_api_tokenize", texts: ["漢字を食べました\nありがとう", "", "読まなかった本"] },
    { type: "hd_api_tokenize", texts: ["食べる"], scanLength: 1 },
    { type: "hd_api_dictionaries" },
    { type: "hd_api_dictionary_open", id: "no such id" },
    { type: "hd_api_anki_card_formats" },
    { type: "hd_api_anki_card_formats", profileIndex: 0 },
    { type: "hd_api_anki_card_formats", profileIndex: 1 },
    { type: "hd_api_anki_card_formats", profileIndex: null },
    { type: "hd_api_anki_fields", text: "漢", entryType: "kanji", markers: ["Character", "onyomi", "kunyomi", "glossary", "tags",
      "stroke-count", "frequencies", "dictionary", "dictionary-alias", "nope"], maxEntries: 0 },
    { type: "hd_api_anki_fields", text: "食べる", entryType: "kanji", markers: ["character"], maxEntries: 1 },
    { type: "hd_api_anki_fields", text: "x", entryType: "other", markers: [] },
    { type: "hd_api_anki_fields", text: 1, entryType: "term", markers: [] },
    { type: "hd_api_anki_fields", text: "x", entryType: "term", markers: "expression" },
    // The term fields render as mining does, which needs jsdom on the extension's side.
    ...(jsdomEntry ? [
      { type: "hd_api_anki_fields", text: "漢字を食べました", entryType: "term", markers: ["expression", "reading", "furigana",
        "glossary", "glossary-first", "frequencies", "pitch-accents", "tags", "audio", "sentence", "cloze-body", "dictionary"],
        maxEntries: 2, includeMedia: true },
      { type: "hd_api_anki_fields", text: "漢字", entryType: "term", markers: ["glossary", "Expression", "expression"], includeMedia: true },
      { type: "hd_api_anki_fields", text: "漢字", entryType: "term", markers: ["glossary"], includeMedia: false },
      { type: "hd_api_anki_fields", text: "ありがとう", entryType: "term", markers: ["expression", "audio"], maxEntries: 1.5 },
      // A marker keeps the spelling the request gave it (#711), audio included.
      { type: "hd_api_anki_fields", text: "ありがとう", entryType: "term", markers: ["Expression", "Audio", "miscInfo"], maxEntries: 1,
        includeMedia: true },
    ] : []),
  ]) {
    const request = { requestId: "a", ...message };
    assert.deepEqual(rendered(await viaRelay(request)), rendered(await extension.api({ target: "hoshidicts-offscreen", ...request })),
      JSON.stringify(message));
  }
  if (jsdomEntry) {
    const { dictionaryMedia } = await viaRelay({ requestId: "a", type: "hd_api_anki_fields", text: "漢字", entryType: "term",
      markers: ["glossary"], includeMedia: true });
    assert.ok(dictionaryMedia.length > 0, "a term's dictionary image was sent");
  } else t.diagnostic("jsdom is unavailable: the API's term fields were not compared");

  // A dictionary download from the relay's HTTP API is the archive the extension builds.
  const archiveOf = async (bytes) => {
    const opened = await extension.openBackupArchive(new Blob([bytes]));
    const files = await Promise.all(opened.files.map(async (file) => [file.path, Buffer.from(await file.data.arrayBuffer()).toString("base64")]));
    return { snapshot: opened.snapshot, files };
  };
  for (const { id } of state.dictionaries) {
    const response = await realFetch(`http://127.0.0.1:${relay.apiPort}/dictionaries/${encodeURIComponent(id)}`);
    assert.equal(response.status, 200);
    const served = await archiveOf(await response.arrayBuffer());
    await extension.assertBackupSnapshot(served.snapshot);
    const { token, size } = await extension.engine({ type: "hd_api_dictionary_open", id });
    const read = await extension.engine({ type: "hd_api_dictionary_read", token, offset: 0, length: size });
    await extension.engine({ type: "hd_api_dictionary_close", token });
    assert.deepEqual(served, await archiveOf(Buffer.from(read.data, "base64")), id);
  }

  // And the HTTP API answers from the relay with no browser host connected.
  const terms = await realFetch(`http://127.0.0.1:${relay.apiPort}/termEntries`, { method: "POST", body: JSON.stringify({ term: "食べる" }) });
  assert.equal(terms.status, 200);
  assert.equal((await terms.json()).dictionaryEntries?.[0]?.headwords?.[0]?.term, "食べる");
});
