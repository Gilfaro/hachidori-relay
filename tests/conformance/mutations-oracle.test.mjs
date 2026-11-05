// Host mode's dictionary mutations against the extension itself: the same
// hd_apply_state, hd_remove, hd_reload, hd_custom_save and hd_custom_append steps
// go to the real Hachidori engine service (extension-host.mjs) and, through a
// linked browser, to the relay, which mirrored that extension's dictionaries
// first. Every reply, the stored dictionary state and custom source after every
// step, and lookups over the result must agree.
//
// Not from upstream. Needs what host-oracle.test.mjs needs; skipped otherwise.
// SPDX-License-Identifier: GPL-3.0-or-later
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { startAnkiRelayServer } from "./anki-relay-server.mjs";
import { FIXTURES, bootExtension, comparer, serveAsBrowserHost, skip } from "./extension-host.mjs";
import { connectClient, untilKind } from "./relay-socket.mjs";

const CUSTOM_ID = "e4c2e20a1a964b6cbd4ae3f87643c1f0";
const STORED = ["dictionaryState", "customDictionarySource", "options"];

test("the relay changes dictionaries as the extension's own engine does", { skip, timeout: 180_000 }, async (t) => {
  const extension = await bootExtension();
  const dir = mkdtempSync(join(tmpdir(), "hachidori-relay-mutations-"));
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

  let counter = 0;
  const relaySide = {
    name: "relay",
    comparable: comparer(),
    async send(message) {
      counter += 1;
      linked.send(JSON.stringify({ kind: "request", id: `r${counter}`, message: { target: "hoshidicts-offscreen", ...message } }));
      for (;;) {
        const reply = await untilKind(linked, "reply");
        if (reply.id === `r${counter}`) return reply.response;
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
    send: (message) => extension.engine({ target: "hoshidicts-offscreen", relayed: true, ...message }),
    async stored() {
      const storage = await extension.storage.api().local.get(STORED);
      return Object.fromEntries(STORED.map((key) => [key, storage[key] ?? null]));
    },
  };
  const sides = [extensionSide, relaySide];

  // One step: a message per side (built from that side's own state when it names
  // its packages), whose replies and resulting storage must agree.
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
  const lookups = async (label) => {
    const found = {};
    for (const text of ["猫", "鳥", "食べる", "漢字の読み方"]) {
      const reply = await step(`${label}: ${text}`, { type: "hd_lookup", text });
      assert.equal(reply.ok, true, `${label}: ${text}: ${reply.error}`);
      found[text] = reply.results.length;
    }
    await step(`${label}: reload`, { type: "hd_reload" });
    return found;
  };
  const state = (stored) => stored.dictionaryState;
  const documentRevision = (stored) => stored.customDictionarySource?.revision ?? 0;

  assert.deepEqual(await lookups("mirrored"), { 猫: 0, 鳥: 0, 食べる: 2, 漢字の読み方: 1 });

  // The custom dictionary: built, stale, source-only, appended, refused, with line errors.
  let reply = await step("custom created", { type: "hd_custom_save", baseDocumentRevision: 0, text: "猫, ねこ, cat\n犬, いぬ, dog\n" });
  assert.equal(reply.ok, true, reply.error);
  assert.equal(reply.rebuilt, true);
  assert.equal(reply.state.dictionaries[0].id, CUSTOM_ID);
  assert.ok((await lookups("custom created"))["猫"] > 0);
  reply = await step("custom stale", { type: "hd_custom_save", baseDocumentRevision: 0, text: "x, y, z" });
  assert.equal(reply.stale, true);
  reply = await step("custom comment", (stored) => ({ type: "hd_custom_save", baseDocumentRevision: documentRevision(stored),
    text: "# mine\n猫, ねこ, cat\n犬, いぬ, dog\n" }));
  assert.equal(reply.ok, true, reply.error);
  assert.equal(reply.rebuilt, false);
  reply = await step("custom append", { type: "hd_custom_append", entry: { term: " 鳥 ", reading: "とり", definition: "bird\r\nsecond \\ line" } });
  assert.equal(reply.rebuilt, true, reply.error);
  assert.ok((await lookups("custom appended"))["鳥"] > 0);
  for (const entry of [{ term: "#x", reading: "x", definition: "x" }, { term: "a,b", reading: "x", definition: "x" }, null, [], "x"]) {
    reply = await step(`custom append ${JSON.stringify(entry)}`, { type: "hd_custom_append", entry });
    assert.equal(reply.ok, false);
  }
  reply = await step("custom line errors", (stored) => ({ type: "hd_custom_save", baseDocumentRevision: documentRevision(stored),
    text: "猫, ねこ, cat\r\nbroken line\r\n, x, y\r\n鳥, とり, bird\r\n" }));
  assert.equal(reply.errors.length, 2);
  for (const message of [{ baseDocumentRevision: -1, text: "" }, { baseDocumentRevision: 1.5, text: "" }, { baseDocumentRevision: 0 }]) {
    reply = await step(`custom invalid ${JSON.stringify(message)}`, { type: "hd_custom_save", ...message });
    assert.equal(reply.ok, false);
  }

  // The dictionary list: reordered, disabled, stale, invalid.
  const others = (stored) => state(stored).dictionaries.slice(1);
  reply = await step("reorder", (stored) => ({ type: "hd_apply_state", baseRevision: state(stored).revision,
    dictionaries: [state(stored).dictionaries[0], ...others(stored).reverse()] }));
  assert.equal(reply.ok, true, reply.error);
  await lookups("reordered");
  reply = await step("disable", (stored) => ({ type: "hd_apply_state", baseRevision: state(stored).revision,
    dictionaries: state(stored).dictionaries.map((entry, index) => (index === 1 ? { ...entry, enabled: false } : entry)) }));
  assert.equal(reply.ok, true, reply.error);
  await lookups("disabled");
  reply = await step("stale state", (stored) => ({ type: "hd_apply_state", baseRevision: state(stored).revision - 1,
    dictionaries: state(stored).dictionaries }));
  assert.equal(reply.conflict, true);
  reply = await step("custom moved", (stored) => ({ type: "hd_apply_state", baseRevision: state(stored).revision,
    dictionaries: [...others(stored), state(stored).dictionaries[0]] }));
  assert.equal(reply.ok, false);
  for (const [label, build] of [
    ["no revision", (stored) => ({ dictionaries: state(stored).dictionaries })],
    ["negative revision", (stored) => ({ baseRevision: -1, dictionaries: state(stored).dictionaries })],
    ["no list", (stored) => ({ baseRevision: state(stored).revision })],
    ["bad path", (stored) => ({ baseRevision: state(stored).revision,
      dictionaries: [...state(stored).dictionaries, { ...others(stored)[0], id: "other", path: "/elsewhere/x" }] })],
    ["not installed", (stored) => ({ baseRevision: state(stored).revision, dictionaries: [...state(stored).dictionaries,
      { ...others(stored)[0], id: "other", path: "/dicts/.hdw-generation-00000000-0000-4000-8000-000000000000/" + others(stored)[0].title }] })],
  ]) {
    reply = await step(`apply ${label}`, (stored) => ({ type: "hd_apply_state", ...build(stored) }));
    assert.equal(reply.ok, false, label);
  }
  await lookups("after refusals");

  // Removal: refused, absent, then one fixture.
  for (const [label, build] of [
    ["unusable title", { title: "" }],
    ["reserved title", { title: ".hdw-import" }],
    ["mismatched id", (stored) => ({ title: "nope", id: others(stored)[0].id })],
    ["custom", { title: "Hachidori Custom Dictionary" }],
    ["absent", { title: "no such dictionary" }],
    ["fixture", (stored) => ({ title: others(stored).at(-1).title, id: others(stored).at(-1).id })],
  ]) {
    await step(`remove ${label}`, (stored) => ({ type: "hd_remove", ...(typeof build === "function" ? build(stored) : build) }));
  }
  assert.equal(state(await relaySide.stored()).dictionaries.length, FIXTURES.length);
  await lookups("removed");

  // An empty source removes the custom package; saving it again is a no-op.
  reply = await step("custom emptied", (stored) => ({ type: "hd_custom_save", baseDocumentRevision: documentRevision(stored), text: "# none\n" }));
  assert.equal(reply.removed, true, reply.error);
  reply = await step("custom empty again", (stored) => ({ type: "hd_custom_save", baseDocumentRevision: documentRevision(stored), text: "" }));
  assert.equal(reply.removed, false, reply.error);
  assert.equal((await lookups("custom removed"))["猫"], 0);
  assert.equal(state(await relaySide.stored()).dictionaries.length, FIXTURES.length - 1);
});
