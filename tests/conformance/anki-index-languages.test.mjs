// The duplicate index with language profiles in a running relay: each
// profile's first Template keeps its own snapshot and schedule, answered for
// the profile a request names, under one row revision. Not from upstream: it
// follows upstream's test/anki-index-languages.test.mjs at d41c37a, whose
// cases run the extension's cache with a fake clock; here the stored attempt
// times are set around the real clock before the relay starts, as
// anki-index.test.mjs does. It needs upstream's AnkiConnect fake, so it skips
// (fails under HACHIDORI_ORACLES=required) without a Hachidori checkout.
// SPDX-License-Identifier: GPL-3.0-or-later
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { startAnkiConnect } from "./anki-fake.mjs";
import { relayBinary, startAnkiRelayServer } from "./anki-relay-server.mjs";
import { awareClient, linkClient } from "./linked-client.mjs";
import { oracleSkip } from "./oracle-skip.mjs";
import { eventually } from "./relay-socket.mjs";

const REPO = fileURLToPath(new URL("../..", import.meta.url));
const HACHIDORI = resolve(process.env.HACHIDORI_SOURCE ?? join(REPO, "..", "hachidori"));
const HOST_MODE = execFileSync(relayBinary(), ["--help"], { encoding: "utf8" }).includes("--host");
const FAKE = join(HACHIDORI, "test/anki-connect-fake.mjs");
const skip = oracleSkip([...(HOST_MODE ? [] : ["a relay built with host mode"]), ...(existsSync(FAKE) ? [] : [FAKE])]);
const INDEX_KEY = "ankiDuplicateIndex";
const REFRESH_MS = 30 * 60 * 1000;
const HEADWORDS = ["猫", "run", "walk"];

// upstream's fixture: a profile's first Template.
const template = (id, model, expression, patch = {}) => ({ id, name: id, deck: "Default", model, tags: [],
  fields: { expression, reading: "", definition: "", sentence: "", frequency: "", pitch: "", audio: "", screenshot: "" },
  duplicateScope: "model", duplicateBehavior: "prevent", captureScreenshot: false, fieldTemplates: null, ...patch });
const profile = (id, language, first) => ({ id, language, settings: { anki: { templates: [first] } }, packSettings: {} });
const records = (url, english = template("words", "English", "Word")) => ({ revision: 4, anki: { url, apiKey: "" },
  profiles: [profile("ja", "ja", template("mining", "Kiku", "Expression")), profile("en", "en", english)] });

async function fake() {
  const { answerAnkiConnect, AnkiConnectError } = await import(pathToFileURL(FAKE));
  const anki = await startAnkiConnect(answerAnkiConnect, AnkiConnectError);
  anki.state.models.English = { id: 1004, fields: ["Word", "Meaning"] };
  anki.state.models["English 2"] = { id: 1005, fields: ["Word", "Meaning"] };
  const note = (noteId, modelName, word, mature = false) => ({ noteId, modelName, deckName: "Default", mature,
    fields: { Word: word, Meaning: "" } });
  anki.state.notes.push(note(21, "English", "run"), note(31, "English 2", "walk", true));
  return anki;
}

// The pulls a fake has answered for one note type: each searches it.
const pullsOf = (anki, model) => anki.state.log
  .filter((entry) => entry.action === "findNotes" && !/prop:ivl/u.test(entry.params?.query ?? "")
    && (entry.params?.query ?? "").includes(`"note:${model}"`) && !/:(?:run|walk|猫)/u.test(entry.params?.query ?? "")).length;

function dataDir(t, seed) {
  const dir = mkdtempSync(join(tmpdir(), "hachidori-relay-index-languages-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const file = join(dir, "storage.json");
  writeFileSync(file, JSON.stringify(seed));
  const read = () => JSON.parse(readFileSync(file, "utf8"));
  return {
    dir,
    index: () => read()[INDEX_KEY] ?? null,
    options: () => read().options,
    rewrite(change) {
      const stored = read();
      stored[INDEX_KEY] = change(stored[INDEX_KEY]);
      writeFileSync(file, JSON.stringify(stored));
    },
  };
}

// A running relay over `data` and a linked page that opted in to profiles.
async function relay(t, data) {
  const server = await startAnkiRelayServer({ extraArgs: ["--host", "--data-dir", data.dir] });
  const link = await awareClient(t, (context, options) => linkClient(context, server.port, options), ["en"],
    { hello: { capabilities: ["linked-anki-v2", "language-profiles-v1"] } });
  assert.equal(link.optIn?.optIn, "accepted", JSON.stringify(link.optIn));
  let id = 0;
  const ask = (message) => link.send({ requestId: `r${++id}`, ...message });
  const statuses = async (profileId) => {
    const reply = await ask({ target: "hachidori-anki", type: "hd_anki_word_status", profileId, request: { headwords: HEADWORDS } });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    return reply.statuses;
  };
  return { close: () => server.close(), ask, statuses };
}

// Every listed source's pull since `after` has ended.
const settled = (data, count, after = 0) => eventually("the index pulls to end", () => {
  const index = data.index();
  const attempts = index?.attempts ?? (index?.attempt ? [index.attempt] : []);
  assert.equal(attempts.length, count, JSON.stringify(index));
  for (const attempt of attempts) {
    assert.equal(typeof attempt.finishedAt, "number");
    assert.ok(attempt.startedAt > after, "a new attempt");
  }
  return index;
}, 20_000);

async function started(t, seed) {
  const anki = await fake();
  const data = dataDir(t, { options: seed(anki.url) });
  let running = await relay(t, data);
  t.after(async () => {
    await running.close();
    await anki.close();
  });
  const restart = async () => {
    await running.close();
    running = await relay(t, data);
    return running;
  };
  return { anki, data, relay: () => running, restart };
}

const keysOf = (entries) => entries.map((entry) => entry.sourceKey);

test("two profiles' first Templates keep one snapshot each, answered for the profile each request names", { skip, timeout: 120_000 }, async (t) => {
  const { anki, data, relay: current } = await started(t, records);
  const index = await settled(data, 2);
  assert.equal(Object.hasOwn(index, "snapshot"), false);
  assert.equal(Object.hasOwn(index, "attempt"), false);
  assert.deepEqual(keysOf(index.snapshots), keysOf(index.attempts), "one of each per source, in profile order");
  assert.equal(new Set(keysOf(index.snapshots)).size, 2);
  assert.deepEqual(index.snapshots.map((snapshot) => snapshot.rows.map(([word]) => word).sort()),
    [["a&lt;b", "abc", "犬", "猫", "鳥"].sort(), ["run"]]);
  assert.ok(index.attempts[1].startedAt >= index.attempts[0].finishedAt, "one pull at a time");
  // Word status reads each profile's all-decks snapshot, which only highlighting keeps.
  assert.equal(await current().statuses("ja"), null, "disabled highlighting has no broad snapshot");
  assert.equal(await current().statuses("en"), null);
  // An English profile without note-type families recognises no Kiku or Lapis note type beside its own.
  assert.equal(pullsOf(anki, "Lapis v1"), 0);
});

test("a repair in one profile moves the one row revision every snapshot shares", { skip, timeout: 120_000 }, async (t) => {
  const { anki, data, relay: current } = await started(t, records);
  const before = await settled(data, 2);
  anki.state.notes.push({ noteId: 22, modelName: "English", deckName: "Default", mature: false, fields: { Word: "walk", Meaning: "" } });
  const status = await current().ask({ target: "hachidori-anki", type: "hd_anki_status", profileId: "en" });
  assert.equal(status.available, true, JSON.stringify(status));
  const repaired = await current().ask({ target: "hachidori-anki", type: "hd_anki_browse", profileId: "en",
    request: { expression: "walk", configKey: status.configKey } });
  assert.equal(repaired.ok, true, JSON.stringify(repaired));
  assert.deepEqual(repaired.noteIds, [22]);
  const after = data.index();
  assert.equal(after.rowRevision, before.rowRevision + 1);
  assert.deepEqual(after.snapshots[0], before.snapshots[0], "the Japanese snapshot is untouched");
  assert.deepEqual(after.snapshots[1].rows, [["run", false, [21]], ["walk", false, [22]]]);
  assert.equal(after.snapshots[1].refreshedAt, before.snapshots[1].refreshedAt);
});

test("each source backs off on its own after a failed pull", { skip, timeout: 120_000 }, async (t) => {
  const { anki, data, relay: current, restart } = await started(t, records);
  const first = await settled(data, 2);
  await current().close();
  const due = Date.now() - REFRESH_MS - 1000;
  data.rewrite((index) => ({ ...index, attempts: index.attempts.map((attempt) => ({ ...attempt, startedAt: due, finishedAt: due + 500 })) }));
  anki.state.faults = [{ action: "findNotes", kind: "error", filter: (params) => params.query.includes("\"note:English\"") }];
  await restart();
  const failed = await settled(data, 2, due);
  assert.deepEqual(failed.snapshots[1], first.snapshots[1], "the failed source keeps its snapshot");
  assert.ok(failed.snapshots[0].refreshedAt > first.snapshots[0].refreshedAt, "the other source refreshed");
  anki.state.faults = [];
  const [ja, en] = [pullsOf(anki, "Kiku"), pullsOf(anki, "English")];
  await restart();
  await new Promise((done) => setTimeout(done, 500));
  assert.deepEqual([pullsOf(anki, "Kiku"), pullsOf(anki, "English")], [ja, en], "each keeps its 30-minute backoff");
});

test("one profile's new source invalidates by profile, and back to one source the record is the single one", { skip, timeout: 120_000 }, async (t) => {
  const { anki, data, relay: current } = await started(t, records);
  const first = await settled(data, 2);
  const write = async (english) => {
    const reply = await current().ask({ target: "hoshidicts-worker", type: "hd_options_write", profileId: "en",
      baseRevision: data.options().revision, options: { anki: english } });
    assert.equal(reply.ok, true, JSON.stringify(reply));
  };
  const english = ({ id: _id, name: _name, ...config }) => ({ ...data.options().anki, ...config, templates: [{ id: _id, name: _name, ...config }] });
  await write(english(template("words", "English 2", "Word")));
  let index = data.index();
  assert.equal(index.configurationRevision, first.configurationRevision + 1);
  assert.deepEqual(index.snapshots.map((snapshot) => snapshot.sourceKey), [first.snapshots[0].sourceKey],
    "the old English snapshot goes; Japanese keeps its own");
  assert.deepEqual(index.attempts[0], first.attempts[0], "Japanese keeps its schedule");
  index = await eventually("the new English source to be pulled", () => {
    const now = data.index();
    assert.equal(now.snapshots.length, 2);
    return now;
  }, 20_000);
  assert.deepEqual(index.snapshots[1].rows, [["walk", true, [31]]]);

  // A profile whose first Template names no note type adds no source: one is left.
  await write(english(template("words", "", "Word")));
  index = data.index();
  assert.equal(index.configurationRevision, first.configurationRevision + 2);
  assert.deepEqual(Object.keys(index).sort(), ["attempt", "configurationRevision", "rowRevision", "snapshot", "version"]);
  assert.equal(index.snapshot.sourceKey, first.snapshots[0].sourceKey);
  assert.deepEqual(index.attempt, first.attempts[0], "a record with several sources keeps the remaining attempt");
  assert.equal(pullsOf(anki, "English 2"), 1);
});

test("highlighting keeps each profile's all-decks source and language families separate", { skip, timeout: 120_000 }, async (t) => {
  const seed = (url) => {
    const options = records(url);
    options.profiles[0].settings.wordHighlightEnabled = true;
    return options;
  };
  const { anki, data, relay: current } = await started(t, seed);
  const index = await settled(data, 3);
  assert.deepEqual(index.snapshots.map((snapshot) => snapshot.rows.map(([word]) => word).sort()),
    [["a&lt;b", "abc", "犬", "猫", "鳥"].sort(), ["a&lt;b", "abc", "犬", "猫", "鳥"].sort(), ["run"]]);
  assert.deepEqual(await current().statuses("ja"), ["known", "unknown", "unknown"]);
  assert.equal(await current().statuses("en"), null);
  // A language without supported families must not start indexing Japanese
  // note types when its user enables highlighting.
  const lapis = pullsOf(anki, "Lapis v1");
  const reply = await current().ask({ target: "hoshidicts-worker", type: "hd_options_write", profileId: "en",
    baseRevision: data.options().revision, options: { wordHighlightEnabled: true } });
  assert.equal(reply.ok, true, JSON.stringify(reply));
  await eventually("the English all-decks source to be pulled", () => assert.equal(data.index().snapshots.length, 4), 20_000);
  assert.deepEqual(await current().statuses("en"), ["unknown", "learning", "unknown"]);
  assert.equal(pullsOf(anki, "Lapis v1"), lapis, "the English all-decks pull names no Japanese family");
});
