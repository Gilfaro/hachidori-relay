// Download benchmark: 4 concurrent 100 MiB dictionary downloads through the API,
// served by upstream's fake host. Measures wall time, CPU and peak memory of the relay.
// Usage: node scripts/bench-downloads.mjs <label> <command> [args...]
// Windows only (reads process stats with PowerShell). Example against upstream:
//   node scripts/bench-downloads.mjs python python path/to/hachidori-anki/addon/server.py
// SPDX-License-Identifier: GPL-3.0-or-later
import { spawn, execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { startFakeHost } from "../tests/conformance/fake-host.mjs";

const [label, command, ...args] = process.argv.slice(2);
const MIB = 2 ** 20;
const SIZE = 100 * MIB;
const COUNT = 4;

const child = spawn(command, [...args, "--port", "0", "--api-port", "0"], { stdio: ["ignore", "pipe", "inherit"] });
const { port, apiPort } = await new Promise((resolve) => {
  let text = "";
  child.stdout.on("data", (chunk) => {
    text += chunk;
    const relay = text.match(/listening (\d+)/u);
    const api = text.match(/api (\d+)/u);
    if (relay && api) resolve({ port: Number(relay[1]), apiPort: Number(api[1]) });
  });
});

function stats() {
  const out = execFileSync("powershell", ["-NoProfile", "-Command",
    `$p = Get-Process -Id ${child.pid}; "$($p.WorkingSet64) $($p.PeakWorkingSet64) $($p.PrivateMemorySize64) $($p.TotalProcessorTime.TotalMilliseconds)"`]).toString().trim().split(" ").map(Number);
  return { ws: out[0] / MIB, peak: out[1] / MIB, priv: out[2] / MIB, cpuMs: out[3] };
}

const files = new Map();
const dictionaries = [];
for (let index = 0; index < COUNT; index += 1) {
  const id = `dict-${index}`;
  files.set(id, randomBytes(SIZE));
  dictionaries.push({ id, title: id, revision: "1", fileName: `${id}.zip` });
}
const host = await startFakeHost(null, port, { files, dictionaries });
// Warm the session so the measurement is downloads only.
await fetch(`http://127.0.0.1:${apiPort}/dictionaries`).then((response) => response.arrayBuffer());

const before = stats();
let peakPrivate = before.priv;
const sampler = setInterval(() => { peakPrivate = Math.max(peakPrivate, stats().priv); }, 250);
const start = performance.now();
const sizes = await Promise.all(dictionaries.map(async ({ id }) => {
  const response = await fetch(`http://127.0.0.1:${apiPort}/dictionaries/${id}`);
  const body = Buffer.from(await response.arrayBuffer());
  if (!body.equals(files.get(id))) throw new Error(`${id} arrived corrupted (${body.length} bytes)`);
  return body.length;
}));
const seconds = (performance.now() - start) / 1000;
clearInterval(sampler);
const after = stats();
const total = sizes.reduce((sum, size) => sum + size, 0) / MIB;

console.log(`## ${label}`);
console.log(`- ${COUNT} x ${SIZE / MIB} MiB downloads: ${seconds.toFixed(1)} s (${(total / seconds).toFixed(0)} MiB/s), all intact`);
console.log(`- relay CPU ${((after.cpuMs - before.cpuMs) / 1000).toFixed(1)} s; peak WS ${after.peak.toFixed(0)} MB; peak private (sampled) ${peakPrivate.toFixed(0)} MB; after: WS ${after.ws.toFixed(0)} MB`);
console.log(`- tokens closed: ${host.closedTokens.length}/${COUNT}`);
child.kill();
host.destroy();
process.exit(0);
