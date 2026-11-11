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
import vm from "node:vm";
import { relayBinary } from "./anki-relay-server.mjs";
import { connectHost, untilKind } from "./relay-socket.mjs";
import { oracleSkip } from "./oracle-skip.mjs";

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
export const skip = oracleSkip(missing);
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
if (!jsdomEntry && !skip && process.env.HACHIDORI_ORACLES === "required") {
  throw new Error("HACHIDORI_ORACLES=required, but jsdom is missing, so the oracles would leave Anki rendering out");
}

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
// `WebSocket` is what the background's sharing host connects to the relay with;
// without one it never connects.
export async function bootExtension({ WebSocket } = {}) {
  readSourcesAsLf();
  const fakes = await import(pathToFileURL(join(HACHIDORI, "test/extension-smoke/fakes.mjs")));
  fakes.installFakeIndexedDB();
  fakes.installFetch();
  // The engine is imported from a file: URL, so it reads its language packs there.
  const fakeFetch = globalThis.fetch;
  globalThis.fetch = async (input) => {
    const url = String(input);
    if (!url.startsWith("file:")) return fakeFetch(input);
    try {
      const body = readFileSync(fileURLToPath(url), "utf8");
      return { ok: true, status: 200, text: async () => body };
    } catch {
      return { ok: false, status: 404, text: async () => "" };
    }
  };
  fakes.installNavigator();
  const bus = fakes.makeBus();
  const storage = fakes.makeStorage();
  const alarms = fakes.makeAlarms();
  const offscreenChrome = fakes.makeChrome("offscreen", bus, storage, alarms);
  globalThis.chrome = offscreenChrome;
  // The build's language registry, which the background and the engine read
  // (extension/vendor/languages/registry.js); the smoke fakes leave it out.
  const vendored = {};
  vendored.globalThis = vendored;
  vm.runInNewContext(readFileSync(join(HACHIDORI, "extension/vendor/languages/registry.js"), "utf8"), vendored);
  globalThis.HDLanguages = vendored.HDLanguages;
  // The background bundle leaves profile-state.js out; the extension's own
  // profile tests hand its functions in (profiles.test.mjs).
  const profileState = await import(pathToFileURL(join(HACHIDORI, "extension/profile-state.js")));
  // So do the sharing host's language modules (sharing-profiles.test.mjs), which
  // read this realm's registry above.
  const sharingModules = await Promise.all(["japanese-view.js", "sharing-languages.js", "language-names.js"]
    .map((name) => import(pathToFileURL(join(HACHIDORI, "extension", name)))));
  // The context's own intrinsics stay its own: with this realm's Error, an error V8
  // raises inside the worker is no `instanceof Error` there and is described as "{}".
  const context = fakes.loadBackgroundScript({
    ...(WebSocket === undefined ? {} : { WebSocket }),
    chrome: fakes.makeChrome("sw", bus, storage, alarms), console, fetch: globalThis.fetch, setTimeout, clearTimeout, URL,
    HDLanguages: vendored.HDLanguages,
    ...Object.fromEntries(Object.entries(profileState).filter(([, value]) => typeof value === "function")),
    ...Object.assign({}, ...sharingModules),
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
  // `sendAnkiRequest(TARGET, ...)`: lookups go through the background, which
  // adds the profile's fields (upstream docs/languages.md 9.3).
  const PROFILED = new Set(["hd_lookup", "hd_kanji", "hd_segment"]);
  const apiEngine = async (fields) => {
    const reply = PROFILED.has(fields.type)
      ? await offscreenChrome.runtime.sendMessage({ target: "hoshidicts-offscreen", ...fields, requestId: "api" })
      : await engine({ ...fields, requestId: "api" });
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
        compactGlossaries: message.compactGlossaries === true, ...(message.profile ? { profile: message.profile } : {}),
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
  // `ankiOptionsOf`, `ankiProfileOf`, `apiProfile` and `apiDictionaries` (background-anki.js,
  // background-sharing.js) over this realm's records.
  const layer = globalThis.HDReaderOptions;
  const { personalDictionaryLanguage } = await import(pathToFileURL(join(HACHIDORI, "extension/custom-dictionary.js")));
  const storedOptions = async () => (await storage.api().local.get("options")).options;
  const readOptions = async (profileId) => {
    const stored = await storedOptions();
    return capabilityAnkiOptions(profileId === undefined ? layer.normaliseOptions(stored) : layer.projectOptions(stored, profileId),
      { screenshot: true, browserSpeech: false });
  };
  const readProfile = async (profileId) => {
    const stored = await storedOptions();
    if (!layer.hasProfiles(stored)) return null;
    const profile = layer.profileDescriptor(stored, profileId ?? layer.JAPANESE_PROFILE_ID);
    return profile === null ? null : { language: profile.language, packSettings: profile.packSettings };
  };
  const resolveProfile = async (profileIndex) => {
    const stored = await storedOptions();
    if (!layer.hasProfiles(stored)) return profileIndex === 0 ? { profileId: undefined } : null;
    const others = layer.profiles(stored).filter((profile) => profile.id !== layer.JAPANESE_PROFILE_ID);
    if (profileIndex === 0) {
      return layer.profiles(stored).some((profile) => profile.id === layer.JAPANESE_PROFILE_ID)
        ? { profileId: layer.JAPANESE_PROFILE_ID } : { profileId: undefined };
    }
    const profile = others[profileIndex - 1];
    return profile === undefined ? null : { profileId: profile.id };
  };
  const readDictionaries = async (profileId) => {
    const { dictionaryState: state } = await storage.api().local.get("dictionaryState");
    if (!layer.hasDictionaryProfiles(state)) return state?.dictionaries ?? [];
    return (layer.projectDictionaryState(state, profileId ?? layer.JAPANESE_PROFILE_ID)?.dictionaries ?? [])
      .filter((row) => [null, layer.JAPANESE_PROFILE_ID].includes(personalDictionaryLanguage(row)));
  };
  const apiHost = createApiHost({
    version: "oracle",
    engine: apiEngine,
    render,
    readDictionaries,
    readAudioSources: async (profileId) => (await readOptions(profileId)).audioSources.filter((source) => source.enabled),
    readAnkiTemplates: async (profileId) => (await readOptions(profileId)).anki.templates,
    readSmallerAnkiCards: async () => (await readOptions()).smallerAnkiCards,
    readCompactGlossaries: async () => (await readOptions()).glossaryLayoutMode === "compact-popup-anki",
    resolveProfile,
    readProfile,
  });
  const api = (message) => apiHost(message).catch((error) => ({ type: `${message.type}_result`, requestId: message.requestId ?? null,
    ok: false, error: error.message, generation: 0 }));
  const { openBackupArchive } = await import(pathToFileURL(join(HACHIDORI, "extension/backup-archive.js")));
  const { assertBackupSnapshot } = await import(pathToFileURL(join(HACHIDORI, "extension/backup-state.js")));
  // A page's request, which the background relays to the engine with its profile's fields.
  const page = (message) => offscreenChrome.runtime.sendMessage(message);
  return { context, engine, page, api, snapshot, storage, openBackupArchive, assertBackupSnapshot, createObjectURL: fakes.createObjectURL };
}

// What may differ by design (docs/host-mode.md) between a reply or stored value of
// the extension and of the relay: each side's own load counter and load path, the
// new generations' random names (numbered in the order each side made them), other
// random ids, such as a new profile's Template ids (numbered the same way), upload
// tokens, and the time each side imported a package.
export function comparer() {
  const names = new Map();
  const ids = new Map();
  const walk = (value) => {
    if (typeof value === "string" && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/u.test(value)) {
      if (!ids.has(value)) ids.set(value, `id-${ids.size + 1}`);
      return ids.get(value);
    }
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
  const socket = await connectHost(t, port);
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
