// The relay's safety limits, which upstream does not have. Each is tested under, at
// and over its value, through the CLI flags. Going over closes only the offending
// peer, and the default limits never trip on large but legitimate traffic.
// Mirrors tests/limits.rs, which tests the same through the library.
// Set HACHIDORI_STRESS=1 to add the 128 MiB run.
// SPDX-License-Identifier: GPL-3.0-or-later
import assert from "node:assert/strict";
import { connect } from "node:net";
import test from "node:test";
import { startAnkiRelayServer } from "./anki-relay-server.mjs";
import { EXTENSION_ORIGIN, connectClient, untilKind } from "./relay-socket.mjs";

async function relay(t, { handshake, message, queued } = {}) {
  const extraArgs = [];
  if (handshake !== undefined) extraArgs.push("--max-handshake-bytes", String(handshake));
  if (message !== undefined) extraArgs.push("--max-message-bytes", String(message));
  if (queued !== undefined) extraArgs.push("--max-queued-bytes", String(queued));
  const server = await startAnkiRelayServer({ extraArgs });
  t.after(() => server.close());
  return server;
}

async function host(t, port) {
  const socket = await connectClient(t, port, "/host");
  assert.equal(socket.status, 101);
  assert.equal((await untilKind(socket, "listening")).port, port);
  return socket;
}

async function link(t, port, hostSocket) {
  const socket = await connectClient(t, port, "/link");
  assert.equal(socket.status, 101);
  return { socket, id: (await untilKind(hostSocket, "client-open")).clientId };
}

// A frame with an all-zero mask: legal, and it leaves the payload as it is, so large
// frames cost no masking time.
function frame(opcode, fin, payload) {
  const first = (fin ? 0x80 : 0) | opcode;
  let header;
  if (payload.length <= 125) header = Buffer.from([first, 0x80 | payload.length]);
  else if (payload.length <= 0xffff) {
    header = Buffer.from([first, 0x80 | 126, 0, 0]);
    header.writeUInt16BE(payload.length, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = first;
    header[1] = 0x80 | 127;
    header.writeBigUInt64BE(BigInt(payload.length), 2);
  }
  return Buffer.concat([header, Buffer.alloc(4), payload]);
}

// A raw upgrade request carrying `padding` extra header bytes. Resolves with the
// status, and whether the relay then closed the connection.
function paddedUpgrade(port, padding) {
  return new Promise((resolve, reject) => {
    const socket = connect(port, "127.0.0.1");
    let text = "";
    let status = null;
    socket.on("error", reject);
    socket.on("data", (chunk) => {
      text += chunk.toString("latin1");
      if (status === null && text.includes("\r\n\r\n")) {
        status = Number(text.split(" ")[1]);
        if (status === 101) {
          socket.destroy();
          resolve({ status, closed: false });
        }
      }
    });
    socket.on("close", () => resolve({ status, closed: true }));
    socket.write([
      "GET /link HTTP/1.1", `Host: 127.0.0.1:${port}`, "Upgrade: websocket", "Connection: Upgrade",
      "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==", "Sec-WebSocket-Version: 13", `Origin: ${EXTENSION_ORIGIN}`,
      `X-Pad: ${"p".repeat(padding)}`, "", "",
    ].join("\r\n"));
  });
}

test("an upgrade request over the limit gets 431 and the relay carries on", async (t) => {
  const { port } = await relay(t, { handshake: 1024 });
  await host(t, port);
  assert.equal((await paddedUpgrade(port, 600)).status, 101, "well under the limit");
  assert.deepEqual(await paddedUpgrade(port, 2000), { status: 431, closed: true });
  // The relay still works: a second host is told the slot is taken.
  const second = await connectClient(t, port, "/host");
  assert.equal(second.status, 101);
  await untilKind(second, "listen-failed");
});

test("a message at the limit passes, and one over it closes only its sender", async (t) => {
  const limit = 64 * 1024;
  const { port } = await relay(t, { message: limit });
  const hostSocket = await host(t, port);
  const big = await link(t, port, hostSocket);
  const other = await link(t, port, hostSocket);

  big.socket.send("a".repeat(limit));
  const exact = await untilKind(hostSocket, "client-text");
  assert.equal(exact.clientId, big.id);
  assert.equal(exact.text.length, limit, "exactly at the limit is fine");

  // Fragments count together: two halves that add up to one byte over.
  big.socket.sendRaw(frame(0x1, false, Buffer.alloc(limit / 2, "b")));
  big.socket.sendRaw(frame(0x0, true, Buffer.alloc(limit / 2 + 1, "b")));
  await big.socket.closed;
  assert.equal((await untilKind(hostSocket, "client-close")).clientId, big.id);

  other.socket.send("still here");
  const relayed = await untilKind(hostSocket, "client-text");
  assert.deepEqual([relayed.clientId, relayed.text], [other.id, "still here"]);

  // One whole frame over the limit is refused from its header alone.
  const single = await link(t, port, hostSocket);
  single.socket.send("c".repeat(limit + 1));
  await single.socket.closed;
  assert.equal((await untilKind(hostSocket, "client-close")).clientId, single.id);

  // A host over the limit is closed too; its clients go with it, and a new host is accepted.
  hostSocket.send("h".repeat(limit + 1));
  await hostSocket.closed;
  await other.socket.closed;
  await host(t, port);
});

test("a stalled client over its queue limit is dropped, and everyone else carries on", async (t) => {
  const { port } = await relay(t, { queued: 4 * 1024 * 1024 });
  const hostSocket = await host(t, port);
  const stalled = await link(t, port, hostSocket);
  stalled.socket.pause();
  const healthy = await link(t, port, hostSocket);
  // 64 × 512 KiB = 32 MiB: far more than the kernel's buffers plus the 4 MiB queue.
  const chunk = "x".repeat(512 * 1024);
  for (let index = 0; index < 64; index += 1) hostSocket.send(JSON.stringify({ kind: "send", clientId: stalled.id, text: chunk }));
  assert.equal((await untilKind(hostSocket, "client-close")).clientId, stalled.id, "the stalled client is dropped");

  hostSocket.send(JSON.stringify({ kind: "send", clientId: healthy.id, text: JSON.stringify({ kind: "reply", ok: true }) }));
  assert.equal((await untilKind(healthy.socket, "reply")).ok, true, "the healthy client is unaffected");
  healthy.socket.send("lookup");
  assert.equal((await untilKind(hostSocket, "client-text")).text, "lookup", "the host is unaffected");
});

test("a queue just under the limit is delivered whole once the client reads", async (t) => {
  const { port } = await relay(t, { queued: 4 * 1024 * 1024 });
  const hostSocket = await host(t, port);
  const slow = await link(t, port, hostSocket);
  slow.socket.pause();
  // 7 × 512 KiB = 3.5 MiB: under the limit even if the kernel buffered none of it.
  const chunk = "y".repeat(512 * 1024);
  for (let sequence = 0; sequence < 7; sequence += 1) {
    hostSocket.send(JSON.stringify({ kind: "send", clientId: slow.id, text: JSON.stringify({ kind: "large", sequence, text: chunk }) }));
  }
  await new Promise((resolve) => setTimeout(resolve, 500));
  slow.socket.resume();
  for (let sequence = 0; sequence < 7; sequence += 1) {
    const large = await untilKind(slow.socket, "large");
    assert.equal(large.sequence, sequence);
    assert.equal(large.text.length, chunk.length);
  }
});

for (const mebibytes of [32, ...(process.env.HACHIDORI_STRESS ? [128] : [])]) {
  test(`the default limits do not trip on ${mebibytes} MiB messages`, { timeout: 180_000 }, async (t) => {
    const { port } = await relay(t);
    const hostSocket = await host(t, port);
    const client = await link(t, port, hostSocket);
    const paused = await link(t, port, hostSocket);
    paused.socket.pause();
    const big = "大".repeat(Math.floor((mebibytes * 1024 * 1024) / 3));
    const bytes = Buffer.byteLength(big);

    client.socket.sendRaw(frame(0x1, true, Buffer.from(big)));
    assert.equal(Buffer.byteLength((await untilKind(hostSocket, "client-text")).text), bytes);

    const reply = JSON.stringify({ kind: "reply", text: big });
    for (const clientId of [client.id, paused.id]) {
      hostSocket.sendRaw(frame(0x1, true, Buffer.from(JSON.stringify({ kind: "send", clientId, text: reply }))));
    }
    hostSocket.send(JSON.stringify({ kind: "broadcast", text: "{\"kind\":\"marker\"}" }));
    assert.equal(Buffer.byteLength((await untilKind(client.socket, "reply")).text), bytes);
    await untilKind(client.socket, "marker");
    // The paused client reads only now, after everything was queued for it.
    paused.socket.resume();
    assert.equal(Buffer.byteLength((await untilKind(paused.socket, "reply")).text), bytes);
    await untilKind(paused.socket, "marker");

    client.socket.send("after");
    assert.equal((await untilKind(hostSocket, "client-text")).text, "after", "nobody was dropped");
  });
}
