// A sharing Hachidori for the API tests: connects to /host, tracks clients and
// answers hello and every hd_api_* request from docs/host-contract.md with the
// canned data in fixtures/host-contract.json. Knobs cover a host without the
// capability, delayed or failing replies, and large payloads.
// SPDX-License-Identifier: GPL-3.0-or-later
import { readFileSync } from "node:fs";
import { EXTENSION_ORIGIN, connectClient, queue } from "./relay-socket.mjs";

export const FIXTURES = JSON.parse(readFileSync(new URL("./fixtures/host-contract.json", import.meta.url), "utf8"));
export const CAPABILITY = "hoshidicts-api-v1";

const sleep = (ms) => new Promise((resolveSleep) => setTimeout(resolveSleep, ms));

// Default canned answers, in the shapes docs/host-contract.md gives.
export function defaultRespond(message, host) {
  const { type } = message;
  switch (type) {
    case "hd_api_version":
      return { version: host.version };
    case "hd_api_term_entries":
      return {
        results: message.terms.map((term, index) => ({
          index,
          dictionaryEntries: [{ type: "term", isPrimary: true, headwords: [{ term, reading: "" }], definitions: [] }],
          originalTextLength: term.length,
        })),
      };
    case "hd_api_kanji_entries":
      return {
        results: message.characters.map((character, index) => ({
          index,
          dictionaryEntries: [{ type: "kanji", character, dictionary: "KANJIDIC" }],
        })),
      };
    case "hd_api_anki_fields": {
      const fields = Object.fromEntries(message.markers.map((marker) => [marker, `${marker}:${message.text}`]));
      const count = message.maxEntries > 0 ? Math.min(message.maxEntries, 2) : 2;
      return {
        fields: Array.from({ length: count }, () => ({ ...fields })),
        dictionaryMedia: [],
        audioMedia: message.includeMedia && message.markers.includes("audio")
          ? [{ term: message.text, reading: "", mediaType: "audio/mpeg", content: Buffer.from("fake audio").toString("base64"), ankiFilename: "hachidori_audio_1.mp3" }]
          : [],
      };
    }
    case "hd_api_anki_card_formats":
      // One set of card formats, as Hachidori has: profile 0.
      if (message.profileIndex !== undefined && message.profileIndex !== 0) {
        return { error: `Invalid input for ankiCardFormats, expected "profileIndex" to be a valid profile index but got ${message.profileIndex}` };
      }
      return FIXTURES.hd_api_anki_card_formats.reply;
    case "hd_api_tokenize":
      return {
        results: message.texts.map((text, index) => ({
          id: "scan", source: message.parser ?? "scanning-parser", dictionary: null, index,
          content: [[{ text, reading: "" }]],
        })),
      };
    case "hd_api_dictionaries":
      return { dictionaries: host.dictionaries.map(({ id, title, revision, fileName }) => ({ id, title, revision, size: host.files.get(id)?.length ?? 0, fileName })) };
    case "hd_api_dictionary_open": {
      const entry = host.dictionaries.find((dictionary) => dictionary.id === message.id);
      const bytes = host.files.get(message.id);
      if (!entry || !bytes) return { error: "unknown dictionary", notFound: true };
      host.tokenCounter += 1;
      const token = `dl-${host.tokenCounter}`;
      host.openTokens.set(token, message.id);
      return { token, size: host.reportSize ? bytes.length : undefined, fileName: entry.fileName };
    }
    case "hd_api_dictionary_read": {
      const id = host.openTokens.get(message.token);
      if (id === undefined) return { error: "unknown token" };
      const bytes = host.files.get(id);
      const length = Math.min(message.length, host.maxChunk ?? message.length);
      const slice = bytes.subarray(message.offset, message.offset + length);
      return { data: slice.toString("base64"), eof: message.offset + slice.length >= bytes.length };
    }
    case "hd_api_dictionary_close":
      host.openTokens.delete(message.token);
      host.closedTokens.push(message.token);
      return {};
    default:
      return { error: `unknown request type ${type}` };
  }
}

// `respond(message, host, defaults)` may return a response, a Promise, or `undefined` for the default.
export async function startFakeHost(t, port, options = {}) {
  const socket = await connectClient(t, port, "/host", options.origin ?? EXTENSION_ORIGIN);
  if (socket.status !== 101) throw new Error(`fake host refused with ${socket.status}`);
  const relayFrames = queue();
  const host = {
    socket,
    status: socket.status,
    version: options.version ?? FIXTURES.hostHello.version,
    name: options.name ?? FIXTURES.hostHello.name,
    capabilities: options.capabilities ?? [CAPABILITY],
    dictionaries: options.dictionaries ?? FIXTURES.hd_api_dictionaries.reply.dictionaries,
    files: options.files ?? new Map([["kanjidic", Buffer.from("PK\u0003\u0004fake kanjidic archive")]]),
    reportSize: options.reportSize ?? true,
    maxChunk: options.maxChunk,
    replyDelayMs: options.replyDelayMs ?? 0,
    respond: options.respond,
    tokenCounter: 0,
    openTokens: new Map(),
    closedTokens: [],
    clients: new Map(),
    opens: [],
    closes: [],
    hellos: [],
    requests: [],
    pongs: 0,
    listening: null,
    relayFrames,
    send(clientId, frame) { socket.send(JSON.stringify({ kind: "send", clientId, text: JSON.stringify(frame) })); },
    broadcast(frame) { socket.send(JSON.stringify({ kind: "broadcast", text: JSON.stringify(frame) })); },
    closeClient(clientId) { socket.send(JSON.stringify({ kind: "close", clientId })); },
    async network(enabled) {
      socket.send(JSON.stringify({ kind: "network", enabled }));
      return relayFrames.next("a network reply");
    },
    // Resolves once `predicate(host)` holds, polling every 10 ms.
    async until(predicate, what = "condition", timeoutMs = 5000) {
      const deadline = Date.now() + timeoutMs;
      while (!predicate(host)) {
        if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
        await sleep(10);
      }
      return host;
    },
    destroy() { socket.destroy(); },
    closed: socket.closed,
  };

  async function handleClientText(clientId, text) {
    let frame;
    try {
      frame = JSON.parse(text);
    } catch {
      host.send(clientId, { kind: "bye", reason: "malformed sharing frame" });
      host.closeClient(clientId);
      return;
    }
    const client = host.clients.get(clientId);
    if (!client) return;
    if (frame.kind === "hello") {
      if (frame.protocol !== 1) {
        host.send(clientId, { kind: "bye", reason: `unsupported sharing protocol ${JSON.stringify(frame.protocol)}` });
        host.closeClient(clientId);
        return;
      }
      Object.assign(client, { name: frame.name, version: frame.version, capabilities: frame.capabilities ?? [] });
      host.hellos.push({ clientId, ...frame });
      if (options.silentHello) return;
      host.send(clientId, { kind: "hello", protocol: 1, version: host.version, name: host.name, dictionaryCount: host.dictionaries.length, capabilities: host.capabilities, snapshot: {} });
      return;
    }
    if (frame.kind === "pong") {
      host.pongs += 1;
      return;
    }
    if (frame.kind === "request") {
      const record = { clientId, id: frame.id, message: frame.message, receivedAt: Date.now() };
      host.requests.push(record);
      if (host.replyDelayMs > 0) await sleep(host.replyDelayMs);
      let response = host.respond ? await host.respond(frame.message, host, defaultRespond) : undefined;
      if (response === undefined) response = defaultRespond(frame.message, host);
      if (response === null) return; // never reply
      host.send(clientId, { kind: "reply", id: frame.id, response });
      return;
    }
    host.send(clientId, { kind: "bye", reason: `unknown sharing frame ${JSON.stringify(frame.kind)}` });
    host.closeClient(clientId);
  }

  socket.onFrame((frame) => {
    if (frame.opcode !== 0x1) return;
    const message = JSON.parse(frame.payload.toString("utf8"));
    switch (message.kind) {
      case "listening":
        host.listening = message;
        return;
      case "client-open":
        host.clients.set(message.clientId, { id: message.clientId, origin: message.origin, address: message.address, capabilities: [] });
        host.opens.push(message);
        return;
      case "client-close":
        host.clients.delete(message.clientId);
        host.closes.push(message);
        return;
      case "client-text":
        void handleClientText(message.clientId, String(message.text));
        return;
      case "ping":
        return;
      default:
        relayFrames.push(message);
    }
  });
  await host.until((current) => current.listening !== null, "the listening frame");
  return host;
}
