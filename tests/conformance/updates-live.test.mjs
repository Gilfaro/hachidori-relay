// Host mode's managed updates and recommended installs against the live release of Bee's Ultimate
// Grammar Dictionary (github.com/bee-san/bees-ultimate-grammar-dictionary).
// They download about 35 MB, so they run only with HACHIDORI_LIVE_TESTS=1.
//
// - An older release, uploaded as a linked browser would, is a managed
//   dictionary by its own index.json: a check finds the live revision, an
//   install downloads it through the index's downloadUrl and replaces the
//   package, and a second check reads it as up to date.
// - A first install from the recommended catalogue downloads the latest
//   release asset and carries its catalogue source.
// - The recommended-dictionary installer (hd_setup_install) installs it,
//   reports its run to an observing request, and finds it installed on a
//   second run.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { relayBinary, startAnkiRelayServer } from "./anki-relay-server.mjs";
import { connectClient, untilKind } from "./relay-socket.mjs";

const HOST_MODE = execFileSync(relayBinary(), ["--help"], { encoding: "utf8" }).includes("--host");
const LIVE = process.env.HACHIDORI_LIVE_TESTS === "1";
const SKIP = (!HOST_MODE && "built without host mode") || (!LIVE && "set HACHIDORI_LIVE_TESTS=1 to download the live dictionary");

const REPOSITORY = "https://github.com/bee-san/bees-ultimate-grammar-dictionary";
const INDEX_URL = "https://raw.githubusercontent.com/bee-san/bees-ultimate-grammar-dictionary/main/dist/index.json";
const CATALOGUE_URL = `${REPOSITORY}/releases/latest/download/bees-ultimate-grammar-dictionary.zip`;
const OLD_REVISION = "2026.10.07.3";
const OLD_ARCHIVE = `${REPOSITORY}/releases/download/v${OLD_REVISION}/bees-ultimate-grammar-dictionary.zip`;
const TITLE = "Bee's Ultimate Grammar Dictionary";
const SOURCE_ID = "bees-ultimate-grammar-dictionary";
// A download and import of the whole dictionary.
const SLOW = 5 * 60 * 1000;

function scratch(t) {
  const dir = mkdtempSync(join(tmpdir(), "hachidori-relay-live-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

// A linked browser, and its requests to the relay.
async function linked(t) {
  const server = await startAnkiRelayServer({ extraArgs: ["--host", "--data-dir", scratch(t)] });
  t.after(() => server.close());
  const socket = await connectClient(t, server.port, "/link");
  assert.equal(socket.status, 101);
  socket.send(JSON.stringify({ kind: "hello", protocol: 1, version: "0.9.0", name: "Firefox", capabilities: [] }));
  await untilKind(socket, "hello");
  let next = 1;
  const ask = async (message, timeoutMs = SLOW) => {
    const id = next++;
    socket.send(JSON.stringify({ kind: "request", id, message: { requestId: `r${id}`, ...message } }));
    for (;;) {
      const frame = await socket.json(`the reply to ${message.type}`, timeoutMs);
      if (frame.kind === "reply" && frame.id === id) return frame.response;
    }
  };
  return { ask };
}

// `uploadDictionary`: the archive, sent in the chunks the relay asks for.
async function upload(ask, bytes, fileName) {
  const target = "hachidori-linked-import";
  const begun = await ask({ target, type: "hd_import_begin", fileName, size: bytes.length, replace: false });
  assert.equal(typeof begun.token, "string", JSON.stringify(begun));
  for (let offset = 0; offset < bytes.length; offset += begun.chunkBytes) {
    const data = bytes.subarray(offset, offset + begun.chunkBytes).toString("base64");
    const reply = await ask({ target, type: "hd_import_chunk", token: begun.token, offset, data });
    assert.notEqual(reply.ok, false, JSON.stringify(reply));
  }
  return ask({ target, type: "hd_import_commit", token: begun.token });
}

async function download(url) {
  const response = await fetch(url);
  assert.equal(response.status, 200, url);
  return response;
}

const dictionaries = async (ask) => (await ask({ target: "hoshidicts-worker", type: "hd_state_read" })).state.dictionaries;
const grammar = async (ask) => (await dictionaries(ask)).find((entry) => entry.title === TITLE);
const check = (ask, id) => ask({ target: "hachidori-updates", type: "hd_updates_check", dictionaryIds: [id] });
const found = async (ask, text) => {
  const reply = await ask({ target: "hoshidicts-offscreen", type: "hd_lookup", text });
  assert.equal(reply.ok, true, reply.error);
  return reply.results.length;
};

test("an older release updates to the live one through its own update index", { skip: SKIP, timeout: 4 * SLOW }, async (t) => {
  const live = (await (await download(INDEX_URL)).json()).revision;
  assert.notEqual(live, OLD_REVISION, "the live release is newer than the one installed first");
  const { ask } = await linked(t);

  const committed = await upload(ask, Buffer.from(await (await download(OLD_ARCHIVE)).arrayBuffer()), "bees-ultimate-grammar-dictionary.zip");
  assert.equal(committed.ok, true, committed.error);
  const old = await grammar(ask);
  assert.equal(old.revision, OLD_REVISION);
  assert.ok(await found(ask, "について") > 0, "the old release answers lookups");

  let reply = await check(ask, old.id);
  assert.equal(reply.ok, true, reply.error);
  assert.deepEqual(reply.outcomes, [{ id: old.id, status: "update-available" }]);
  let stored = await grammar(ask);
  assert.deepEqual([stored.lastUpdateCheck.status, stored.lastUpdateCheck.remoteRevision], ["update-available", live]);

  reply = await ask({ target: "hachidori-updates", type: "hd_updates_install", dictionaryIds: [old.id] });
  assert.equal(reply.ok, true, reply.error);
  assert.deepEqual(reply.outcomes, [{ id: old.id, status: "updated" }], JSON.stringify(reply.outcomes));
  const installed = await dictionaries(ask);
  assert.equal(installed.filter((entry) => entry.title === TITLE).length, 1, "the update replaced the old package");
  stored = await grammar(ask);
  assert.equal(stored.revision, live);
  assert.notEqual(stored.path, old.path);
  assert.equal(stored.indexUrl, INDEX_URL);
  assert.deepEqual([stored.lastUpdateCheck.status, stored.lastUpdateCheck.remoteRevision], ["up-to-date", live]);
  assert.ok(await found(ask, "について") > 0, "the new release answers lookups");

  reply = await check(ask, stored.id);
  assert.deepEqual(reply.outcomes, [{ id: stored.id, status: "up-to-date" }]);
});

test("a first install from the recommended catalogue downloads the latest release", { skip: SKIP, timeout: 3 * SLOW }, async (t) => {
  const live = (await (await download(INDEX_URL)).json()).revision;
  const { ask } = await linked(t);

  // Only the catalogue's own archive URL is downloaded for a recommended source.
  let reply = await ask({ target: "hoshidicts-offscreen", type: "hd_import", archiveUrl: OLD_ARCHIVE, sourceId: SOURCE_ID });
  assert.equal(reply.ok, false);
  assert.match(reply.error, /must be downloaded from its catalogue archive URL/u);

  reply = await ask({ target: "hoshidicts-offscreen", type: "hd_import", archiveUrl: CATALOGUE_URL, sourceId: SOURCE_ID,
    fileName: "bees-ultimate-grammar-dictionary.zip" });
  assert.equal(reply.ok, true, reply.error);
  const stored = await grammar(ask);
  assert.equal(stored.revision, live);
  assert.deepEqual([stored.sourceId, stored.downloadUrl, stored.indexUrl, stored.isUpdatable], [SOURCE_ID, CATALOGUE_URL, INDEX_URL, true]);
  assert.ok(await found(ask, "について") > 0);

  reply = await check(ask, stored.id);
  assert.equal(reply.ok, true, reply.error);
  assert.deepEqual(reply.outcomes, [{ id: stored.id, status: "up-to-date" }]);
});

test("the recommended-dictionary installer downloads a source once and reports its run", { skip: SKIP, timeout: 3 * SLOW }, async (t) => {
  const live = (await (await download(INDEX_URL)).json()).revision;
  const { ask } = await linked(t);
  const install = (sourceIds) => ask({ target: "hachidori-setup", type: "hd_setup_install", sourceIds });

  const started = await install([SOURCE_ID]);
  assert.equal(started.ok, true, started.error);
  assert.equal(started.finished, false);
  assert.deepEqual(started.entries.map((entry) => entry.sourceId), [SOURCE_ID]);
  // Linked pages observe the run with an empty request, as recommended-install-client.js does.
  let run = started;
  for (const deadline = Date.now() + SLOW; !run.finished && Date.now() < deadline;) {
    await new Promise((resolve) => { setTimeout(resolve, 500); });
    run = await install([]);
    assert.equal(run.runId, started.runId);
  }
  assert.equal(run.finished, true, "the run finished");
  assert.deepEqual([run.entries[0].phase, run.entries[0].error], ["installed", null]);
  const stored = await grammar(ask);
  assert.deepEqual([stored.revision, stored.sourceId], [live, SOURCE_ID]);
  assert.ok(await found(ask, "について") > 0);

  // A new run finds it installed and downloads nothing.
  const again = await install([SOURCE_ID]);
  assert.notEqual(again.runId, started.runId);
  for (run = again; !run.finished;) {
    await new Promise((resolve) => { setTimeout(resolve, 100); });
    run = await install([]);
  }
  assert.equal(run.entries[0].phase, "already-installed");
  assert.equal((await dictionaries(ask)).filter((entry) => entry.title === TITLE).length, 1);
});
