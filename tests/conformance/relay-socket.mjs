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
    next(what = "item", timeoutMs = 5000) {
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
  const closed = new Promise((resolveClose) => socket.once("close", resolveClose));
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
    closed,
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
    async ping(text) {
      socket.write(encodeClientFrame(0x9, Buffer.from(text, "utf8")));
      let frame = await frames.next("a pong frame");
      while (frame.opcode !== 0xA) frame = await frames.next("a pong frame");
      assert.equal(frame.payload.toString("utf8"), text);
    },
    async json(what = "a frame") {
      const frame = await frames.next(what);
      assert.equal(frame.opcode, 0x1, `expected a text frame, got opcode ${frame.opcode}`);
      return JSON.parse(frame.payload.toString("utf8"));
    },
    async closeFrame() {
      let frame = await frames.next("a close frame");
      while (frame.opcode !== 0x8) frame = await frames.next("a close frame");
      return frame;
    },
    close() { socket.write(encodeClientFrame(0x8, Buffer.alloc(0))); },
    destroy() { socket.destroy(); },
  };
}

export async function untilKind(client, kind) {
  let frame = await client.json(`a ${kind} frame`);
  while (frame.kind !== kind) frame = await client.json(`a ${kind} frame`);
  return frame;
}
