// Host mode, milestone 1, over real sockets against the binary: a browser host's
// configuration and dictionaries are mirrored into the data directory, kept up to
// date, served to linked browsers, and still there after a restart with no browser
// host. Not from upstream. Needs a binary built with `--features host`; it is skipped
// otherwise. `src/host/tests.rs` tells the same story in process.
// SPDX-License-Identifier: GPL-3.0-or-later
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { crc32 } from "node:zlib";
import { relayBinary, startAnkiRelayServer } from "./anki-relay-server.mjs";
import { connectClient, connectHost, eventually, untilKind } from "./relay-socket.mjs";

const HOST_MODE = execFileSync(relayBinary(), ["--help"], { encoding: "utf8" }).includes("--host");
const GENERATION_1 = "/dicts/.hdw-generation-1/Fixture";
const GENERATION_2 = "/dicts/.hdw-generation-2/Fixture";

// A stored (uncompressed) ZIP, as the extension's backup archives are.
function zip(entries) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  for (const [name, data] of entries) {
    const nameBytes = Buffer.from(name, "utf8");
    const bytes = Buffer.from(data);
    const crc = crc32(bytes);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(bytes.length, 18);
    local.writeUInt32LE(bytes.length, 22);
    local.writeUInt16LE(nameBytes.length, 26);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(20, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt32LE(crc, 16);
    central.writeUInt32LE(bytes.length, 20);
    central.writeUInt32LE(bytes.length, 24);
    central.writeUInt16LE(nameBytes.length, 28);
    central.writeUInt32LE(offset, 42);
    locals.push(local, nameBytes, bytes);
    centrals.push(central, nameBytes);
    offset += local.length + nameBytes.length + bytes.length;
  }
  const directory = Buffer.concat(centrals);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, directory, end]);
}

// The one-dictionary archive the extension's hd_api_dictionary_open builds.
function archive(path, blobs, id = "fixture") {
  const payload = [["dictionaries/0/.hoshidicts_5", ""], ["dictionaries/0/index.json", "{}"], ["dictionaries/0/blobs.bin", blobs]];
  const files = payload.map(([name, data]) => ({ path: name, size: Buffer.byteLength(data) }));
  const manifest = { format: "hachidori-backup", version: 3, createdAt: "2026-11-06T00:00:00.000Z",
    snapshot: { state: { dictionaries: [{ id, path, title: id }] } }, files };
  return zip([["hachidori-backup.json", JSON.stringify(manifest)], ...payload]);
}

function dictionaryState(path) {
  return { dictionaries: [{ id: "fixture", title: "Fixture", revision: "1", path }], groups: [], revision: 1, schemaVersion: 1 };
}

// A list of several dictionaries: [id, generation path] pairs, in order.
function dictionaryStates(entries) {
  return { dictionaries: entries.map(([id, path]) => ({ id, title: id, revision: "1", path })), groups: [], revision: 1, schemaVersion: 1 };
}

function generationDir(dir, path) {
  return join(dir, "dictionaries", createHash("sha1").update(path).digest("hex"));
}

const stored = (dir) => JSON.parse(readFileSync(join(dir, "storage.json"), "utf8"));
const installed = (dir) => readdirSync(join(dir, "dictionaries"));
// Waits until the installed generations are `expected`: the relay publishes a new list
// first and deletes the generations it no longer names just after.
async function installedBecome(dir, expected) {
  await eventually("the installed generations", () => assert.deepEqual(installed(dir).sort(), [...expected].sort()));
}

async function hostRelay(t, dir) {
  const server = await startAnkiRelayServer({ extraArgs: ["--host", "--data-dir", dir] });
  t.after(() => server.close());
  return server;
}

// The browser host's side: it answers the relay's pull session.
async function browserHost(t, port, snapshot) {
  const socket = await connectHost(t, port);
  const open = await untilKind(socket, "client-open");
  assert.equal(open.origin, "relay://hachidori-relay", "the pull session identifies as the relay mirror");
  const host = {
    socket,
    async said() {
      return JSON.parse((await untilKind(socket, "client-text")).text);
    },
    says(frame) {
      socket.send(JSON.stringify({ kind: "send", clientId: open.clientId, text: JSON.stringify(frame) }));
    },
    broadcast(frame) {
      socket.send(JSON.stringify({ kind: "broadcast", text: JSON.stringify(frame) }));
    },
    // One download of `bytes` in two chunks; the lease must be closed afterwards.
    async serveDownload(bytes, id = "fixture") {
      const opened = await host.said();
      assert.equal(opened.message.type, "hd_api_dictionary_open");
      assert.equal(opened.message.id, id);
      host.says({ kind: "reply", id: opened.id, response: { token: "dl-1", size: bytes.length, fileName: "f.zip" } });
      const half = Math.floor(bytes.length / 2);
      for (const [offset, part, eof] of [[0, bytes.subarray(0, half), false], [half, bytes.subarray(half), true]]) {
        const read = await host.said();
        assert.equal(read.message.type, "hd_api_dictionary_read");
        assert.equal(read.message.offset, offset);
        host.says({ kind: "reply", id: read.id, response: { data: part.toString("base64"), eof } });
      }
      const close = await host.said();
      assert.equal(close.message.type, "hd_api_dictionary_close");
      assert.equal(close.message.token, "dl-1");
    },
  };
  assert.equal((await host.said()).kind, "hello");
  host.says({ kind: "hello", protocol: 1, version: "0.9.0", name: "Chrome", dictionaryCount: 1, capabilities: ["hoshidicts-api-v1"], snapshot });
  return host;
}

async function link(t, port) {
  const socket = await connectClient(t, port, "/link");
  assert.equal(socket.status, 101);
  socket.send(JSON.stringify({ kind: "hello", protocol: 1, version: "0.9.0", name: "Firefox", capabilities: [] }));
  return { socket, hello: await untilKind(socket, "hello") };
}

function scratch(t) {
  const dir = mkdtempSync(join(tmpdir(), "hachidori-relay-host-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

test("a browser host is mirrored to disk, served to linked browsers, and outlives itself and the relay", { skip: !HOST_MODE && "built without host mode" }, async (t) => {
  const dir = scratch(t);
  const relay = await hostRelay(t, dir);

  // With no browser anywhere, a linked browser is still answered, by the relay.
  const { socket: linked, hello } = await link(t, relay.port);
  assert.equal(hello.name, "Hachidori Relay");
  assert.equal(hello.dictionaryCount, 0);
  assert.equal(hello.snapshot.options, null);

  // A browser host connects: its settings arrive at once, its dictionary after the download.
  const browser = await browserHost(t, relay.port, {
    dictionaryState: dictionaryState(GENERATION_1), options: { scanLength: 20 }, customDictionarySource: null,
    dictionaryUpdates: {}, lookupStats: { generation: "g", revision: 1 }, wordStatusOverrides: null,
  });
  const settings = await untilKind(linked, "storage");
  assert.deepEqual(settings.changes.options, { scanLength: 20 });
  assert.equal(settings.changes.dictionaryState, undefined, "the dictionary list waits for its files");
  await browser.serveDownload(archive(GENERATION_1, "first"));
  assert.deepEqual((await untilKind(linked, "storage")).changes.dictionaryState, dictionaryState(GENERATION_1));
  assert.equal(readFileSync(join(generationDir(dir, GENERATION_1), "blobs.bin"), "utf8"), "first");
  assert.deepEqual(stored(dir).options, { scanLength: 20 });
  assert.deepEqual(stored(dir).dictionaryState, dictionaryState(GENERATION_1));

  // The browser's later changes win, lookup-count rows included; unshared keys stay out.
  browser.broadcast({ kind: "storage", changes: { options: { scanLength: 30 }, "lookupStats:g:分かる": { lookupCount: 2 }, sharing: { enabled: true } } });
  const change = await untilKind(linked, "storage");
  assert.deepEqual(change.changes, { options: { scanLength: 30 }, "lookupStats:g:分かる": { lookupCount: 2 } });
  assert.deepEqual(stored(dir)["lookupStats:g:分かる"], { lookupCount: 2 });
  assert.equal(stored(dir).sharing, undefined);

  // A re-imported dictionary is a new generation: downloaded again, and the old one removed.
  browser.broadcast({ kind: "storage", changes: { dictionaryState: dictionaryState(GENERATION_2) } });
  await browser.serveDownload(archive(GENERATION_2, "second"));
  assert.deepEqual((await untilKind(linked, "storage")).changes.dictionaryState, dictionaryState(GENERATION_2));
  assert.deepEqual(installed(dir), [createHash("sha1").update(GENERATION_2).digest("hex")]);

  // The browser host leaving does not take the linked browser with it, and the relay
  // answers its update check itself: nothing it holds updates from a source.
  browser.socket.destroy();
  linked.send(JSON.stringify({ kind: "request", id: "r1", message: { target: "hachidori-updates", type: "hd_updates_check", requestId: "q" } }));
  const reply = await untilKind(linked, "reply");
  assert.equal(reply.id, "r1");
  assert.equal(reply.response.ok, true);
  assert.deepEqual(reply.response.outcomes, []);
  assert.equal(reply.response.type, "hd_updates_check_result");
  assert.equal(reply.response.requestId, "q");
  await relay.close();

  // After a restart, with no browser host, the mirror is what linked browsers get.
  const again = await hostRelay(t, dir);
  const { hello: restored } = await link(t, again.port);
  assert.equal(restored.dictionaryCount, 1);
  assert.deepEqual(restored.snapshot.options, { scanLength: 30 });
  assert.deepEqual(restored.snapshot.dictionaryState, dictionaryState(GENERATION_2));
  assert.deepEqual(restored.snapshot.lookupStats, { generation: "g", revision: 1 });
  assert.ok(existsSync(join(generationDir(dir, GENERATION_2), "index.json")));
});

test("a second browser host's dictionaries replace the first's, and only the ones it lacks are downloaded", { skip: !HOST_MODE && "built without host mode" }, async (t) => {
  const dir = scratch(t);
  const relay = await hostRelay(t, dir);
  const { socket: linked } = await link(t, relay.port);
  const generation = (name) => `/dicts/.hdw-generation-${name}/Dictionary`;
  const name = (path) => createHash("sha1").update(path).digest("hex");
  const settings = { options: null, customDictionarySource: null, dictionaryUpdates: {}, lookupStats: null, wordStatusOverrides: null };

  // The first browser shares two dictionaries.
  const first = dictionaryStates([["only-first", generation("a")], ["both", generation("shared")]]);
  const one = await browserHost(t, relay.port, { ...settings, dictionaryState: first });
  await one.serveDownload(archive(generation("a"), "a", "only-first"), "only-first");
  await one.serveDownload(archive(generation("shared"), "shared", "both"), "both");
  let change;
  do change = await untilKind(linked, "storage"); while (change.changes.dictionaryState === undefined);
  assert.deepEqual(change.changes.dictionaryState, first);
  assert.deepEqual(installed(dir).sort(), [name(generation("a")), name(generation("shared"))].sort());
  one.socket.destroy();

  // A second browser keeps one of them and has one of its own: only that one is
  // downloaded, and the first browser's other dictionary is deleted.
  const second = dictionaryStates([["both", generation("shared")], ["only-second", generation("b")]]);
  const two = await browserHost(t, relay.port, { ...settings, dictionaryState: second });
  await two.serveDownload(archive(generation("b"), "b", "only-second"), "only-second");
  do change = await untilKind(linked, "storage"); while (change.changes.dictionaryState === undefined);
  assert.deepEqual(change.changes.dictionaryState, second);
  await installedBecome(dir, [name(generation("b")), name(generation("shared"))]);
  assert.equal(readFileSync(join(generationDir(dir, generation("shared")), "blobs.bin"), "utf8"), "shared", "kept, not downloaded again");
  assert.ok(!existsSync(generationDir(dir, generation("a"))), "the first browser's other dictionary is deleted");
  assert.deepEqual(stored(dir).dictionaryState, second);
  two.socket.destroy();

  // A third browser without dictionaries leaves none behind.
  await browserHost(t, relay.port, { ...settings, dictionaryState: dictionaryStates([]) });
  do change = await untilKind(linked, "storage"); while (change.changes.dictionaryState === undefined);
  assert.deepEqual(change.changes.dictionaryState, dictionaryStates([]));
  await installedBecome(dir, []);
});

test("a failed download keeps the old list and is retried on the next change", { skip: !HOST_MODE && "built without host mode" }, async (t) => {
  const dir = scratch(t);
  const relay = await hostRelay(t, dir);
  const { socket: linked } = await link(t, relay.port);
  const browser = await browserHost(t, relay.port, { dictionaryState: dictionaryState(GENERATION_1), options: {} });
  await untilKind(linked, "storage");
  const opened = await browser.said();
  assert.equal(opened.message.type, "hd_api_dictionary_open");
  browser.says({ kind: "reply", id: opened.id, response: { error: "busy" } });
  assert.equal(stored(dir).dictionaryState, undefined);
  assert.deepEqual(installed(dir), []);

  // A newer list interrupts the retry wait, and installs.
  browser.broadcast({ kind: "storage", changes: { dictionaryState: dictionaryState(GENERATION_2) } });
  await browser.serveDownload(archive(GENERATION_2, "ok"));
  assert.deepEqual((await untilKind(linked, "storage")).changes.dictionaryState, dictionaryState(GENERATION_2));
});

test("a linked browser with another protocol is told bye and closed", { skip: !HOST_MODE && "built without host mode" }, async (t) => {
  const relay = await hostRelay(t, scratch(t));
  const socket = await connectClient(t, relay.port, "/link");
  socket.send(JSON.stringify({ kind: "hello", protocol: 2 }));
  assert.equal((await untilKind(socket, "bye")).reason, "unsupported sharing protocol 2");
  await socket.closeFrame();
});

// Upstream's sharing-protocol.test.mjs "frames are validated on both sides" and
// "linked Anki submissions have one 16 MiB UTF-8 frame limit": the host's
// parseClientFrame closes a link that sends either.
test("a malformed request or an oversized Anki submission is told bye", { skip: !HOST_MODE && "built without host mode" }, async (t) => {
  const relay = await hostRelay(t, scratch(t));
  for (const [frame, reason] of [
    [{ kind: "request", id: 1 }, "malformed sharing request"],
    [{ kind: "request", id: 1, message: [] }, "malformed sharing request"],
    [{ kind: "request", id: 1, message: { target: "hoshidicts-offscreen" } }, "malformed sharing request"],
    [{ kind: "request", id: 1, message: { target: 5, type: "hd_status" } }, "malformed sharing request"],
    [{ kind: "request", id: true, message: { target: "hoshidicts-offscreen", type: "hd_status" } }, "malformed sharing request"],
    [{ kind: "request", id: 1, message: { target: "hachidori-anki", type: "hd_anki_submit", request: { term: "é".repeat(8 * 1024 * 1024) } } },
      "The linked Anki submission exceeds the 16 MiB frame limit."],
  ]) {
    const { socket } = await link(t, relay.port);
    socket.send(JSON.stringify(frame));
    assert.equal((await untilKind(socket, "bye")).reason, reason, JSON.stringify(frame).slice(0, 120));
    await socket.closeFrame();
  }
});

test("without --host the relay still refuses clients while no browser host is connected", { skip: !HOST_MODE && "built without host mode" }, async (t) => {
  const relay = await startAnkiRelayServer();
  t.after(() => relay.close());
  const socket = await connectClient(t, relay.port, "/link");
  assert.notEqual(socket.status, 101);
});
