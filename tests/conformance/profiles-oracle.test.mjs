// Host mode's language profiles against the extension itself: the same
// hd_profile_add, hd_profile_remove, per-language personal dictionary saves,
// profile views of hd_apply_state, removals and lookups by profile go to the
// real Hachidori (a page's request, through its background, which names the
// profile's fields) and, through a linked browser, to the relay, which mirrored
// that extension's dictionaries first. Every reply and the stored records after
// every step must agree.
//
// Not from upstream. Needs what host-oracle.test.mjs needs; skipped otherwise.
// SPDX-License-Identifier: GPL-3.0-or-later
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { startAnkiRelayServer } from "./anki-relay-server.mjs";
import { FIXTURES, bootExtension, comparer, jsdomEntry, serveAsBrowserHost, skip } from "./extension-host.mjs";
import { awareClient, linkClient } from "./linked-client.mjs";
import { connectClient, untilKind } from "./relay-socket.mjs";

const STORED = ["dictionaryState", "customDictionarySource", "options", "lookupStats", "wordStatusOverrides"];

test("the relay keeps language profiles as the extension does", { skip, timeout: 240_000 }, async (t) => {
  const extension = await bootExtension();
  const dir = mkdtempSync(join(tmpdir(), "hachidori-relay-profiles-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const relay = await startAnkiRelayServer({ apiPort: 0, extraArgs: ["--host", "--data-dir", dir] });
  t.after(() => relay.close());
  const linked = await connectClient(t, relay.port, "/link");
  linked.send(JSON.stringify({ kind: "hello", protocol: 1, version: "0.9.0", name: "Firefox", capabilities: [] }));
  await untilKind(linked, "hello");
  await serveAsBrowserHost(t, relay.port, extension);
  let mirrored = null;
  while (mirrored === null) mirrored = (await untilKind(linked, "storage")).changes.dictionaryState ?? null;
  assert.equal(mirrored.dictionaries.length, FIXTURES.length);

  // A profile-aware browser with every pack: once the records gain profiles the
  // host closes its link (docs/languages.md 17.3 item 6), and it links again and
  // opts in. A request the closing host never ran is sent again.
  const packs = globalThis.HDLanguages.codes.filter((code) => code !== "ja");
  const relayLink = (context, options) => linkClient(context, relay.port, options);
  let link = await awareClient(t, relayLink, packs);
  linked.close();
  const relaySide = {
    name: "relay",
    comparable: comparer(),
    async send(message) {
      for (;;) {
        if (link.closed) link = await awareClient(t, relayLink, packs);
        let reply;
        try {
          reply = await link.send({ target: "hoshidicts-offscreen", ...message });
        } catch (error) {
          if (link.closed) continue;
          throw error;
        }
        if (reply?.error === "The linked Hachidori is changing its languages. Try again.") {
          await link.whenClosed();
          continue;
        }
        return reply;
      }
    },
    async stored() {
      const storage = JSON.parse(readFileSync(join(dir, "storage.json"), "utf8"));
      return Object.fromEntries(STORED.map((key) => [key, storage[key] ?? null]));
    },
  };
  const extensionSide = {
    name: "extension",
    comparable: comparer(),
    send: (message) => extension.page({ target: "hoshidicts-offscreen", ...message }),
    async stored() {
      const storage = await extension.storage.api().local.get(STORED);
      return Object.fromEntries(STORED.map((key) => [key, storage[key] ?? null]));
    },
  };
  const sides = [extensionSide, relaySide];

  // One step: a message per side, built from that side's own records when it
  // names them, whose replies and resulting records must agree.
  const step = async (label, build) => {
    const results = [];
    for (const side of sides) {
      const before = await side.stored();
      const message = { requestId: label, ...(typeof build === "function" ? build(before) : build) };
      const reply = await side.send(message);
      results.push({ reply: side.comparable(reply), stored: side.comparable(await side.stored()) });
    }
    assert.deepEqual(results[1].reply, results[0].reply, `${label}: reply`);
    assert.deepEqual(results[1].stored, results[0].stored, `${label}: storage`);
    return results[0].reply;
  };
  const found = async (label, text, profileId) => {
    const reply = await step(`${label}: ${text} (${profileId ?? "none"})`,
      { type: "hd_lookup", text, ...(profileId === undefined ? {} : { profileId }) });
    assert.equal(reply.ok, true, `${label}: ${text}: ${reply.error}`);
    return reply.results.length;
  };
  const state = (stored) => stored.dictionaryState;
  const englishRevision = (stored) => stored.customDictionarySource?.languages?.en?.revision ?? 0;
  // A profile's schema-1 view, as Settings shows it: its rows, in its order.
  const view = (stored, profileId) => {
    const records = state(stored);
    const rows = new Map(records.dictionaries.map((row) => [row.id, row]));
    return records.profiles[profileId].dictionaries.map((entry) => ({ ...rows.get(entry.id), enabled: entry.enabled }));
  };

  assert.ok(await found("japanese only", "食べる") > 0);
  let reply = await step("profile for a single-profile record", { type: "hd_lookup", text: "食べる", profileId: "en" });
  assert.equal(reply.errorCode, "profile-missing");

  // Adding a language: the records gain profiles, every package stays Japanese's.
  reply = await step("add English", { type: "hd_profile_add", language: "en" });
  assert.equal(reply.ok, true, reply.error);
  assert.equal(reply.state.schemaVersion, 2);
  assert.deepEqual(reply.languages, ["ja", "en"]);
  reply = await step("add English again", { type: "hd_profile_add", language: "en" });
  assert.equal(reply.ok, false);
  // A linked browser's malformed language meets the sharing host's pack check
  // first (sharing-oracle.test.mjs); a page's reaches the engine.
  for (const message of [{ language: "de", copyFrom: 3 }]) {
    reply = await step(`add ${JSON.stringify(message)}`, { type: "hd_profile_add", ...message });
    assert.equal(reply.ok, false);
  }
  assert.ok(await found("two languages", "食べる") > 0);
  assert.ok(await found("two languages", "食べる", "ja") > 0);
  assert.equal(await found("two languages", "cat", "en"), 0);
  reply = await step("a removed profile", { type: "hd_lookup", text: "cat", profileId: "fr" });
  assert.equal(reply.errorCode, "profile-missing");

  // English's personal dictionary: created, appended, found only in English.
  reply = await step("English personal created", (stored) => ({ type: "hd_custom_save", profileId: "en",
    baseDocumentRevision: englishRevision(stored), text: "cat, , a feline\nrun, , to move fast\n" }));
  assert.equal(reply.ok, true, reply.error);
  assert.equal(reply.rebuilt, true);
  assert.equal(reply.state.schemaVersion, 2);
  reply = await step("English personal stale", { type: "hd_custom_save", profileId: "en", baseDocumentRevision: 0, text: "x, , y" });
  assert.equal(reply.stale, true);
  reply = await step("English personal append", { type: "hd_custom_append", profileId: "en",
    entry: { term: "bird", reading: "", definition: "it flies" } });
  assert.equal(reply.ok, true, reply.error);
  assert.ok(await found("English personal", "cat", "en") > 0);
  assert.ok(await found("English personal", "bird", "en") > 0);
  assert.equal(await found("English personal", "bird", "ja"), 0);
  reply = await step("Japanese personal", (stored) => ({ type: "hd_custom_save", profileId: "ja",
    baseDocumentRevision: stored.customDictionarySource?.revision ?? 0, text: "猫, ねこ, cat\n" }));
  assert.equal(reply.ok, true, reply.error);
  assert.ok(await found("Japanese personal", "猫", "ja") > 0);
  assert.equal(await found("Japanese personal", "猫", "en"), 0);

  // Profile views: English takes a Japanese package and turns it off again (its
  // personal package stays on), Japanese reorders through an older client's view.
  reply = await step("English personal off", (stored) => ({ type: "hd_apply_state", profileId: "en",
    baseRevision: state(stored).revision, dictionaries: view(stored, "en").map((row) => ({ ...row, enabled: false })), groups: [] }));
  assert.equal(reply.ok, false);
  reply = await step("English takes a fixture", (stored) => ({ type: "hd_apply_state", profileId: "en",
    baseRevision: state(stored).revision, groups: [],
    dictionaries: [...view(stored, "en"), { ...view(stored, "ja").at(-1), enabled: true }] }));
  assert.equal(reply.ok, true, reply.error);
  assert.ok(await found("English fixture", "食べる", "en") > 0);

  // The Yomitan API answers from the profile its index names (api-host.js with
  // background-sharing.js's apiProfile): 0 is Japanese, 1 English, and an index
  // past the last answers from 0, except for card formats. Rendered fields differ
  // only in each side's generation and its style scope spelling (host-oracle.test.mjs).
  const rendered = (reply) => JSON.parse(JSON.stringify(reply)
    .replace(/data-hoshidicts-generation=\\"\d+\\"/gu, "data-hoshidicts-generation")
    .replace(/\[data-dictionary=\\"([^"\\]*)\\"\]/gu, "[data-dictionary=$1]"));
  const api = async (message) => {
    const request = { requestId: "api", ...message };
    const expected = rendered(comparer()(await extension.api({ target: "hoshidicts-offscreen", ...request })));
    const actual = rendered(comparer()(await relaySide.send(request)));
    assert.deepEqual(actual, expected, JSON.stringify(message));
    return expected;
  };
  for (const profileIndex of [undefined, 0, 1, 2, "1", null]) {
    const at = profileIndex === undefined ? {} : { profileIndex };
    const entries = await api({ type: "hd_api_term_entries", terms: ["cat", "食べる", "bird"], ...at });
    assert.equal(entries.results.length, 3);
    await api({ type: "hd_api_kanji_entries", characters: ["食"], ...at });
    await api({ type: "hd_api_tokenize", texts: ["cat 食べる\nbird"], ...at });
    await api({ type: "hd_api_dictionaries", ...at });
    await api({ type: "hd_api_anki_card_formats", ...at });
    if (jsdomEntry) {
      await api({ type: "hd_api_anki_fields", text: "cat", entryType: "term", markers: ["expression", "reading", "furigana",
        "glossary", "part-of-speech", "frequencies", "sentence"], includeMedia: true, ...at });
      await api({ type: "hd_api_anki_fields", text: "食べる", entryType: "term", markers: ["furigana", "glossary"], ...at });
    }
  }
  const english = await api({ type: "hd_api_term_entries", terms: ["cat"], profileIndex: 1 });
  assert.ok(english.results[0].dictionaryEntries.length > 0, "English's personal dictionary answers profile 1");
  const japanese = await api({ type: "hd_api_term_entries", terms: ["cat"], profileIndex: 0 });
  assert.equal(japanese.results[0].dictionaryEntries.length, 0);
  const formats = await api({ type: "hd_api_anki_card_formats", profileIndex: 2 });
  assert.match(formats.error, /valid profile index but got 2/u);
  if (!jsdomEntry) t.diagnostic("jsdom is unavailable: the API's term fields were not compared");
  reply = await step("English fixture off", (stored) => ({ type: "hd_apply_state", profileId: "en",
    baseRevision: state(stored).revision, groups: [],
    dictionaries: view(stored, "en").map((row, index) => (index === 0 ? row : { ...row, enabled: false })) }));
  assert.equal(reply.ok, true, reply.error);
  assert.equal(await found("English fixture off", "食べる", "en"), 0);
  assert.ok(await found("English fixture off", "食べる", "ja") > 0);
  reply = await step("legacy Japanese view", (stored) => ({ type: "hd_apply_state", baseRevision: state(stored).revision,
    dictionaries: [view(stored, "ja")[0], ...view(stored, "ja").slice(1).reverse()], groups: [] }));
  assert.equal(reply.ok, true, reply.error);
  assert.ok(await found("reordered", "食べる") > 0);
  reply = await step("stale view", (stored) => ({ type: "hd_apply_state", profileId: "en", baseRevision: state(stored).revision - 1,
    dictionaries: view(stored, "en"), groups: [] }));
  assert.equal(reply.conflict, true);
  reply = await step("view of a removed profile", (stored) => ({ type: "hd_apply_state", profileId: "fr",
    baseRevision: state(stored).revision, dictionaries: [], groups: [] }));
  assert.equal(reply.errorCode, "profile-missing");

  // Removal: an older client may not delete what English uses; one naming a
  // profile removes the package from every profile.
  const shared = (stored) => view(stored, "en").at(-1);
  reply = await step("legacy removal of a shared package", (stored) => ({ type: "hd_remove", title: shared(stored).title,
    id: shared(stored).id }));
  assert.equal(reply.ok, false);
  assert.match(reply.error, /used by English/u);
  reply = await step("removal of English's personal package", (stored) => ({ type: "hd_remove", profileId: "en",
    title: view(stored, "en")[0].title, id: view(stored, "en")[0].id }));
  assert.equal(reply.ok, false);
  reply = await step("removal for a profile", (stored) => ({ type: "hd_remove", profileId: "ja", title: shared(stored).title,
    id: shared(stored).id }));
  assert.equal(reply.ok, true, reply.error);
  assert.equal(state(await relaySide.stored()).dictionaries.length, FIXTURES.length + 1);

  // Removing English keeps its package in the library; adding it again brings
  // it back. An erase with a stale confirmation keeps the source.
  reply = await step("remove English", { type: "hd_profile_remove", profileId: "en" });
  assert.equal(reply.ok, true, reply.error);
  assert.equal(reply.state.schemaVersion, 2);
  reply = await step("remove English again", { type: "hd_profile_remove", profileId: "en" });
  assert.equal(reply.errorCode, "profile-missing");
  reply = await step("add English back", { type: "hd_profile_add", language: "en", copyFrom: "ja" });
  assert.equal(reply.ok, true, reply.error);
  assert.ok(await found("English back", "cat", "en") > 0);
  for (const message of [{ profileId: "" }, { language: "en" }, { profileId: "en", erase: true }]) {
    reply = await step(`remove ${JSON.stringify(message)}`, { type: "hd_profile_remove", ...message });
    assert.equal(reply.ok, false);
  }
  reply = await step("erase English stale", (stored) => ({ type: "hd_profile_remove", profileId: "en", erase: true,
    baseDocumentRevision: englishRevision(stored) + 1 }));
  assert.equal(reply.stale, true);
  reply = await step("erase English", (stored) => ({ type: "hd_profile_remove", profileId: "en", erase: true,
    baseDocumentRevision: englishRevision(stored) }));
  assert.equal(reply.ok, true, reply.error);
  assert.ok(await found("erased", "食べる") > 0);
  reply = await step("remove the last language", { type: "hd_profile_remove", profileId: "ja" });
  assert.equal(reply.ok, false);
});
