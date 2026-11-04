// The relay as a plain process for the raw-socket contract tests.
// Vendored from bee-san/hachidori-anki; adapted to start the Rust binary.
// SPDX-License-Identifier: GPL-3.0-or-later
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

// The version the relay reports in /serverVersion and its API hello: the crate's own.
export const RELAY_VERSION = readFileSync(new URL("../../Cargo.toml", import.meta.url), "utf8").match(/^version\s*=\s*"([^"]+)"/mu)[1];

// HACHIDORI_RELAY_BIN is the built binary (cargo test sets it); the debug build is the fallback.
function relayBinary() {
  if (process.env.HACHIDORI_RELAY_BIN) return process.env.HACHIDORI_RELAY_BIN;
  const exe = process.platform === "win32" ? "hachidori-relay.exe" : "hachidori-relay";
  return fileURLToPath(new URL(`../../target/debug/${exe}`, import.meta.url));
}

// `apiPort`: undefined leaves the HTTP API off; 0 picks a free port.
// `extraArgs`: further flags, such as the limits (not in upstream).
export async function startAnkiRelayServer({ port = 0, pingMs = 20_000, apiPort, apiTimeoutSeconds, extraArgs = [] } = {}) {
  const args = ["--port", String(port), "--ping-seconds", String(pingMs / 1000), ...extraArgs];
  if (apiPort !== undefined) args.push("--api-port", String(apiPort));
  if (apiTimeoutSeconds !== undefined) args.push("--api-timeout-seconds", String(apiTimeoutSeconds));
  const child = spawn(relayBinary(), args, { stdio: ["ignore", "pipe", "inherit"] });
  let exitCode = null;
  const exited = new Promise((resolveExit) => child.once("exit", (code, signal) => {
    exitCode = code ?? signal;
    resolveExit();
  }));
  // The relay prints `listening <port>`, then `api <port>` or `api-failed <reason>` when asked for the API.
  const announced = await new Promise((resolveAnnounced, rejectAnnounced) => {
    const found = {};
    let text = "";
    child.once("error", rejectAnnounced);
    exited.then(() => rejectAnnounced(new Error(`the Anki relay exited with ${exitCode}`)));
    child.stdout.on("data", (chunk) => {
      text += String(chunk);
      let newline;
      while ((newline = text.indexOf("\n")) !== -1) {
        const line = text.slice(0, newline).trim();
        text = text.slice(newline + 1);
        const [word, ...rest] = line.split(" ");
        if (word === "listening") found.port = Number(rest[0]);
        else if (word === "api") found.apiPort = Number(rest[0]);
        else if (word === "api-failed") found.apiError = rest.join(" ");
      }
      if (found.port !== undefined && (apiPort === undefined || found.apiPort !== undefined || found.apiError !== undefined)) resolveAnnounced(found);
    });
  });
  return {
    port: announced.port,
    apiPort: announced.apiPort ?? null,
    apiError: announced.apiError ?? null,
    get exitCode() { return exitCode; },
    close() {
      child.kill();
      return exited;
    },
  };
}
