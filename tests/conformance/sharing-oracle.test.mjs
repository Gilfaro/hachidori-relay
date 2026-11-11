// Host mode's sharing with language profiles against the extension's own
// sharing host (background-sharing.js, docs/languages.md 17.3-17.6): the same
// browsers link to the real Hachidori, through an in-memory relay as
// sharing-profiles.test.mjs upstream links them, and to the relay in host
// mode, which mirrored that extension's dictionaries first. Older browsers,
// profile-aware ones with and without a language, and the host's own records
// gaining and losing languages: every hello, opt-in, reply, change frame and
// forced reconnection must agree.
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
import { awareClient, linkClient, memoryRelay } from "./linked-client.mjs";
import { eventually } from "./relay-socket.mjs";

const STORED = ["dictionaryState", "customDictionarySource", "options", "lookupStats", "wordStatusOverrides"];
const OLDER = { version: "0.3.2", name: "GSM", capabilities: ["linked-anki-v1", "linked-anki-v2"] };
const settle = () => new Promise((done) => setTimeout(done, 150));

// What a link received, as both hosts must send it: the hello and change frames
// with the shared records only (the hosts' names and versions differ by design),
// and no Anki index revisions, which only the relay's own index raises here.
// Each kind keeps its order; a reply and the change frame of its own commit may
// come in either order (the extension's goes out when chrome.storage tells it).
function received(frames) {
  const kept = frames.filter((frame) => frame.kind !== "word-status").map((frame) => {
    if (frame.kind === "hello") {
      const { version, name, snapshot, ...rest } = frame;
      return { ...rest, snapshot: Object.fromEntries(STORED.map((key) => [key, snapshot[key] ?? null])) };
    }
    if (frame.kind === "storage") {
      const changes = Object.fromEntries(Object.entries(frame.changes).filter(([key]) => STORED.includes(key)
        || key.startsWith("lookupStats:")));
      return Object.keys(changes).length === 0 ? null : { kind: "storage", changes };
    }
    return frame;
  }).filter((frame) => frame !== null);
  return Object.groupBy(kept, (frame) => frame.kind);
}

test("the relay shares language profiles as the extension does", { skip, timeout: 300_000 }, async (t) => {
  const memory = memoryRelay();
  const extension = await bootExtension({ WebSocket: memory.WebSocket });
  await eventually("the extension's sharing host to connect", () => assert.ok(memory.connected()));
  const dir = mkdtempSync(join(tmpdir(), "hachidori-relay-sharing-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const relay = await startAnkiRelayServer({ apiPort: 0, extraArgs: ["--host", "--data-dir", dir] });
  t.after(() => relay.close());
  const watcher = await linkClient(t, relay.port);
  await serveAsBrowserHost(t, relay.port, extension);
  await watcher.until("the mirrored dictionaries", (frame) => frame.kind === "storage"
    && frame.changes.dictionaryState?.dictionaries?.length === FIXTURES.length);
  watcher.close();

  const packs = globalThis.HDLanguages.codes.filter((code) => code !== "ja");
  const sides = [
    {
      name: "extension",
      link: (context, options) => memory.link(context, options),
      stored: async () => extension.storage.api().local.get(STORED),
    },
    {
      name: "relay",
      link: (context, options) => linkClient(context, relay.port, options),
      stored: async () => JSON.parse(readFileSync(join(dir, "storage.json"), "utf8")),
    },
  ].map((side) => ({ ...side, comparable: comparer(), links: {}, marks: {} }));

  // One step on each side in turn: `run(side)` returns what must agree, then
  // every link's closed state and what it received since the step began.
  const step = async (label, run) => {
    const results = [];
    for (const side of sides) {
      for (const [name, client] of Object.entries(side.links)) side.marks[name] = client.frames.length;
      const result = await run(side);
      await settle();
      const links = Object.fromEntries(Object.entries(side.links).map(([name, client]) => [name,
        { closed: client.closed, received: received(client.frames.slice(side.marks[name])) }]));
      results.push(side.comparable({ result: result ?? null, links }));
    }
    assert.deepEqual(results[1], results[0], label);
    return results[0].result;
  };
  const link = (side, name, options) => side.link(t, options).then((client) => (side.links[name] = client));
  const aware = (side, name, languages) => awareClient(t, side.link, languages).then((client) => (side.links[name] = client));
  const worker = (client, message) => client.send({ target: "hoshidicts-worker", requestId: message.type, ...message });
  const engine = (client, message) => client.send({ target: "hoshidicts-offscreen", requestId: message.type, ...message });
  const revision = async (side) => (await side.stored()).options?.revision ?? 0;

  // A single-profile host: today's hello, one change frame, today's replies.
  let result = await step("single-profile hello", async (side) => (await link(side, "older", { hello: OLDER })).hello.capabilities);
  assert.equal(result.includes("language-profiles-v1"), false);
  result = await step("no opt-in without the capability", async (side) => (await aware(side, "aware", ["en"])).optIn);
  assert.equal(result, null);
  await step("a write is a change frame", async (side) => {
    const control = await link(side, "control");
    return worker(control, { type: "hd_options_write", baseRevision: await revision(side), options: { scanLength: 14 } });
  });
  result = await step("an older state read", async (side) => Object.keys(await worker(side.links.older, { type: "hd_state_read" })).sort());
  assert.deepEqual(result, ["error", "legacyDictionaries", "ok", "requestId", "state", "type"]);

  // The records gain profiles: every connection closes once its requests are done.
  result = await step("records gain profiles", async (side) => {
    const reply = await engine(side.links.control, { type: "hd_profile_add", language: "en" });
    for (const client of Object.values(side.links)) await client.whenClosed();
    return reply.ok;
  });
  assert.equal(result, true);

  // Each comes back with its projection.
  result = await step("older hello with profiles", async (side) => {
    side.links = {};
    return (await link(side, "older", { hello: OLDER })).hello.snapshot.dictionaryState.schemaVersion;
  });
  assert.equal(result, 1);
  result = await step("aware with every pack", async (side) => {
    await aware(side, "control", packs);
    return (await aware(side, "aware", packs)).optIn;
  });
  assert.equal(result.optIn, "accepted");
  result = await step("aware without English", async (side) => (await aware(side, "poor", [])).optIn);
  assert.deepEqual([result.optIn, result.missingLanguages], ["refused", ["en"]]);

  // Change frames and replies: the records as stored, or their Japanese view.
  await step("an English write", async (side) => worker(side.links.control,
    { type: "hd_options_write", profileId: "en", baseRevision: await revision(side), options: { scanLength: 33 } }));
  result = await step("an older write naming English", async (side) => worker(side.links.older,
    { type: "hd_options_write", profileId: "en", baseRevision: await revision(side), options: { scanLength: 31 } }));
  assert.equal(result.ok, true, result.error);
  assert.equal(Object.hasOwn(result.options, "profiles"), false);
  await step("an older state read with profiles", async (side) => worker(side.links.older, { type: "hd_state_read" }));
  result = await step("an older browser adds no language", async (side) => engine(side.links.older,
    { type: "hd_profile_add", language: "de", copyFrom: "ja" }));
  assert.equal(result.ok, false);
  for (const message of [{ language: "EN" }, {}, { language: "en" }]) {
    await step(`aware adds ${JSON.stringify(message)}`, async (side) => engine(side.links.aware, { type: "hd_profile_add", ...message }));
  }

  // A language a browser lacks closes its link; one it has keeps it.
  result = await step("aware with English only", async (side) => (await aware(side, "english", ["en"])).optIn);
  assert.equal(result.optIn, "accepted");
  result = await step("a language the English browser lacks", async (side) => {
    const reply = await engine(side.links.control, { type: "hd_profile_add", language: "de" });
    await side.links.english.whenClosed();
    return reply.ok;
  });
  assert.equal(result, true);
  await step("the English browser comes back refused", async (side) => (await aware(side, "english", ["en"])).optIn);

  // Removing the languages a refused browser lacked makes it link again.
  await step("German removed", async (side) => engine(side.links.control, { type: "hd_profile_remove", profileId: "de" }));
  result = await step("English removed", async (side) => {
    const reply = await engine(side.links.control, { type: "hd_profile_remove", profileId: "en" });
    await side.links.poor.whenClosed();
    await side.links.english.whenClosed();
    return reply.ok;
  });
  assert.equal(result, true);
  // Japanese alone is the single-profile form again: no profiles are offered.
  result = await step("the refused browser comes back", async (side) => (await aware(side, "poor", [])).hello.capabilities);
  assert.equal(result.includes("language-profiles-v1"), false);
});
