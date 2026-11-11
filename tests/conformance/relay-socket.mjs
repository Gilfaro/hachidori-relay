// A minimal WebSocket client over raw sockets, shared by the relay tests and
// the fake host. Node's built-in client cannot set the Origin header the relay
// checks.
// SPDX-License-Identifier: GPL-3.0-or-later
import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { connect } from "node:net";

export const EXTENSION_ORIGIN = "chrome-extension://hachidorirelaytestextensionid";
// Firefox gives each installation of an extension its own UUID origin.
export const FIREFOX_ORIGIN = "moz-extension://5a1d4c2e-8f3b-4e6a-9c7d-0b1e2f3a4b5c";
export const HOSHIDICTS_ORIGIN = "hoshi://hoshidicts";

// Not in upstream: timing for loaded CI runners (macOS ones especially), where a fixed
// sleep or a short wait loses races that a fast machine always wins. `WAIT_MS` bounds
// every wait for something the relay should do, measured over the whole wait: the
// relay's pings must not restart it, or a missing event only shows as the test's own
// timeout. `SLOW_TEST` is the timeout for tests that move tens of MiB.
export const WAIT_MS = 30_000;
export const SLOW_TEST = { timeout: 180_000 };
const sleep = (ms) => new Promise((resolveSleep) => setTimeout(resolveSleep, ms));

// Rejects with `timed out waiting for <what>` if `promise` has not settled in `timeoutMs`.
export function within(what, promise, timeoutMs = WAIT_MS) {
  let timer;
  const late = new Promise((_, rejectLate) => { timer = setTimeout(() => rejectLate(new Error(`timed out waiting for ${what}`)), timeoutMs); });
  return Promise.race([promise, late]).finally(() => clearTimeout(timer));
}

// Retries `check` every 20 ms until it resolves, instead of sleeping a fixed time and
// hoping the relay got there. Its last failure is rethrown at the deadline.
export async function eventually(what, check, timeoutMs = WAIT_MS) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      return await check();
    } catch (error) {
      if (Date.now() > deadline) throw new Error(`${what} never happened: ${error.message}`, { cause: error });
    }
    await sleep(20);
  }
}

export function queue() {
  const items = [];
  const waiters = [];
  return {
    push(item) {
      const waiter = waiters.shift();
      if (waiter) waiter(item);
      else items.push(item);
    },
    drain() { return items.splice(0, items.length); },
    next(what = "item", timeoutMs = WAIT_MS) {
      if (items.length > 0) return Promise.resolve(items.shift());
      return new Promise((resolveNext, rejectNext) => {
        const timer = setTimeout(() => rejectNext(new Error(`timed out waiting for ${what}`)), timeoutMs);
        waiters.push((item) => { clearTimeout(timer); resolveNext(item); });
      });
    },
  };
}

export function encodeClientFrame(opcode, payload) {
  const mask = randomBytes(4);
  let header;
  if (payload.length <= 125) header = Buffer.from([0x80 | opcode, 0x80 | payload.length]);
  else if (payload.length <= 0xffff) {
    header = Buffer.alloc(4);
    header[0] = 0x80 | opcode;
    header[1] = 0x80 | 126;
    header.writeUInt16BE(payload.length, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x80 | opcode;
    header[1] = 0x80 | 127;
    header.writeBigUInt64BE(BigInt(payload.length), 2);
  }
  const masked = Buffer.from(payload);
  for (let index = 0; index < masked.length; index += 1) masked[index] ^= mask[index % 4];
  return Buffer.concat([header, mask, masked]);
}

export function readServerFrame(buffer) {
  if (buffer.length < 2) return null;
  const opcode = buffer[0] & 0x0f;
  let length = buffer[1] & 0x7f;
  let offset = 2;
  if (length === 126) {
    if (buffer.length < 4) return null;
    length = buffer.readUInt16BE(2);
    offset = 4;
  } else if (length === 127) {
    if (buffer.length < 10) return null;
    length = Number(buffer.readBigUInt64BE(2));
    offset = 10;
  }
  if (buffer.length < offset + length) return null;
  return { opcode, payload: buffer.subarray(offset, offset + length), length: offset + length };
}

// The whole length of the frame starting `buffer`, once its header is in; 0 before.
function frameLength(buffer) {
  if (buffer.length < 2) return 0;
  const length = buffer[1] & 0x7f;
  if (length === 126) return buffer.length < 4 ? 0 : 4 + buffer.readUInt16BE(2);
  if (length === 127) return buffer.length < 10 ? 0 : 10 + Number(buffer.readBigUInt64BE(2));
  return 2 + length;
}

// `t` is a node:test context whose after() hook destroys the socket, or null.
export async function connectClient(t, port, path, origin = EXTENSION_ORIGIN, host = "127.0.0.1") {
  const socket = connect(port, host);
  if (t) t.after(() => socket.destroy());
  await new Promise((resolveConnect, rejectConnect) => {
    socket.once("connect", resolveConnect);
    socket.once("error", rejectConnect);
  });
  const key = randomBytes(16).toString("base64");
  socket.write([
    `GET ${path} HTTP/1.1`, `Host: ${host}:${port}`, "Upgrade: websocket", "Connection: Upgrade",
    `Sec-WebSocket-Key: ${key}`, "Sec-WebSocket-Version: 13", ...(origin === null ? [] : [`Origin: ${origin}`]), "", "",
  ].join("\r\n"));
  const frames = queue();
  const listeners = [];
  const closedOnce = new Promise((resolveClose) => socket.once("close", resolveClose));
  let buffer = Buffer.alloc(0);
  let status = null;
  // Not in upstream: chunks wait here until the frame they belong to is complete, so a
  // large frame is concatenated once rather than once per chunk (quadratic at 128 MiB).
  let pending = [];
  let pendingLength = 0;
  let needed = 0;
  const head = new Promise((resolveHead) => {
    socket.on("data", (chunk) => {
      pending.push(chunk);
      pendingLength += chunk.length;
      if (buffer.length + pendingLength < needed) return;
      buffer = Buffer.concat([buffer, ...pending]);
      pending = [];
      pendingLength = 0;
      needed = 0;
      if (status === null) {
        const end = buffer.indexOf("\r\n\r\n");
        if (end === -1) return;
        status = Number(buffer.toString("latin1", 0, end).split(" ")[1]);
        buffer = buffer.subarray(end + 4);
        resolveHead(status);
      }
      while (true) {
        const frame = readServerFrame(buffer);
        if (frame === null) {
          needed = frameLength(buffer);
          return;
        }
        buffer = buffer.subarray(frame.length);
        if (listeners.length > 0) for (const listener of listeners) listener(frame);
        else frames.push(frame);
      }
    });
  });
  socket.on("error", () => {});
  await head;
  return {
    status,
    // A getter, so that the deadline starts when a test begins to wait.
    get closed() { return within("the socket to close", closedOnce); },
    // Not in upstream: runs `listener` once the socket has closed, however long that takes.
    onClose(listener) { closedOnce.then(listener); },
    pause() { socket.pause(); },
    resume() { socket.resume(); },
    // Routes every frame to `listener` instead of the queue (for the fake host's event loop).
    onFrame(listener) {
      listeners.push(listener);
      for (const frame of frames.drain()) listener(frame);
    },
    send(text) { socket.write(encodeClientFrame(0x1, Buffer.from(text, "utf8"))); },
    // Not in upstream: writes already-encoded bytes (for the limit tests).
    sendRaw(bytes) { socket.write(bytes); },
    async ping(text, timeoutMs = WAIT_MS) {
      socket.write(encodeClientFrame(0x9, Buffer.from(text, "utf8")));
      const deadline = Date.now() + timeoutMs;
      let frame = await frames.next("a pong frame", deadline - Date.now());
      while (frame.opcode !== 0xA) frame = await frames.next("a pong frame", deadline - Date.now());
      assert.equal(frame.payload.toString("utf8"), text);
    },
    async json(what = "a frame", timeoutMs = undefined) {
      const frame = await frames.next(what, timeoutMs);
      assert.equal(frame.opcode, 0x1, `expected a text frame, got opcode ${frame.opcode}`);
      return JSON.parse(frame.payload.toString("utf8"));
    },
    async closeFrame(timeoutMs = WAIT_MS) {
      const deadline = Date.now() + timeoutMs;
      let frame = await frames.next("a close frame", deadline - Date.now());
      while (frame.opcode !== 0x8) frame = await frames.next("a close frame", deadline - Date.now());
      return frame;
    },
    close() { socket.write(encodeClientFrame(0x8, Buffer.alloc(0))); },
    destroy() { socket.destroy(); },
  };
}

export async function untilKind(client, kind, timeoutMs = WAIT_MS) {
  const deadline = Date.now() + timeoutMs;
  let frame = await client.json(`a ${kind} frame`, deadline - Date.now());
  while (frame.kind !== kind) frame = await client.json(`a ${kind} frame`, deadline - Date.now());
  return frame;
}

// Not in upstream: connects a host and reads its `listening` frame (returned as
// `listening`). A test that has just dropped the previous host can get here before the
// relay has noticed; the relay then turns this one away as a second host, so it retries.
// Tests of that refusal itself connect with `connectClient`.
export async function connectHost(t, port, origin = EXTENSION_ORIGIN) {
  return eventually("the relay taking a new host", async () => {
    const socket = await connectClient(t, port, "/host", origin);
    assert.equal(socket.status, 101);
    const first = await socket.json("the listening frame");
    if (first.kind !== "listening") {
      socket.destroy();
      assert.equal(first.kind, "listening", JSON.stringify(first));
    }
    return Object.assign(socket, { listening: first });
  });
}
