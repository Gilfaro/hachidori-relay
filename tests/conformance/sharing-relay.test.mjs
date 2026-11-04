// Drives the relay, run as a
// process, over raw sockets.
// SPDX-License-Identifier: GPL-3.0-or-later
import assert from "node:assert/strict";
import test from "node:test";
import { startAnkiRelayServer } from "./anki-relay-server.mjs";
import { EXTENSION_ORIGIN, FIREFOX_ORIGIN, HOSHIDICTS_ORIGIN, connectClient, untilKind } from "./relay-socket.mjs";

async function server(t) {
  const started = await startAnkiRelayServer({ pingMs: 50 });
  t.after(() => started.close());
  return started;
}

const listening = (port) => ({ kind: "listening", port });

test("the relay survives idle accept timeouts before and after a host connects", async (t) => {
  const relay = await startAnkiRelayServer();
  t.after(() => relay.close());
  await new Promise(resolveWait => setTimeout(resolveWait, 1500));
  assert.equal(relay.exitCode, null, "an idle listener must stay alive");
  const host = await connectClient(t, relay.port, "/host");
  assert.deepEqual(await host.json(), listening(relay.port));
  await new Promise(resolveWait => setTimeout(resolveWait, 1500));
  assert.equal(relay.exitCode, null, "an idle host must not shut the listener down");
  const client = await connectClient(t, relay.port, "/link");
  assert.equal(client.status, 101);
  assert.equal((await untilKind(host, "client-open")).origin, EXTENSION_ORIGIN);
});

test("clients are refused until a host connects, and a second host is turned away", async (t) => {
  const { port } = await server(t);
  assert.equal((await connectClient(t, port, "/link")).status, 503);
  assert.equal((await connectClient(t, port, "/host", "https://example.com")).status, 403);
  assert.equal((await connectClient(t, port, "/host", null)).status, 403);
  assert.equal((await connectClient(t, port, "/elsewhere")).status, 404);
  const host = await connectClient(t, port, "/host");
  assert.equal(host.status, 101);
  assert.deepEqual(await host.json(), listening(port));
  const second = await connectClient(t, port, "/host");
  assert.equal(second.status, 101);
  assert.deepEqual(await second.json(), { kind: "listen-failed", error: "Another browser on this computer is already sharing through Anki." });
  await second.closeFrame();
  const client = await connectClient(t, port, "/link");
  assert.equal(client.status, 101);
  const opened = await untilKind(host, "client-open");
  assert.equal(opened.origin, EXTENSION_ORIGIN);
});

test("Hachidori for Firefox may host and link like Hachidori for Chrome", async (t) => {
  const { port } = await server(t);
  const host = await connectClient(t, port, "/host", FIREFOX_ORIGIN);
  assert.equal(host.status, 101);
  assert.deepEqual(await host.json(), listening(port));
  const chrome = await connectClient(t, port, "/link", EXTENSION_ORIGIN);
  assert.equal(chrome.status, 101);
  assert.equal((await untilKind(host, "client-open")).origin, EXTENSION_ORIGIN);
  const firefox = await connectClient(t, port, "/link", FIREFOX_ORIGIN);
  assert.equal(firefox.status, 101);
  assert.equal((await untilKind(host, "client-open")).origin, FIREFOX_ORIGIN);
  for (const origin of ["moz-extension:", "https://moz-extension.example", "resource://gre"]) {
    assert.equal((await connectClient(t, port, "/link", origin)).status, 403, `${origin} is not an extension`);
  }
});

test("the Hoshidicts app origin is admitted only on the client endpoint", async (t) => {
  const { port } = await server(t);
  assert.equal((await connectClient(t, port, "/link", HOSHIDICTS_ORIGIN)).status, 503, "host availability still gates native clients");
  assert.equal((await connectClient(t, port, "/host", HOSHIDICTS_ORIGIN)).status, 403, "native apps cannot become hosts");
  assert.equal((await connectClient(t, port, "/host", "hoshi://hoshidicts.evil")).status, 403);
  const host = await connectClient(t, port, "/host");
  await host.json();
  for (const origin of ["https://hoshidicts", "http://hoshidicts", "app://mangatan", "hoshi://mangatan", "hoshi://hoshidicts.evil", null]) {
    assert.equal((await connectClient(t, port, "/link", origin)).status, 403, `${origin} must not enter the native-client allowlist`);
  }
  const client = await connectClient(t, port, "/link", HOSHIDICTS_ORIGIN);
  assert.equal(client.status, 101);
  assert.equal((await untilKind(host, "client-open")).origin, HOSHIDICTS_ORIGIN);
});

test("text crosses the relay whole in both directions, with pings on both sides", async (t) => {
  const { port } = await server(t);
  const host = await connectClient(t, port, "/host");
  await host.json();
  const first = await connectClient(t, port, "/link");
  const { clientId } = await untilKind(host, "client-open");
  const large = "大きな".repeat(Math.ceil(1.5 * 1024 * 1024 / 3));
  first.send(large);
  const relayed = await untilKind(host, "client-text");
  assert.equal(relayed.clientId, clientId);
  assert.equal(relayed.text, large);
  host.send(JSON.stringify({ kind: "send", clientId, text: JSON.stringify({ kind: "reply", id: "r1", response: { ok: true, text: "reply ✓" } }) }));
  assert.deepEqual(await untilKind(first, "reply"), { kind: "reply", id: "r1", response: { ok: true, text: "reply ✓" } });
  const second = await connectClient(t, port, "/link");
  const secondOpen = await untilKind(host, "client-open");
  host.send(JSON.stringify({ kind: "broadcast", text: JSON.stringify({ kind: "storage", changes: { options: null } }) }));
  const [one, two] = await Promise.all([untilKind(first, "storage"), untilKind(second, "storage")]);
  assert.deepEqual([one, two], [{ kind: "storage", changes: { options: null } }, { kind: "storage", changes: { options: null } }]);
  assert.deepEqual(await untilKind(host, "ping"), { kind: "ping" });
  assert.deepEqual(await untilKind(second, "ping"), { kind: "ping" });
  host.send(JSON.stringify({ kind: "close", clientId: secondOpen.clientId }));
  await second.closeFrame();
  await second.closed;
  const closedFrame = await untilKind(host, "client-close");
  assert.equal(closedFrame.clientId, secondOpen.clientId);
  first.close();
  assert.equal((await untilKind(host, "client-close")).clientId, clientId);
});

test("losing the host closes its clients, and the next host is accepted", async (t) => {
  const { port } = await server(t);
  const host = await connectClient(t, port, "/host");
  await host.json();
  const client = await connectClient(t, port, "/link");
  await untilKind(host, "client-open");
  host.destroy();
  await client.closeFrame();
  await client.closed;
  assert.equal((await connectClient(t, port, "/link")).status, 503, "the lost host is forgotten");
  const next = await connectClient(t, port, "/host");
  assert.deepEqual(await next.json(), listening(port));
});

// The relay listens on this computer alone until the host asks for the network.
// Skipped, with a note, on a machine that has no address other than loopback.
async function networkOn(t, host) {
  host.send(JSON.stringify({ kind: "network", enabled: true }));
  const reply = await untilKind(host, "network");
  assert.equal(reply.enabled, true);
  assert.ok(Array.isArray(reply.addresses));
  if (reply.addresses.length === 0) {
    t.diagnostic("no network address on this machine; the network cases did not run");
    return null;
  }
  for (const entry of reply.addresses) assert.match(entry.kind, /^(tailscale|local)$/u, JSON.stringify(entry));
  return reply.addresses[0].address;
}

test("the host opens the relay to the network and closes it again", async (t) => {
  const { port } = await server(t);
  const host = await connectClient(t, port, "/host");
  assert.deepEqual(await host.json(), listening(port));
  const address = await networkOn(t, host);
  if (address === null) return;
  assert.equal((await connectClient(t, port, "/host", EXTENSION_ORIGIN, address)).status, 403, "only this computer's Hachidori hosts");
  const remote = await connectClient(t, port, "/link", EXTENSION_ORIGIN, address);
  assert.equal(remote.status, 101);
  const remoteOpen = await untilKind(host, "client-open");
  assert.equal(remoteOpen.address, address, "the host learns where a linked browser is");
  const local = await connectClient(t, port, "/link");
  assert.equal(local.status, 101);
  assert.equal((await untilKind(host, "client-open")).address, "127.0.0.1");
  host.send(JSON.stringify({ kind: "network", enabled: false }));
  assert.deepEqual(await untilKind(host, "network"), { kind: "network", enabled: false, addresses: [] });
  await remote.closeFrame();
  await remote.closed;
  assert.equal((await untilKind(host, "client-close")).clientId, remoteOpen.clientId);
  await assert.rejects(connectClient(t, port, "/link", EXTENSION_ORIGIN, address), /ECONNREFUSED/u, "the network listener is gone");
  local.send("still here");
  assert.equal((await untilKind(host, "client-text")).text, "still here", "the linked browser on this computer stays");
  assert.equal((await connectClient(t, port, "/link")).status, 101, "the relay still accepts on this computer");
});

test("losing the host returns the relay to this computer", async (t) => {
  const { port } = await server(t);
  const host = await connectClient(t, port, "/host");
  await host.json();
  const address = await networkOn(t, host);
  if (address === null) return;
  host.send(JSON.stringify({ kind: "network", enabled: true }));
  assert.equal((await untilKind(host, "network")).enabled, true);
  assert.equal((await connectClient(t, port, "/link", EXTENSION_ORIGIN, address)).status, 101);
  host.destroy();
  await new Promise((resolveWait) => setTimeout(resolveWait, 200));
  await assert.rejects(connectClient(t, port, "/link", EXTENSION_ORIGIN, address), /ECONNREFUSED/u);
  const next = await connectClient(t, port, "/host");
  assert.deepEqual(await next.json(), listening(port));
});

// Larger than the TCP buffers while a peer is paused, using real sockets.
const largeReply = () => "大きな".repeat(Math.ceil(16 * 1024 * 1024 / 9));
function sendReply(host, clientId, reply) {
  host.send(JSON.stringify({ kind: "send", clientId, text: JSON.stringify(reply) }));
}

test("a stalled peer leaves healthy requests and pings responsive, then receives large frames in order", { timeout: 20000 }, async (t) => {
  const { port } = await server(t);
  const host = await connectClient(t, port, "/host");
  await host.json();
  const slow = await connectClient(t, port, "/link");
  const { clientId: slowId } = await untilKind(host, "client-open");
  const healthy = await connectClient(t, port, "/link");
  const { clientId: healthyId } = await untilKind(host, "client-open");
  slow.pause();
  const text = largeReply();
  sendReply(host, slowId, { kind: "large", sequence: 1, text });
  host.send(JSON.stringify({ kind: "broadcast", text: JSON.stringify({ kind: "marker", sequence: 2 }) }));
  assert.equal((await untilKind(healthy, "marker")).sequence, 2, "the host can broadcast past a stalled send");
  healthy.send("healthy lookup");
  const request = await untilKind(host, "client-text");
  assert.equal(request.clientId, healthyId);
  assert.equal(request.text, "healthy lookup");
  sendReply(host, healthyId, { kind: "reply", text: "healthy result" });
  assert.equal((await untilKind(healthy, "reply")).text, "healthy result");
  await healthy.ping("healthy control ping");
  assert.deepEqual(await untilKind(healthy, "ping"), { kind: "ping" });
  sendReply(host, slowId, { kind: "large", sequence: 3, text });
  slow.resume();
  const first = await untilKind(slow, "large");
  assert.equal(first.sequence, 1);
  assert.ok(first.text === text, "the first large UTF-8 frame arrives intact");
  assert.equal((await untilKind(slow, "marker")).sequence, 2);
  const last = await untilKind(slow, "large");
  assert.equal(last.sequence, 3);
  assert.ok(last.text === text, "the queued large UTF-8 frame arrives intact after the broadcast");
});

test("network disable and host loss interrupt stalled sends and release their connections", { timeout: 20000 }, async (t) => {
  const { port } = await server(t);
  const host = await connectClient(t, port, "/host");
  await host.json();
  const address = await networkOn(t, host);
  if (address === null) return;
  const remote = await connectClient(t, port, "/link", EXTENSION_ORIGIN, address);
  const { clientId: remoteId } = await untilKind(host, "client-open");
  const healthy = await connectClient(t, port, "/link");
  await untilKind(host, "client-open");
  remote.pause();
  const text = largeReply();
  sendReply(host, remoteId, { kind: "large", text });
  host.send(JSON.stringify({ kind: "network", enabled: false }));
  assert.equal((await untilKind(host, "network")).enabled, false);
  assert.equal((await untilKind(host, "client-close")).clientId, remoteId);
  remote.resume();
  await remote.closed;
  healthy.send("still local");
  assert.equal((await untilKind(host, "client-text")).text, "still local");
  await assert.rejects(connectClient(t, port, "/link", EXTENSION_ORIGIN, address), /ECONNREFUSED/u);

  const slow = await connectClient(t, port, "/link");
  const { clientId: slowId } = await untilKind(host, "client-open");
  slow.pause();
  sendReply(host, slowId, { kind: "large", text });
  host.send(JSON.stringify({ kind: "network", enabled: false }));
  await untilKind(host, "network");
  host.destroy();
  await healthy.closeFrame();
  await healthy.closed;
  slow.resume();
  await slow.closed;
  const next = await connectClient(t, port, "/host");
  assert.deepEqual(await next.json(), listening(port), "host loss releases the host slot too");
});
