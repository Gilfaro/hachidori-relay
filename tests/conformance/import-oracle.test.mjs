// Host mode's dictionary uploads (linked-import-v1) against the extension itself:
// the same archives go, through the extension's own uploadDictionary sender, to the
// extension's upload host (linked-import.js' createUploadHost and
// sharing-protocol.js' allowLinkedImportRequest, with its engine importing them) and
// to the relay as a linked browser's requests. Every reply, the stored dictionary
// state after every step, lookups over the result and the downloads of what was
// imported must agree.
//
// Not from upstream. Needs what host-oracle.test.mjs needs; skipped otherwise.
// SPDX-License-Identifier: GPL-3.0-or-later
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import test from "node:test";
import zlib from "node:zlib";
import { startAnkiRelayServer } from "./anki-relay-server.mjs";
import { HACHIDORI, bootExtension, comparer, realFetch, serveAsBrowserHost, skip } from "./extension-host.mjs";
import { connectClient, untilKind } from "./relay-socket.mjs";

const TARGET = "hachidori-linked-import";
const STORED = ["dictionaryState", "options"];

// A stored ZIP of `files` ({name: text}).
function zip(files) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  for (const [name, text] of Object.entries(files)) {
    const nameBytes = Buffer.from(name);
    const data = Buffer.from(text);
    const crc = zlib.crc32(data);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(data.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(nameBytes.length, 26);
    locals.push(local, nameBytes, data);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(20, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt32LE(crc, 16);
    central.writeUInt32LE(data.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(nameBytes.length, 28);
    central.writeUInt32LE(offset, 42);
    centrals.push(central, nameBytes);
    offset += 30 + nameBytes.length + data.length;
  }
  const directory = Buffer.concat(centrals);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(centrals.length / 2, 8);
  end.writeUInt16LE(centrals.length / 2, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return new Uint8Array(Buffer.concat([...locals, directory, end]));
}

// A one-term Yomitan dictionary.
const dictionary = (index, term, bank) => zip({
  "index.json": JSON.stringify({ format: 3, revision: "1", sequenced: false, ...index }),
  "term_bank_1.json": bank ?? JSON.stringify([[term, "", "", "", 0, [`${term} defined`], 0, ""]]),
});

test("the relay imports uploaded dictionaries as the extension's own engine does", { skip, timeout: 180_000 }, async (t) => {
  const extension = await bootExtension();
  const dir = mkdtempSync(join(tmpdir(), "hachidori-relay-imports-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const relay = await startAnkiRelayServer({ apiPort: 0, extraArgs: ["--host", "--data-dir", dir] });
  t.after(() => relay.close());
  const linked = await connectClient(t, relay.port, "/link");
  linked.send(JSON.stringify({ kind: "hello", protocol: 1, version: "0.9.0", name: "Firefox", capabilities: [] }));
  const hello = await untilKind(linked, "hello");
  assert.deepEqual(hello.capabilities, ["hoshidicts-api-v1", "linked-import-v1", "linked-anki-v2"]);
  await serveAsBrowserHost(t, relay.port, extension);
  let mirrored = null;
  while (mirrored === null) mirrored = (await untilKind(linked, "storage")).changes.dictionaryState ?? null;

  // The extension's upload host, wired as background-sharing.js wires it, with the
  // bytes held as the offscreen document holds them.
  const module = (name) => import(pathToFileURL(join(HACHIDORI, "extension", name)));
  const { createUploadHost, uploadDictionary, uploadImportDecision } = await module("linked-import.js");
  const { allowLinkedImportRequest } = await module("sharing-protocol.js");
  const { readDictionaryArchiveIdentity } = await module("dictionary-import-archive.js");
  const held = new Map();
  const uploadHost = createUploadHost({
    store: {
      append: (token, data) => held.set(token, [...(held.get(token) ?? []), Buffer.from(data, "base64")]),
      discard: (token) => held.delete(token),
    },
    importUpload: async (token, { fileName, replace }) => {
      if (!held.has(token)) throw new Error("The dictionary upload is no longer held on the host. Import it again.");
      const bytes = new Uint8Array(Buffer.concat(held.get(token)));
      const identity = await readDictionaryArchiveIdentity(new Blob([bytes]));
      const dictionaries = (await extension.storage.api().local.get("dictionaryState")).dictionaryState?.dictionaries ?? [];
      return extension.engine({ target: "hoshidicts-offscreen", type: "hd_import", requestId: "uploaded-import",
        blobUrl: extension.createObjectURL(bytes), fileName, importDecision: uploadImportDecision(identity, dictionaries, replace) });
    },
  });
  // answerUploadRequest, with dispatchSharedRequest's workerReply and failureReply.
  const workerReply = (message, { ok = true, error = null, ...payload }) =>
    ({ type: `${message.type}_result`, requestId: message.requestId ?? null, ok, error, ...payload });
  const owner = "remote:oracle";
  const answerUpload = async (message) => {
    try {
      const allowed = allowLinkedImportRequest(message);
      switch (allowed.type) {
        case "hd_import_begin": return workerReply(allowed, uploadHost.begin(allowed, owner));
        case "hd_import_chunk": return workerReply(allowed, await uploadHost.chunk(allowed, owner));
        case "hd_import_abort": return workerReply(allowed, uploadHost.abort(allowed, owner));
        default: return { ...await uploadHost.commit(allowed, owner), type: "hd_import_commit_result", requestId: allowed.requestId };
      }
    } catch (error) {
      return { type: `${message?.type ?? "hd_unknown"}_result`, requestId: message?.requestId ?? null, ok: false,
        error: error.message, generation: 0 };
    }
  };

  let counter = 0;
  const viaRelay = async (message) => {
    counter += 1;
    linked.send(JSON.stringify({ kind: "request", id: `r${counter}`, message }));
    for (;;) {
      const reply = await untilKind(linked, "reply");
      if (reply.id === `r${counter}`) return reply.response;
    }
  };
  const sides = [
    {
      comparable: comparer(),
      send: (message) => (message.target === TARGET ? answerUpload(message) : extension.engine({ relayed: true, ...message })),
      stored: () => extension.storage.api().local.get(STORED),
    },
    {
      comparable: comparer(),
      send: viaRelay,
      stored: () => JSON.parse(readFileSync(join(dir, "storage.json"), "utf8")),
    },
  ];
  const stored = async (side) => {
    const values = await side.stored();
    return Object.fromEntries(STORED.map((key) => [key, values[key] ?? null]));
  };
  // Errors the two sides word in their own way (a ZIP library's) compare by presence.
  const unworded = (value) => JSON.parse(JSON.stringify(value, (key, field) => (key === "error" && field ? "error" : field)));

  // One step: `run(send)` on each side, whose results and storage must agree.
  const step = async (label, run, loose = false) => {
    const results = [];
    for (const side of sides) {
      let n = 0;
      const send = (message) => side.send({ requestId: `${label}-${n++}`, ...message });
      const result = await run(send).catch((error) => ({ thrown: error.message }));
      results.push({ result: side.comparable(loose ? unworded(result) : result), stored: side.comparable(await stored(side)) });
    }
    assert.deepEqual(results[1].result, results[0].result, `${label}: reply`);
    assert.deepEqual(results[1].stored, results[0].stored, `${label}: storage`);
    return results[0].result;
  };
  const upload = (label, bytes, fileName, replace = false) =>
    step(label, (send) => uploadDictionary({ blob: new Blob([bytes]), fileName, replace,
      send: (type, fields) => send({ target: TARGET, type, ...fields }) }));
  const found = async (label, text) => {
    const reply = await step(`${label}: ${text}`, (send) => send({ target: "hoshidicts-offscreen", type: "hd_lookup", text }));
    assert.equal(reply.ok, true, `${label}: ${reply.error}`);
    return reply.results.length;
  };
  const titles = async () => (await stored(sides[1])).dictionaryState.dictionaries.map((entry) => entry.title);

  // Installed, kept beside a second copy, replaced, and a mirrored fixture replaced.
  const whale = dictionary({ title: "Oracle Upload" }, "鯨");
  let reply = await upload("new", whale, "oracle.zip");
  assert.equal(reply.ok, true, reply.error ?? reply.thrown);
  assert.equal(reply.report.title, "Oracle Upload");
  assert.ok(await found("new", "鯨") > 0);
  reply = await upload("separate", dictionary({ title: "Oracle Upload" }, "鮫"), "oracle.zip");
  assert.equal(reply.report.title, "Oracle Upload (2)", reply.error ?? reply.thrown);
  assert.ok(await found("separate", "鮫") > 0);
  reply = await upload("replace", dictionary({ title: "Oracle Upload", revision: "2" }, "鰐"), "oracle.zip", true);
  assert.equal(reply.ok, true, reply.error ?? reply.thrown);
  assert.equal(await found("replaced", "鯨"), 0);
  assert.ok(await found("replaced", "鰐") > 0);
  reply = await upload("fixture replaced", readFileSync(join(HACHIDORI, "test/fixtures/hachidori-fixture.zip")), "fixture.zip", true);
  assert.equal(reply.ok, true, reply.error ?? reply.thrown);
  assert.deepEqual((await titles()).filter((title) => title.startsWith("Oracle")), ["Oracle Upload", "Oracle Upload (2)"]);

  // Archives that do not import.
  for (const [label, bytes] of [
    ["reserved title", dictionary({ title: "Hachidori Custom Dictionary" }, "犬")],
    ["broken bank", dictionary({ title: "Broken" }, "", "not json")],
    ["no index", zip({ "term_bank_1.json": "[]" })],
    ["no title", zip({ "index.json": JSON.stringify({ format: 3, revision: "1" }) })],
  ]) {
    reply = await upload(label, bytes, `${label}.zip`);
    assert.equal(reply.ok ?? false, false, label);
  }
  // Uploads that are refused, and the protocol's own errors.
  for (const [label, fileName] of [["mdx", "a.mdx"], ["path", "a/b.zip"], ["dot", ".."]]) {
    reply = await upload(label, whale, fileName);
    assert.ok(reply.thrown, label);
  }
  await step("empty", (send) => send({ target: TARGET, type: "hd_import_begin", fileName: "a.zip", size: 0, replace: false }));
  for (const message of [
    { type: "hd_import_begin", fileName: "a.zip", size: 3 },
    { type: "hd_import_begin", fileName: "a.zip", size: 1.5, replace: false },
    { type: "hd_import_begin", fileName: "x".repeat(256), size: 3, replace: false },
    { type: "hd_import_chunk", token: "a/b", offset: 0, data: "" },
    { type: "hd_import_chunk", token: "a", offset: "0", data: "" },
    { type: "hd_import_commit" },
    { type: "hd_import_commit", token: "no-such-upload" },
    { type: "hd_import_abort", token: "no-such-upload" },
    { type: "hd_import_other", token: "a" },
  ]) {
    await step(`malformed ${JSON.stringify(message)}`, (send) => send({ target: TARGET, ...message }));
  }
  const begun = async (send, size = 3) => (await send({ target: TARGET, type: "hd_import_begin", fileName: "a.zip", size, replace: false })).token;
  const chunked = async (label, chunks, finish = "hd_import_commit", loose = false) => step(label, async (send) => {
    const token = await begun(send);
    const replies = [];
    for (const [offset, data] of chunks) replies.push(await send({ target: TARGET, type: "hd_import_chunk", token, offset, data }));
    replies.push(await send({ target: TARGET, type: finish, token }));
    replies.push(await send({ target: TARGET, type: "hd_import_chunk", token, offset: 0, data: "YQ==" }));
    return replies;
  }, loose);
  await chunked("out of order", [[1, "YQ=="]]);
  await chunked("not base64", [[0, "Y"]]);
  await chunked("too long", [[0, "YWJjZA=="]]);
  await chunked("short", [[0, "YQ=="]]);
  await chunked("aborted", [[0, "YQ=="]], "hd_import_abort");
  const [, notZip] = await chunked("not a zip", [[0, "YWJj"]], "hd_import_commit", true);
  assert.equal(notZip.ok, false);

  // What was imported downloads as the extension's archive of it.
  const archiveOf = async (bytes) => {
    const opened = await extension.openBackupArchive(new Blob([bytes]));
    // Each side's engine stamps index.json with the time it imported the archive.
    const contents = async (file) => {
      const bytes = Buffer.from(await file.data.arrayBuffer());
      return file.path.endsWith("/index.json") ? { ...JSON.parse(bytes), importDate: "time" } : bytes.toString("base64");
    };
    const files = await Promise.all(opened.files.map(async (file) => [file.path, await contents(file)]));
    return { snapshot: opened.snapshot, files };
  };
  const extensionState = (await sides[0].stored()).dictionaryState;
  for (const entry of (await stored(sides[1])).dictionaryState.dictionaries.filter((entry) => entry.title.startsWith("Oracle"))) {
    const response = await realFetch(`http://127.0.0.1:${relay.apiPort}/dictionaries/${encodeURIComponent(entry.id)}`);
    assert.equal(response.status, 200, entry.title);
    const served = await archiveOf(await response.arrayBuffer());
    await extension.assertBackupSnapshot(served.snapshot);
    const { id } = extensionState.dictionaries.find((other) => other.title === entry.title);
    const { token, size } = await extension.engine({ type: "hd_api_dictionary_open", id });
    const read = await extension.engine({ type: "hd_api_dictionary_read", token, offset: 0, length: size });
    await extension.engine({ type: "hd_api_dictionary_close", token });
    assert.deepEqual(sides[1].comparable(served), sides[0].comparable(await archiveOf(Buffer.from(read.data, "base64"))), entry.title);
  }
});
