// The duplicate index's lifecycle in a running relay: what a restart restores
// and repeats, an interrupted or failed pull, malformed replies, and option
// changes that do or do not replace the index. Not from upstream: it follows
// upstream's test/anki-index-cache.test.mjs at d41c37a, whose cases run the
// extension's cache with a fake clock; here the stored index's attempt times
// are set around the real clock before the relay starts. It needs upstream's
// AnkiConnect fake, so it skips (fails under HACHIDORI_ORACLES=required)
// without a Hachidori checkout.
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
import { oracleSkip } from "./oracle-skip.mjs";
import { connectClient, eventually, untilKind } from "./relay-socket.mjs";

const REPO = fileURLToPath(new URL("../..", import.meta.url));
const HACHIDORI = resolve(process.env.HACHIDORI_SOURCE ?? join(REPO, "..", "hachidori"));
const HOST_MODE = execFileSync(relayBinary(), ["--help"], { encoding: "utf8" }).includes("--host");
const FAKE = join(HACHIDORI, "test/anki-connect-fake.mjs");
const skip = oracleSkip([...(HOST_MODE ? [] : ["a relay built with host mode"]), ...(existsSync(FAKE) ? [] : [FAKE])]);
const INDEX_KEY = "ankiDuplicateIndex";
const REFRESH_MS = 30 * 60 * 1000;
const HEADWORDS = ["猫", "犬", "魚", "鯨"];

const ankiOptions = (url, fields = {}) => ({
  url, apiKey: "", deck: "Mining", model: "Kiku", tags: ["hachidori"], duplicateScope: "all", duplicateBehavior: "prevent",
  captureScreenshot: true, fields: Object.fromEntries(["expression", "reading", "definition", "sentence", "frequency", "pitch", "audio", "screenshot"]
    .map((field) => [field, ""])),
  fieldTemplates: { Expression: { value: "{expression}", overwriteMode: "coalesce" } },
  ...fields,
});

async function fake() {
  const { answerAnkiConnect, AnkiConnectError } = await import(pathToFileURL(FAKE));
  return startAnkiConnect(answerAnkiConnect, AnkiConnectError);
}

// The index pulls a fake has answered: each one first lists the note types.
const pulls = (anki) => anki.state.log.filter((entry) => entry.action === "modelNamesAndIds").length;

// A data directory the relay restarts over; `index` reads or rewrites the stored index.
function dataDir(t, seed) {
  const dir = mkdtempSync(join(tmpdir(), "hachidori-relay-index-"));
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

// A running relay over `data` and a linked page asking it.
async function relay(t, data) {
  const server = await startAnkiRelayServer({ extraArgs: ["--host", "--data-dir", data.dir] });
  const socket = await connectClient(t, server.port, "/link");
  socket.send(JSON.stringify({ kind: "hello", protocol: 1, version: "0.9.0", name: "linked", capabilities: ["linked-anki-v2"] }));
  await untilKind(socket, "hello");
  let id = 0;
  const ask = async (message) => {
    id += 1;
    socket.send(JSON.stringify({ kind: "request", id, message: { requestId: `r${id}`, ...message } }));
    for (;;) {
      const frame = await socket.json(`the reply to ${message.type}`);
      if (frame.kind === "reply" && frame.id === id) return frame.response;
    }
  };
  const statuses = async () => {
    const reply = await ask({ target: "hachidori-anki", type: "hd_anki_word_status", request: { headwords: HEADWORDS } });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    return reply.statuses;
  };
  const writeOptions = async (anki) => {
    const reply = await ask({ target: "hoshidicts-worker", type: "hd_options_write", baseRevision: data.options().revision, options: { anki } });
    assert.equal(reply.ok, true, JSON.stringify(reply));
  };
  return { close: () => server.close(), ask, statuses, writeOptions };
}

// The pull a relay start or change runs has ended: its attempt carries an outcome.
const settled = (data, after = 0) => eventually("the index pull to end", () => {
  const attempt = data.index()?.attempt;
  assert.equal(typeof attempt?.finishedAt, "number");
  assert.ok(attempt.startedAt > after, "a new attempt");
  return data.index();
});

async function started(t) {
  const anki = await fake();
  const data = dataDir(t, { options: { revision: 1, anki: ankiOptions(anki.url) } });
  let running = await relay(t, data);
  // The last relay first: its connections would keep the fake open.
  t.after(async () => {
    await running.close();
    await anki.close();
  });
  const first = await settled(data);
  assert.ok(first.snapshot.rows.length > 0);
  const restart = async () => {
    await running.close();
    running = await relay(t, data);
    return running;
  };
  return { anki, data, first, relay: () => running, restart };
}

test("a restarted relay restores the snapshot without repeating a recent pull, even a failed one", { skip, timeout: 120_000 }, async (t) => {
  const { anki, data, first, relay: current, restart } = await started(t);
  const known = await current().statuses();
  assert.ok(known.some((status) => status !== "unknown"), JSON.stringify(known));

  let calls = pulls(anki);
  let relayNow = await restart();
  assert.deepEqual(await relayNow.statuses(), known, "the snapshot answers at once");
  assert.equal(pulls(anki), calls, "a pull finished minutes ago is not repeated");

  // A pull that ran to failure keeps its backoff and the previous snapshot.
  await relayNow.close();
  data.rewrite((index) => ({ ...index, attempt: { ...index.attempt, startedAt: Date.now() - REFRESH_MS - 1000, finishedAt: Date.now() - REFRESH_MS } }));
  anki.state.refuse = true;
  const before = data.index().attempt.startedAt;
  relayNow = await restart();
  const failed = await settled(data, before);
  assert.deepEqual(failed.snapshot, first.snapshot, "a failed pull keeps the snapshot");
  assert.deepEqual(await relayNow.statuses(), known);
  anki.state.refuse = false;
  calls = pulls(anki);
  relayNow = await restart();
  assert.deepEqual(await relayNow.statuses(), known);
  assert.equal(pulls(anki), calls, "a recent failed pull is not repeated by a restart");
  assert.equal(data.index().attempt.startedAt, failed.attempt.startedAt);
});

// Upstream's anki-index-cache.test.mjs "the complete index refreshes every 30
// minutes and retries if a post-write update races its pull": a row repaired
// while a pull runs is not erased by that pull's older snapshot.
test("a repair while a pull runs is kept: the pull is retried", { skip, timeout: 120_000 }, async (t) => {
  const { answerAnkiConnect, AnkiConnectError } = await import(pathToFileURL(FAKE));
  let gate = null;
  const reached = new Promise((resolveReached) => {
    let release;
    gate = { reached: resolveReached, released: new Promise((resolveRelease) => { release = resolveRelease; }), release: () => release() };
  });
  const held = gate;
  const anki = await startAnkiConnect(async (body, handle) => {
    if (gate !== null && JSON.stringify(body).includes("\"modelNamesAndIds\"")) {
      const current = gate;
      gate = null;
      current.reached();
      await current.released;
    }
    return answerAnkiConnect(body, handle);
  }, AnkiConnectError);
  const data = dataDir(t, { options: { revision: 1, anki: ankiOptions(anki.url) } });
  const running = await relay(t, data);
  t.after(async () => {
    held.release();
    await running.close();
    await anki.close();
  });
  await reached;
  anki.state.notes.push({ noteId: 40, modelName: "Kiku", deckName: "Mining", mature: false,
    fields: Object.fromEntries(anki.state.models.Kiku.fields.map((field, index) => [field, index === 0 ? "鯨" : ""])) });
  const status = await running.ask({ target: "hachidori-anki", type: "hd_anki_status" });
  assert.equal(status.available, true, JSON.stringify(status));
  const repaired = await running.ask({ target: "hachidori-anki", type: "hd_anki_browse", request: { expression: "鯨", configKey: status.configKey } });
  assert.equal(repaired.ok, true, JSON.stringify(repaired));
  assert.deepEqual(repaired.noteIds, [40]);
  const moved = data.index().rowRevision;
  // The held pull is logged once it runs; the repair listed the note types too.
  const calls = pulls(anki);
  assert.ok(moved >= 1, JSON.stringify(data.index()));
  held.release();
  const index = await eventually("the retried pull to commit", () => {
    const now = data.index();
    assert.ok(now.snapshot?.rows.some(([word]) => word === "鯨"), JSON.stringify(now));
    assert.equal(typeof now.attempt?.finishedAt, "number");
    return now;
  }, 20_000);
  assert.equal(pulls(anki), calls + 2, "the held pull, then one retry");
  assert.ok(index.rowRevision > moved);
  assert.equal((await running.statuses())[3], "learning", "the repaired note answers");
});

test("a pull interrupted before its commit is retried by the next start", { skip, timeout: 120_000 }, async (t) => {
  const { anki, data, relay: current, restart } = await started(t);
  await current().close();
  // The relay that reserved this pull stopped before it ended: no outcome.
  data.rewrite((index) => ({ ...index, attempt: { sourceKey: index.attempt.sourceKey, startedAt: Date.now() - 60_000 } }));
  const before = data.index().attempt.startedAt;
  const calls = pulls(anki);
  await restart();
  await settled(data, before);
  assert.equal(pulls(anki), calls + 1, "an orphaned attempt is due at once, not at its 30-minute mark");
});

test("malformed replies keep the previous snapshot instead of publishing partial rows", { skip, timeout: 120_000 }, async (t) => {
  const { anki, data, first, relay: current, restart } = await started(t);
  const known = await current().statuses();
  for (const result of ["nope", [1, "two"], null]) {
    await current().close();
    data.rewrite((index) => ({ ...index, attempt: { ...index.attempt, startedAt: Date.now() - REFRESH_MS - 1000, finishedAt: Date.now() - REFRESH_MS } }));
    const before = data.index().attempt.startedAt;
    anki.state.faults = [{ action: "findNotes", kind: "result", result }];
    anki.state.ran = [];
    const relayNow = await restart();
    const after = await settled(data, before);
    assert.deepEqual(anki.state.faults, [], "the malformed reply was sent");
    assert.deepEqual(after.snapshot, first.snapshot, `a malformed reply (${JSON.stringify(result)}) changes nothing`);
    assert.deepEqual(await relayNow.statuses(), known);
  }
});

test("a scope change replaces the index at once, a policy change keeps it", { skip, timeout: 120_000 }, async (t) => {
  const { anki, data, first, relay: current } = await started(t);
  const relayNow = current();
  let calls = pulls(anki);
  await relayNow.writeOptions(ankiOptions(anki.url, { duplicateBehavior: "overwrite" }));
  assert.equal(data.index().configurationRevision, first.configurationRevision);
  await new Promise((resolveWait) => setTimeout(resolveWait, 500));
  assert.equal(pulls(anki), calls, "a duplicate policy keeps the index");
  const known = await relayNow.statuses();
  assert.ok(known.some((status) => status !== "unknown"));

  calls = pulls(anki);
  await relayNow.writeOptions(ankiOptions(anki.url, { duplicateBehavior: "overwrite", duplicateScope: "deck" }));
  assert.equal(data.index().configurationRevision, first.configurationRevision + 1);
  await settled(data, first.attempt.startedAt);
  assert.equal(pulls(anki), calls + 1, "a duplicate scope replaces the index at once");
});

test("an endpoint change never answers from the old collection", { skip, timeout: 120_000 }, async (t) => {
  const { anki, data, first, relay: current } = await started(t);
  const relayNow = current();
  const known = await relayNow.statuses();
  // Another collection, without the words the first one holds.
  const other = await fake();
  t.after(() => other.close());
  other.state.notes = [];
  await relayNow.writeOptions(ankiOptions(other.url));
  // Before or after the replacement pull, the old collection's rows are never used.
  const during = await relayNow.statuses();
  assert.ok(during === null || during.every((status) => status === "unknown"), JSON.stringify(during));
  await settled(data, first.attempt.startedAt);
  assert.deepEqual(await relayNow.statuses(), HEADWORDS.map(() => "unknown"));
  assert.notDeepEqual(known, HEADWORDS.map(() => "unknown"));
  assert.ok(pulls(other) >= 1);
});
