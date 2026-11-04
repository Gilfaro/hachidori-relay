// Resource benchmark: the same scripted load against one relay command.
// Usage: node scripts/bench.mjs <label> <command> [args...]   (the relay must print "listening <port>")
// Windows only (reads process stats with PowerShell). Example against upstream:
//   node scripts/bench.mjs python python path/to/hachidori-anki/addon/server.py
// SPDX-License-Identifier: GPL-3.0-or-later
import { spawn, execFileSync } from "node:child_process";
import { connectClient, untilKind } from "../tests/conformance/relay-socket.mjs";

const [label, command, ...args] = process.argv.slice(2);
const child = spawn(command, [...args, "--port", "0", "--ping-seconds", "1"], { stdio: ["ignore", "pipe", "inherit"] });
const port = await new Promise((resolve) => {
  let text = "";
  child.stdout.on("data", (chunk) => {
    text += chunk;
    const match = text.match(/listening (\d+)/u);
    if (match) resolve(Number(match[1]));
  });
});

function stats() {
  const out = execFileSync("powershell", ["-NoProfile", "-Command",
    `$p = Get-Process -Id ${child.pid}; "$($p.WorkingSet64) $($p.PeakWorkingSet64) $($p.PrivateMemorySize64) $($p.TotalProcessorTime.TotalMilliseconds) $($p.Threads.Count)"`]).toString().trim().split(" ").map(Number);
  return { ws: out[0] / 2 ** 20, peak: out[1] / 2 ** 20, priv: out[2] / 2 ** 20, cpuMs: out[3], threads: out[4] };
}
const fmt = (s) => `WS ${s.ws.toFixed(1)} MB, peak ${s.peak.toFixed(1)} MB, private ${s.priv.toFixed(1)} MB, threads ${s.threads}`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const rows = {};

await sleep(500);
rows.idle = fmt(stats());

const host = await connectClient(null, port, "/host");
await untilKind(host, "listening");
const clients = [];
for (let index = 0; index < 200; index += 1) {
  clients.push(await connectClient(null, port, "/link"));
  await untilKind(host, "client-open");
}
await sleep(500);
const before = stats();
// 5 s idle with 201 peers and a ping every second.
await sleep(5000);
const after = stats();
rows["200 clients"] = `${fmt(after)}, CPU ${(after.cpuMs - before.cpuMs).toFixed(0)} ms over 5 s of pings`;

// 32 MiB lookup reply host -> client, and a 32 MiB message client -> host. Wall time, median of 3.
const big = "大".repeat(Math.floor((32 * 2 ** 20) / 3));
const reply = JSON.stringify({ kind: "reply", text: big });
const target = clients[0];
const id = "client-1";
const down = [];
const up = [];
for (let round = 0; round < 3; round += 1) {
  let start = performance.now();
  host.send(JSON.stringify({ kind: "send", clientId: id, text: reply }));
  await untilKind(target, "reply");
  down.push(performance.now() - start);
  start = performance.now();
  target.send(big);
  while ((await untilKind(host, "client-text")).clientId !== id) { /* skip */ }
  up.push(performance.now() - start);
}
const median = (values) => values.sort((a, b) => a - b)[1].toFixed(0);
rows["32 MiB"] = `host->client ${median(down)} ms, client->host ${median(up)} ms, ${fmt(stats())}`;

// A stalled client while healthy traffic continues: 64 x 512 KiB toward a client that never reads.
const stalled = clients[1];
stalled.pause();
const chunk = "x".repeat(512 * 1024);
for (let index = 0; index < 64; index += 1) host.send(JSON.stringify({ kind: "send", clientId: "client-2", text: chunk }));
const start = performance.now();
host.send(JSON.stringify({ kind: "send", clientId: "client-3", text: JSON.stringify({ kind: "probe" }) }));
await untilKind(clients[2], "probe");
rows["stalled peer"] = `healthy reply in ${(performance.now() - start).toFixed(0)} ms, ${fmt(stats())}`;

console.log(`## ${label}`);
for (const [name, value] of Object.entries(rows)) console.log(`- ${name}: ${value}`);
child.kill();
process.exit(0);
