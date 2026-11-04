// Drives the Yomitan-compatible HTTP API of the relay, run as a
// process, with real HTTP requests and a fake sharing Hachidori on /host.
// SPDX-License-Identifier: GPL-3.0-or-later
import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import { createServer } from "node:net";
import test from "node:test";
import { RELAY_VERSION, startAnkiRelayServer } from "./anki-relay-server.mjs";
import { CAPABILITY, FIXTURES, startFakeHost } from "./fake-host.mjs";
import { connectClient, untilKind } from "./relay-socket.mjs";

const sleep = (ms) => new Promise((resolveSleep) => setTimeout(resolveSleep, ms));

async function server(t, options = {}) {
  const started = await startAnkiRelayServer({ pingMs: 50, apiPort: 0, ...options });
  t.after(() => started.close());
  assert.ok(started.apiPort > 0, `the API must announce its port, got ${started.apiError}`);
  return started;
}

// Every request opens its own TCP connection: a kept-alive one would outlive a
// network rebind, which is exactly what the network cases must not confuse with
// a listener that still accepts.
function api(port, host = "127.0.0.1") {
  const base = `http://${host}:${port}`;
  return {
    base,
    post(path, body, init = {}) {
      const headers = { Connection: "close", ...(body === undefined ? {} : { "Content-Type": "application/json" }) };
      return fetch(base + path, { method: "POST", headers, ...(body === undefined ? {} : { body: typeof body === "string" ? body : JSON.stringify(body) }), ...init });
    },
    get(path, init = {}) { return fetch(base + path, { headers: { Connection: "close" }, ...init }); },
  };
}

async function expectJson(response, status) {
  assert.equal(response.status, status, `expected ${status}, got ${response.status}: ${await response.clone().text()}`);
  assert.match(response.headers.get("content-type"), /^application\/json/u);
  assert.equal(response.headers.get("access-control-allow-origin"), "*");
  return response.json();
}

// Task 2: the listener and /serverVersion need no host.

test("serverVersion, method and path errors, malformed JSON and CORS preflight need no host", async (t) => {
  const { apiPort } = await server(t);
  const http = api(apiPort);
  assert.deepEqual(await expectJson(await http.post("/serverVersion"), 200), { version: RELAY_VERSION });
  assert.deepEqual(await expectJson(await http.post("/serverVersion", ""), 200), { version: RELAY_VERSION });
  const wrongMethod = await expectJson(await http.get("/serverVersion"), 405);
  assert.match(wrongMethod.error, /POST/u);
  assert.deepEqual(await expectJson(await http.post("/nope"), 404), { error: "unknown path" });
  assert.deepEqual(await expectJson(await http.get("/nope"), 404), { error: "unknown path" });
  assert.deepEqual(await expectJson(await http.post("/termEntries", "{not json"), 400), { error: "malformed JSON body" });
  assert.deepEqual(await expectJson(await http.post("/termEntries", "[1, 2]"), 400), { error: "expected a JSON object body" });
  for (const method of ["PUT", "DELETE", "PATCH"]) {
    const known = await fetch(`${http.base}/serverVersion`, { method, headers: { Connection: "close" } });
    assert.equal(known.status, 405, `${method} on a known path`);
    assert.match(known.headers.get("content-type"), /^application\/json/u);
    assert.match((await known.json()).error, /POST only/u);
    const unknown = await fetch(`${http.base}/nowhere`, { method, headers: { Connection: "close" } });
    assert.equal(unknown.status, 404, `${method} on an unknown path`);
    assert.deepEqual(await unknown.json(), { error: "unknown path" });
  }
  const head = await fetch(`${http.base}/dictionaries`, { method: "HEAD", headers: { Connection: "close" } });
  assert.equal(head.status, 405);
  assert.equal(await head.text(), "", "HEAD carries no body");
  const preflight = await fetch(`${http.base}/termEntries`, { method: "OPTIONS", headers: { Origin: "https://tool.example", "Access-Control-Request-Method": "POST", "Access-Control-Request-Headers": "content-type" } });
  assert.equal(preflight.status, 204);
  assert.equal(preflight.headers.get("access-control-allow-origin"), "*");
  assert.match(preflight.headers.get("access-control-allow-headers"), /Content-Type/u);
  assert.match(preflight.headers.get("access-control-allow-methods"), /POST/u);
  assert.equal(preflight.headers.get("cache-control"), "no-store");
});

test("the WebSocket relay behaves as before while the API is enabled", async (t) => {
  const { port, apiPort } = await server(t);
  assert.notEqual(port, apiPort);
  assert.equal((await connectClient(t, port, "/link")).status, 503);
  const host = await connectClient(t, port, "/host");
  assert.deepEqual(await host.json(), { kind: "listening", port });
  const link = await connectClient(t, port, "/link");
  assert.equal(link.status, 101);
  const opened = await untilKind(host, "client-open");
  assert.equal(opened.origin, "chrome-extension://hachidorirelaytestextensionid");
  link.send("hello relay");
  assert.equal((await untilKind(host, "client-text")).text, "hello relay");
  // The API port is not a WebSocket endpoint and the relay port is not HTTP JSON.
  assert.equal((await connectClient(t, apiPort, "/host")).status, 404);
});

// Task 3: the synthetic client session and /yomitanVersion.

test("yomitanVersion is 503 without a host, 501 without the capability, and 200 through the fake host", async (t) => {
  const { port, apiPort } = await server(t);
  const http = api(apiPort);
  assert.deepEqual(await expectJson(await http.post("/yomitanVersion"), 503), { error: "No sharing Hachidori is connected to the relay." });
  const old = await startFakeHost(t, port, { capabilities: [] });
  const unsupported = await expectJson(await http.post("/yomitanVersion"), 501);
  assert.match(unsupported.error, /update Hachidori/u);
  assert.equal(old.opens.length, 1);
  assert.equal(old.opens[0].origin, "relay://yomitan-api");
  assert.equal(old.opens[0].address, "127.0.0.1");
  // The unsupported host keeps its session; the answer does not change until the host does.
  await expectJson(await http.post("/yomitanVersion"), 501);
  assert.equal(old.opens.length, 1);
  old.destroy();
  await old.closed;
  await sleep(100);
  const host = await startFakeHost(t, port);
  const body = await expectJson(await http.post("/yomitanVersion"), 200);
  assert.deepEqual(body, { version: FIXTURES.hostHello.version });
  assert.equal(host.opens.length, 1, "exactly one client-open for the API session");
  assert.equal(host.opens[0].origin, "relay://yomitan-api");
  assert.deepEqual(host.hellos[0].capabilities, [CAPABILITY]);
  assert.equal(host.hellos[0].protocol, 1);
  assert.equal(host.hellos[0].version, `hachidori-relay/${RELAY_VERSION}`);
  assert.deepEqual(host.requests[0].message, { target: "hoshidicts-offscreen", type: "hd_api_version" });
  // A second request reuses the session.
  await expectJson(await http.post("/yomitanVersion"), 200);
  assert.equal(host.opens.length, 1);
  assert.equal(host.requests.length, 2);
  assert.notEqual(host.requests[0].id, host.requests[1].id);
  // The relay pings the session as it pings any client, and the session answers with pong toward the host.
  await host.until((current) => current.pongs > 0, "a pong from the API session");
});

test("a host that disconnects mid-request fails it with 503, and the next host gets a fresh session", async (t) => {
  const { port, apiPort } = await server(t);
  const http = api(apiPort);
  const host = await startFakeHost(t, port, { respond: (message) => (message.type === "hd_api_version" ? null : undefined) });
  const pending = http.post("/yomitanVersion");
  await host.until((current) => current.requests.length === 1, "the request to reach the host");
  host.destroy();
  const failed = await expectJson(await pending, 503);
  assert.match(failed.error, /disconnected/u);
  await host.closed;
  assert.deepEqual(await expectJson(await http.post("/yomitanVersion"), 503), { error: "No sharing Hachidori is connected to the relay." });
  const next = await startFakeHost(t, port, { version: "1.2.3" });
  assert.deepEqual(await expectJson(await http.post("/yomitanVersion"), 200), { version: "1.2.3" });
  assert.equal(next.opens.length, 1);
  assert.equal(next.opens[0].origin, "relay://yomitan-api");
});

// Task 4: lookup endpoints.

test("lookup endpoints forward to hd_api_* and answer in Yomitan's shapes", async (t) => {
  const { port, apiPort } = await server(t);
  const http = api(apiPort);
  const host = await startFakeHost(t, port);

  const term = await expectJson(await http.post("/termEntries", { term: "分かる" }), 200);
  assert.equal(term.index, 0);
  assert.equal(term.originalTextLength, 3);
  assert.equal(term.dictionaryEntries[0].headwords[0].term, "分かる");
  assert.deepEqual(host.requests.at(-1).message, { target: "hoshidicts-offscreen", type: "hd_api_term_entries", terms: ["分かる"] });
  const terms = await expectJson(await http.post("/termEntries", { term: ["猫", "犬"] }), 200);
  assert.deepEqual(terms.map((entry) => entry.index), [0, 1]);
  assert.equal(terms[1].dictionaryEntries[0].headwords[0].term, "犬");
  assert.deepEqual(await expectJson(await http.post("/termEntries", { term: 5 }), 400), { error: 'expected "term" to be a string or a string array' });
  assert.deepEqual(await expectJson(await http.post("/termEntries", {}), 400), { error: 'expected "term" to be a string or a string array' });

  const kanji = await expectJson(await http.post("/kanjiEntries", { character: "分" }), 200);
  assert.ok(Array.isArray(kanji), "Yomitan returns the bare entry list for a string input");
  assert.equal(kanji[0].character, "分");
  assert.deepEqual(host.requests.at(-1).message, { target: "hoshidicts-offscreen", type: "hd_api_kanji_entries", characters: ["分"] });
  const kanjis = await expectJson(await http.post("/kanjiEntries", { character: ["日", "本"] }), 200);
  assert.deepEqual(kanjis.map((entry) => [entry.index, entry.dictionaryEntries[0].character]), [[0, "日"], [1, "本"]]);
  await expectJson(await http.post("/kanjiEntries", { character: ["日", 1] }), 400);

  // Exactly what backfill-anki-yomitan sends.
  const backfillBody = { text: "分かる", type: "term", markers: ["Expression", "Glossary", "Reading"], maxEntries: 3, includeMedia: true };
  const fields = await expectJson(await http.post("/ankiFields", backfillBody), 200);
  assert.deepEqual(Object.keys(fields), ["fields", "dictionaryMedia", "audioMedia"]);
  assert.deepEqual(fields.fields[0], { Expression: "Expression:分かる", Glossary: "Glossary:分かる", Reading: "Reading:分かる" });
  const { type: entryType, ...rest } = backfillBody;
  assert.deepEqual(host.requests.at(-1).message, { target: "hoshidicts-offscreen", type: "hd_api_anki_fields", entryType, ...rest });
  const withAudio = await expectJson(await http.post("/ankiFields", { ...backfillBody, markers: ["audio"] }), 200);
  assert.equal(withAudio.audioMedia[0].ankiFilename, "hachidori_audio_1.mp3");
  assert.equal(Buffer.from(withAudio.audioMedia[0].content, "base64").toString(), "fake audio");
  await expectJson(await http.post("/ankiFields", { text: "x", type: "sentence", markers: [] }), 400);
  await expectJson(await http.post("/ankiFields", { text: "x", type: "term", markers: "expression" }), 400);
  await expectJson(await http.post("/ankiFields", { text: "x", type: "term", markers: [], maxEntries: "3" }), 400);
  const defaults = await expectJson(await http.post("/ankiFields", { text: "x", type: "kanji", markers: ["character"] }), 200);
  assert.equal(defaults.fields.length, 2);
  assert.deepEqual(host.requests.at(-1).message, { target: "hoshidicts-offscreen", type: "hd_api_anki_fields", text: "x", entryType: "kanji", markers: ["character"], maxEntries: 0, includeMedia: false });

  // GSM Companion asks for profile 0. Yomitan reads anything but a number as the active profile.
  const formats = FIXTURES.hd_api_anki_card_formats.reply.cardFormats;
  assert.deepEqual(await expectJson(await http.post("/ankiCardFormats"), 200), formats, "Yomitan answers the bare array");
  assert.deepEqual(host.requests.at(-1).message, { target: "hoshidicts-offscreen", type: "hd_api_anki_card_formats" });
  assert.deepEqual(await expectJson(await http.post("/ankiCardFormats", { profileIndex: 0 }), 200), formats);
  assert.deepEqual(host.requests.at(-1).message, { target: "hoshidicts-offscreen", type: "hd_api_anki_card_formats", profileIndex: 0 });
  for (const profileIndex of ["0", true, null]) {
    assert.deepEqual(await expectJson(await http.post("/ankiCardFormats", { profileIndex }), 200), formats);
    assert.deepEqual(host.requests.at(-1).message, { target: "hoshidicts-offscreen", type: "hd_api_anki_card_formats" },
      `${JSON.stringify(profileIndex)} means the active profile`);
  }
  assert.deepEqual(await expectJson(await http.post("/ankiCardFormats", { profileIndex: 1 }), 500),
    { error: 'Invalid input for ankiCardFormats, expected "profileIndex" to be a valid profile index but got 1' });
  assert.match((await expectJson(await http.get("/ankiCardFormats"), 405)).error, /POST only/u);

  const tokens = await expectJson(await http.post("/tokenize", { text: "大きい", scanLength: 10, parser: "scanning-parser" }), 200);
  assert.ok(Array.isArray(tokens), "tokenize always answers an array");
  assert.deepEqual(tokens[0].content, [[{ text: "大きい", reading: "" }]]);
  assert.equal(tokens[0].index, 0);
  assert.deepEqual(host.requests.at(-1).message, { target: "hoshidicts-offscreen", type: "hd_api_tokenize", texts: ["大きい"], scanLength: 10, parser: "scanning-parser" });
  const manyTokens = await expectJson(await http.post("/tokenize", { text: ["猫", "犬"], scanLength: 5 }), 200);
  assert.deepEqual(manyTokens.map((entry) => entry.index), [0, 1]);
  assert.equal(host.requests.at(-1).message.parser, "scanning-parser", "the parser defaults like Yomitan's");
  await expectJson(await http.post("/tokenize", { text: "x" }), 400);
  await expectJson(await http.post("/tokenize", { text: "x", scanLength: "10" }), 400);
});

test("a host error is 500 with an error body, large replies stream through, and concurrent requests keep their own replies", { timeout: 30000 }, async (t) => {
  const { port, apiPort } = await server(t);
  const http = api(apiPort);
  const big = "大きな".repeat(Math.ceil(10 * 1024 * 1024 / 9));
  const host = await startFakeHost(t, port, {
    respond(message, current, defaults) {
      if (message.type === "hd_api_term_entries" && message.terms[0] === "fail") return FIXTURES.error;
      if (message.type === "hd_api_term_entries" && message.terms[0] === "big") {
        return { results: [{ index: 0, dictionaryEntries: [{ type: "term", glossary: big }], originalTextLength: 3 }] };
      }
      if (message.type === "hd_api_version") return new Promise((resolveLater) => setTimeout(() => resolveLater({ version: "slow" }), 300));
      return undefined;
    },
  });
  assert.deepEqual(await expectJson(await http.post("/termEntries", { term: "fail" }), 500), FIXTURES.error);
  const large = await expectJson(await http.post("/termEntries", { term: "big" }), 200);
  assert.equal(large.dictionaryEntries[0].glossary.length, big.length);
  assert.ok(large.dictionaryEntries[0].glossary === big, "a 10 MiB reply arrives intact");
  const [slow, fast, kanji] = await Promise.all([
    expectJson(await http.post("/yomitanVersion"), 200),
    expectJson(await http.post("/termEntries", { term: ["fast"] }), 200),
    expectJson(await http.post("/kanjiEntries", { character: "山" }), 200),
  ]);
  assert.deepEqual(slow, { version: "slow" });
  assert.equal(fast[0].dictionaryEntries[0].headwords[0].term, "fast");
  assert.equal(kanji[0].character, "山");
  assert.equal(host.opens.length, 1, "concurrent requests share the one session");
});

// Task 5: GET /dictionaries.

test("GET /dictionaries lists the host's catalogue and has the same host-state errors", async (t) => {
  const { port, apiPort } = await server(t);
  const http = api(apiPort);
  assert.equal((await http.get("/dictionaries")).status, 503);
  assert.equal((await http.post("/dictionaries", {})).status, 405);
  const old = await startFakeHost(t, port, { capabilities: ["something-else"] });
  assert.equal((await http.get("/dictionaries")).status, 501);
  old.destroy();
  await old.closed;
  await sleep(100);
  const host = await startFakeHost(t, port, { files: new Map([["jitendex", Buffer.alloc(20 * 1024 * 1024)], ["kanjidic", Buffer.alloc(4096)]]) });
  const listed = await expectJson(await http.get("/dictionaries"), 200);
  assert.deepEqual(listed, FIXTURES.hd_api_dictionaries.reply);
  assert.deepEqual(host.requests.at(-1).message, { target: "hoshidicts-offscreen", type: "hd_api_dictionaries" });
});

// Task 6: GET /dictionaries/<id>.

const sha256 = (buffer) => createHash("sha256").update(buffer).digest("hex");

test("GET /dictionaries/<id> streams the archive in chunks with the right headers and closes the token", { timeout: 60000 }, async (t) => {
  const { port, apiPort } = await server(t);
  const http = api(apiPort);
  const payload = randomBytes(20 * 1024 * 1024);
  const host = await startFakeHost(t, port, { files: new Map([["jitendex", payload], ["kanjidic", Buffer.from("small")]]) });
  const response = await http.get("/dictionaries/jitendex");
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("content-type"), "application/zip");
  assert.equal(response.headers.get("content-length"), String(payload.length));
  assert.equal(response.headers.get("content-disposition"), 'attachment; filename="jitendex.hachidori.zip"');
  const body = Buffer.from(await response.arrayBuffer());
  assert.equal(body.length, payload.length);
  assert.equal(sha256(body), sha256(payload));
  const reads = host.requests.filter((entry) => entry.message.type === "hd_api_dictionary_read");
  assert.equal(reads.length, 5, "20 MiB in 4 MiB chunks");
  assert.deepEqual(reads.map((entry) => entry.message.offset), [0, 4194304, 8388608, 12582912, 16777216]);
  assert.ok(reads.every((entry) => entry.message.length === 4194304));
  await host.until((current) => current.closedTokens.length === 1, "the close request");
  assert.equal(host.closedTokens[0], reads[0].message.token);
  assert.equal(host.openTokens.size, 0);

  const missing = await http.get("/dictionaries/does-not-exist");
  assert.equal(missing.status, 404);
  assert.deepEqual(await missing.json(), { error: "unknown dictionary" });
  assert.equal((await http.get("/dictionaries/")).status, 404);
  assert.equal((await http.post("/dictionaries/jitendex", {})).status, 405);
  const encoded = await http.get(`/dictionaries/${encodeURIComponent("kanjidic")}`);
  assert.equal(encoded.status, 200);
  assert.equal(await encoded.text(), "small");
});

test("downloads without a reported size, with short host chunks, and with non-ASCII names still arrive whole", { timeout: 30000 }, async (t) => {
  const { port, apiPort } = await server(t);
  const http = api(apiPort);
  const payload = randomBytes(3 * 1024 * 1024 + 17);
  const host = await startFakeHost(t, port, {
    reportSize: false, maxChunk: 1024 * 1024,
    dictionaries: [{ id: "jmdict", title: "JMdict", revision: "1", fileName: "辞書.zip" }],
    files: new Map([["jmdict", payload]]),
  });
  const response = await http.get("/dictionaries/jmdict");
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("content-length"), null);
  assert.equal(response.headers.get("content-disposition"), "attachment; filename=\"__.zip\"; filename*=UTF-8''%E8%BE%9E%E6%9B%B8.zip");
  const body = Buffer.from(await response.arrayBuffer());
  assert.equal(sha256(body), sha256(payload));
  assert.equal(host.requests.filter((entry) => entry.message.type === "hd_api_dictionary_read").length, 4);
  await host.until((current) => current.closedTokens.length === 1, "the close request");
});

test("a client that aborts a download still causes close for its token, and two downloads do not interleave", { timeout: 60000 }, async (t) => {
  const { port, apiPort } = await server(t);
  const http = api(apiPort);
  const first = randomBytes(12 * 1024 * 1024);
  const second = randomBytes(9 * 1024 * 1024);
  const host = await startFakeHost(t, port, {
    dictionaries: [{ id: "a", title: "A", revision: "1", fileName: "a.zip" }, { id: "b", title: "B", revision: "1", fileName: "b.zip" }],
    files: new Map([["a", first], ["b", second]]),
  });
  const controller = new AbortController();
  const aborted = http.get("/dictionaries/a", { signal: controller.signal });
  await host.until((current) => current.requests.filter((entry) => entry.message.type === "hd_api_dictionary_read").length >= 1, "the first chunk read");
  const response = await aborted;
  const reader = response.body.getReader();
  await reader.read();
  controller.abort();
  await assert.rejects(reader.read(), /abort/iu);
  await host.until((current) => current.closedTokens.length === 1, "close after the client abort", 15000);
  assert.equal(host.openTokens.size, 0);

  const [one, two] = await Promise.all([http.get("/dictionaries/a"), http.get("/dictionaries/b")]);
  const [bodyOne, bodyTwo] = await Promise.all([one.arrayBuffer(), two.arrayBuffer()]);
  assert.equal(sha256(Buffer.from(bodyOne)), sha256(first));
  assert.equal(sha256(Buffer.from(bodyTwo)), sha256(second));
  await host.until((current) => current.closedTokens.length === 3, "both closes");
});

test("a chunk read the host answers with an error ends the response short of its length, and the token is closed", { timeout: 30000 }, async (t) => {
  const { port, apiPort } = await server(t);
  const http = api(apiPort);
  const payload = randomBytes(6 * 1024 * 1024);
  let reads = 0;
  const host = await startFakeHost(t, port, {
    files: new Map([["kanjidic", payload]]),
    respond(message) {
      if (message.type !== "hd_api_dictionary_read") return undefined;
      reads += 1;
      return reads === 2 ? { error: "read failed" } : undefined;
    },
  });
  const response = await http.get("/dictionaries/kanjidic");
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("content-length"), String(payload.length));
  let received = 0;
  await assert.rejects((async () => {
    for await (const chunk of response.body) received += chunk.length;
  })(), (error) => error instanceof TypeError || /terminated|closed|aborted|ECONNRESET/iu.test(String(error.cause ?? error)));
  assert.ok(received < payload.length, `only the chunk before the error arrives, got ${received}`);
  await host.until((current) => current.closedTokens.length === 1, "the close request");
});

test("a chunk read the host never answers ends the response short of its length, and the token is still closed", { timeout: 30000 }, async (t) => {
  const { port, apiPort } = await server(t, { apiTimeoutSeconds: 1 });
  const http = api(apiPort);
  const payload = randomBytes(6 * 1024 * 1024);
  let reads = 0;
  const host = await startFakeHost(t, port, {
    files: new Map([["kanjidic", payload]]),
    respond(message) {
      if (message.type !== "hd_api_dictionary_read") return undefined;
      reads += 1;
      return reads === 2 ? null : undefined; // never answer the second chunk
    },
  });
  const response = await http.get("/dictionaries/kanjidic");
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("content-length"), String(payload.length));
  let received = 0;
  const started = Date.now();
  await assert.rejects((async () => {
    for await (const chunk of response.body) received += chunk.length;
  })(), (error) => error instanceof TypeError || /terminated|closed|aborted|ECONNRESET/iu.test(String(error.cause ?? error)));
  assert.equal(received, 4 * 1024 * 1024, "exactly the first chunk arrived");
  assert.ok(Date.now() - started < 10000, "the timeout, not the client, ended it");
  await host.until((current) => current.closedTokens.length === 1, "the close request");
  assert.equal(host.openTokens.size, 0);
});

test("a host that opens a dictionary after the relay stopped waiting gets 504 answered and its token closed", { timeout: 30000 }, async (t) => {
  const { port, apiPort } = await server(t, { apiTimeoutSeconds: 0.5 });
  const http = api(apiPort);
  const host = await startFakeHost(t, port, {
    files: new Map([["kanjidic", Buffer.from("late archive")]]),
    respond(message, current, defaults) {
      if (message.type !== "hd_api_dictionary_open") return undefined;
      return new Promise((resolveLater) => setTimeout(() => resolveLater(defaults(message, current)), 1200));
    },
  });
  const late = await expectJson(await http.get("/dictionaries/kanjidic"), 504);
  assert.match(late.error, /did not answer/u);
  await host.until((current) => current.closedTokens.length === 1, "close for the late token", 10000);
  assert.equal(host.closedTokens[0], "dl-1");
  assert.equal(host.openTokens.size, 0, "the lease the host granted too late is released");
  assert.deepEqual(await expectJson(await http.post("/yomitanVersion"), 200), { version: FIXTURES.hostHello.version }, "the session is still usable");
});

test("a host that closes the API client makes the relay forget it, and the next request reconnects", async (t) => {
  const { port, apiPort } = await server(t);
  const http = api(apiPort);
  const host = await startFakeHost(t, port);
  await expectJson(await http.post("/yomitanVersion"), 200);
  const [{ clientId }] = host.opens;
  host.closeClient(clientId);
  await host.until((current) => current.closes.length === 1, "client-close for the API session");
  assert.equal(host.closes[0].clientId, clientId);
  assert.deepEqual(await expectJson(await http.post("/yomitanVersion"), 200), { version: FIXTURES.hostHello.version });
  assert.equal(host.opens.length, 2, "a fresh session after the host closed the previous one");
  assert.notEqual(host.opens[1].clientId, clientId);
});

// Task 7: the network switch.

async function nonLoopbackAddress(t, host) {
  const reply = await host.network(true);
  assert.equal(reply.kind, "network");
  assert.equal(reply.enabled, true);
  assert.equal(reply.error, undefined, `the API must rebind with the relay: ${reply.error}`);
  if (reply.addresses.length === 0) {
    t.diagnostic("no network address on this machine; the network cases did not run");
    return null;
  }
  return reply.addresses[0].address;
}

test("the API follows the host's network switch and returns to this computer when the host leaves", { timeout: 30000 }, async (t) => {
  const { port, apiPort } = await server(t);
  const host = await startFakeHost(t, port);
  const address = await nonLoopbackAddress(t, host);
  if (address === null) return;
  const remote = api(apiPort, address);
  assert.deepEqual(await expectJson(await remote.post("/serverVersion"), 200), { version: RELAY_VERSION });
  assert.deepEqual(await expectJson(await remote.post("/yomitanVersion"), 200), { version: FIXTURES.hostHello.version });
  assert.deepEqual(await host.network(false), { kind: "network", enabled: false, addresses: [] });
  await assert.rejects(remote.post("/serverVersion"), /ECONNREFUSED|fetch failed/u, "network off refuses the LAN address");
  assert.deepEqual(await expectJson(await api(apiPort).post("/serverVersion"), 200), { version: RELAY_VERSION }, "loopback keeps serving");
  assert.equal((await host.network(true)).enabled, true);
  assert.equal((await remote.post("/serverVersion")).status, 200, "network on serves the LAN address again");
  assert.deepEqual(await expectJson(await remote.post("/yomitanVersion"), 200), { version: FIXTURES.hostHello.version }, "the API session survives the rebind");
  assert.equal(host.opens.length, 1);
  assert.deepEqual(await host.network(false), { kind: "network", enabled: false, addresses: [] });
  await assert.rejects(remote.post("/serverVersion"), /ECONNREFUSED|fetch failed/u);
  assert.equal((await host.network(true)).enabled, true);
  assert.equal((await remote.post("/serverVersion")).status, 200);
  host.destroy();
  await host.closed;
  await sleep(300);
  await assert.rejects(remote.post("/serverVersion"), /ECONNREFUSED|fetch failed/u, "host loss returns the API to this computer");
  assert.equal((await api(apiPort).post("/serverVersion")).status, 200);
});

test("a download in flight survives a network rebind", { timeout: 60000 }, async (t) => {
  const { port, apiPort } = await server(t);
  const http = api(apiPort);
  const payload = randomBytes(16 * 1024 * 1024);
  const host = await startFakeHost(t, port, { files: new Map([["kanjidic", payload]]), replyDelayMs: 100 });
  const download = http.get("/dictionaries/kanjidic").then((response) => response.arrayBuffer());
  await host.until((current) => current.requests.some((entry) => entry.message.type === "hd_api_dictionary_read"), "the first read");
  const reply = await host.network(true);
  assert.equal(reply.enabled, true);
  assert.equal(reply.error, undefined);
  await host.until((current) => current.requests.filter((entry) => entry.message.type === "hd_api_dictionary_read").length >= 3, "more reads");
  assert.deepEqual(await host.network(false), { kind: "network", enabled: false, addresses: [] });
  assert.equal(sha256(Buffer.from(await download)), sha256(payload), "the in-flight download completes intact across both rebinds");
});

test("an occupied API port leaves the relay running and is reported", async (t) => {
  const blocker = createServer();
  await new Promise((resolveListen) => blocker.listen(0, "127.0.0.1", resolveListen));
  t.after(() => blocker.close());
  const taken = blocker.address().port;
  const relay = await startAnkiRelayServer({ pingMs: 50, apiPort: taken });
  t.after(() => relay.close());
  assert.equal(relay.apiPort, null);
  assert.match(relay.apiError, /in use|Address/iu);
  const host = await connectClient(t, relay.port, "/host");
  assert.deepEqual(await host.json(), { kind: "listening", port: relay.port });
});
