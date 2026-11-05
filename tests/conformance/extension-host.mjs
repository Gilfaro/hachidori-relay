// The real Hachidori extension in Node, as the browser host the relay mirrors: its
// engine service (wasm Hoshidicts) and background worker on the smoke suite's
// fakes, with the extension's fixture dictionaries imported. Shared by the oracle
// tests that compare the relay's host mode against it.
//
// Not from upstream.
// SPDX-License-Identifier: GPL-3.0-or-later
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs, { existsSync, readFileSync } from "node:fs";
import { createRequire, syncBuiltinESMExports } from "node:module";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { relayBinary } from "./anki-relay-server.mjs";
import { connectClient, untilKind } from "./relay-socket.mjs";

// The extension fakes replace fetch; the HTTP API is reached with the real one.
export const realFetch = globalThis.fetch;

const REPO = fileURLToPath(new URL("../..", import.meta.url));
export const HACHIDORI = resolve(process.env.HACHIDORI_SOURCE ?? join(REPO, "..", "hachidori"));
export const FIXTURES = ["hachidori-fixture.zip", "hachidori-fixture-trained.zip", "hachidori-fixture-many-banks.zip"];
const HOST_MODE = execFileSync(relayBinary(), ["--help"], { encoding: "utf8" }).includes("--host");
const missing = [
  ...(HOST_MODE ? [] : ["a relay built with host mode"]),
  ...["extension/vendor/hoshidicts.wasm", ...FIXTURES.map((name) => `test/fixtures/${name}`)]
    .filter((path) => !existsSync(join(HACHIDORI, path))).map((path) => join(HACHIDORI, path)),
];
export const skip = missing.length > 0 && `needs ${missing.join(", ")}`;
// jsdom, for the extension's Anki field rendering, where Hachidori's own tests look
// for it; without it the oracles leave Anki rendering out.
const JSDOM_PATHS = [process.env.HACHIDORI_JSDOM, join(HACHIDORI, "test/tooling"), join(homedir(), ".cache/hachidori-e2e")].filter(Boolean);
export const jsdomEntry = (() => {
  try {
    return createRequire(import.meta.url).resolve("jsdom", { paths: JSDOM_PATHS });
  } catch {
    return null;
  }
})();

// The smoke fakes strip a module's imports with LF-only patterns, so a checkout with
// CRLF line endings (Git for Windows' default) is read as LF.
function readSourcesAsLf() {
  const read = fs.readFileSync;
  fs.readFileSync = function readFileSyncLf(path, options) {
    const data = read.call(this, path, options);
    return typeof data === "string" && String(path).startsWith(HACHIDORI) ? data.replaceAll("\r\n", "\n") : data;
  };
  syncBuiltinESMExports();
}

// The extension in Node, as test/extension-smoke/engine.mjs boots it.
export async function bootExtension() {
  readSourcesAsLf();
  const fakes = await import(pathToFileURL(join(HACHIDORI, "test/extension-smoke/fakes.mjs")));
  fakes.installFakeIndexedDB();
  fakes.installFetch();
  fakes.installNavigator();
  const bus = fakes.makeBus();
  const storage = fakes.makeStorage();
  const alarms = fakes.makeAlarms();
  const offscreenChrome = fakes.makeChrome("offscreen", bus, storage, alarms);
  globalThis.chrome = offscreenChrome;
  // The context's own intrinsics stay its own: with this realm's Error, an error V8
  // raises inside the worker is no `instanceof Error` there and is described as "{}".
  fakes.loadBackgroundScript({
    chrome: fakes.makeChrome("sw", bus, storage, alarms), console, fetch: globalThis.fetch, setTimeout, clearTimeout, URL,
  });
  const service = await import(pathToFileURL(join(HACHIDORI, "extension/engine-service.js")));
  const { default: createHoshidicts } = await import(pathToFileURL(join(HACHIDORI, "extension/vendor/hoshidicts.mjs")));
  service.configureEngineService((message) => offscreenChrome.runtime.sendMessage(message),
    { createHoshidicts, storageBackend: "idbfs", lowRam: true });
  service.startEngine();
  offscreenChrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
    if (message?.target !== "hoshidicts-offscreen" || message.relayed !== true) return false;
    service.handleEngineMessage(message).then(sendResponse);
    return true;
  });
  const engine = (message) => service.handleEngineMessage(message);
  for (let status = await engine({ type: "hd_status" }); !(status.ready && !status.loading);) {
    await new Promise((done) => setTimeout(done, 25));
    status = await engine({ type: "hd_status" });
  }
  for (const name of FIXTURES) {
    const blobUrl = fakes.createObjectURL(new Uint8Array(readFileSync(join(HACHIDORI, "test/fixtures", name))));
    const reply = await engine({ type: "hd_import", blobUrl, fileName: name });
    assert.equal(reply.ok, true, `${name}: ${reply.error}`);
  }
  const snapshot = await storage.api().local.get(["dictionaryState", "options", "customDictionarySource",
    "dictionaryUpdates", "lookupStats", "wordStatusOverrides"]);
  // The API host as background-sharing.js builds it, answering as dispatchSharedRequest does.
  const { createApiHost } = await import(pathToFileURL(join(HACHIDORI, "extension/api-host.js")));
  const apiEngine = async (fields) => {
    const reply = await engine({ ...fields, requestId: "api" });
    if (!reply?.ok) throw new Error(reply?.error || "Anki preparation did not complete.");
    return reply;
  };
  // The offscreen document's Anki rendering (`createAnkiOffscreenService`), in jsdom.
  // A pronunciation needs a source, and this oracle configures none.
  let render = async () => { throw new Error("jsdom is unavailable"); };
  if (jsdomEntry) {
    const { buildAnkiResourceFields } = await import(pathToFileURL(join(HACHIDORI, "extension/anki-resources.js")));
    const { JSDOM } = createRequire(import.meta.url)(jsdomEntry);
    const { document } = new JSDOM("<!doctype html><html><body></body></html>").window;
    render = async (message) => {
      if (message.type === "hd_anki_audio") throw new Error("Browser text-to-speech recording is unavailable.");
      return buildAnkiResourceFields(message.request, message.templates, {
        document, dictionaryPaths: message.dictionaryPaths, audio: message.audio, compactGlossary: message.compactGlossary === true,
        styles: async () => {
          const reply = await apiEngine({ type: "hd_styles" });
          if (reply.generation !== message.request.generation) throw new Error("Dictionary styles changed during Anki preparation.");
          return reply.styles;
        },
      });
    };
  }
  // `readAnkiOptions`, for a host without browser speech, as the relay is.
  await import(pathToFileURL(join(HACHIDORI, "extension/reader-options.js")));
  const { capabilityAnkiOptions } = await import(pathToFileURL(join(HACHIDORI, "extension/setup-state.js")));
  const readOptions = async () => capabilityAnkiOptions(
    globalThis.HDReaderOptions.normaliseOptions((await storage.api().local.get("options")).options),
    { screenshot: true, browserSpeech: false });
  const apiHost = createApiHost({
    version: "oracle",
    engine: apiEngine,
    render,
    readDictionaries: async () => (await storage.api().local.get("dictionaryState")).dictionaryState?.dictionaries ?? [],
    readAudioSources: async () => (await readOptions()).audioSources.filter((source) => source.enabled),
    readAnkiTemplates: async () => (await readOptions()).anki.templates,
    readSmallerAnkiCards: async () => (await readOptions()).smallerAnkiCards,
  });
  const api = (message) => apiHost(message).catch((error) => ({ type: `${message.type}_result`, requestId: message.requestId ?? null,
    ok: false, error: error.message, generation: 0 }));
  const { openBackupArchive } = await import(pathToFileURL(join(HACHIDORI, "extension/backup-archive.js")));
  const { assertBackupSnapshot } = await import(pathToFileURL(join(HACHIDORI, "extension/backup-state.js")));
  return { engine, api, snapshot, storage, openBackupArchive, assertBackupSnapshot, createObjectURL: fakes.createObjectURL };
}

// What may differ by design (docs/host-mode.md) between a reply or stored value of
// the extension and of the relay: each side's own load counter and load path, the
// new generations' random names (numbered in the order each side made them), upload
// tokens, and the time each side imported a package.
export function comparer() {
  const names = new Map();
  const walk = (value) => {
    if (typeof value === "string") {
      return value.replace(/\.hdw-generation-[0-9a-f-]{36}/gu, (name) => {
        if (!names.has(name)) names.set(name, `generation-${names.size + 1}`);
        return names.get(name);
      });
    }
    if (Array.isArray(value)) return value.map(walk);
    if (value === null || typeof value !== "object") return value;
    return Object.fromEntries(Object.entries(value).filter(([key]) => !["generation", "loadPath"].includes(key))
      .map(([key, field]) => [key, key === "installedAt" && typeof field === "string" ? "time" : key === "token" && typeof field === "string" ? "token" : walk(field)]));
  };
  return (value) => walk(JSON.parse(JSON.stringify(value)));
}

// The extension as the relay's browser host: its hello, and its answers to the
// relay's pull session (api-host.js' hd_api_dictionary_*, from the engine).
export async function serveAsBrowserHost(t, port, { engine, snapshot }) {
  const socket = await connectClient(t, port, "/host");
  assert.equal(socket.status, 101);
  await untilKind(socket, "listening");
  const open = await untilKind(socket, "client-open");
  const says = (frame) => socket.send(JSON.stringify({ kind: "send", clientId: open.clientId, text: JSON.stringify(frame) }));
  const shapes = {
    hd_api_dictionary_open: (reply) => ({ token: reply.token, size: reply.size, fileName: "dictionary.hachidori.zip" }),
    hd_api_dictionary_read: (reply) => ({ data: reply.data, eof: reply.eof }),
    hd_api_dictionary_close: () => ({}),
  };
  (async () => {
    for (;;) {
      const frame = JSON.parse((await untilKind(socket, "client-text")).text);
      if (frame.kind === "hello") {
        says({ kind: "hello", protocol: 1, version: "oracle", name: "Hachidori", dictionaryCount: 3, capabilities: ["hoshidicts-api-v1"], snapshot });
      } else if (frame.kind === "request") {
        const reply = await engine(frame.message);
        says({ kind: "reply", id: frame.id, response: reply.ok ? shapes[frame.message.type](reply) : { error: reply.error } });
      }
    }
  })().catch(() => {});
}
