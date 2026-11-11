// A browser linked to a sharing host, as the oracles drive it: every frame it
// receives is kept in order, a close from the host marks it closed, and
// requests wait for the reply with their own id. The link runs over the
// relay's /link socket (`linkClient`) or over the in-memory relay that the
// extension's own sharing host talks to (`memoryRelay`).
//
// Not from upstream.
// SPDX-License-Identifier: GPL-3.0-or-later
import { connectClient, EXTENSION_ORIGIN, WAIT_MS } from "./relay-socket.mjs";

export const OPT_IN = "hd_language_profiles_opt_in";
export const PROFILES_CAPABILITY = "language-profiles-v1";

// A link over `transport`, `{ send(text), close(), attach({ text, closed }) }`,
// that says `hello` and waits for the host's.
async function startLink(transport, hello) {
  const frames = [];
  const waiters = new Set();
  let closed = false;
  let counter = 0;
  const wake = () => {
    for (const waiter of [...waiters]) waiter();
  };
  transport.attach({
    text(text) {
      frames.push(JSON.parse(text));
      wake();
    },
    closed() {
      closed = true;
      wake();
    },
  });
  // The first frame after `from` that `matches`, waited for.
  const until = (what, matches, from = 0, timeoutMs = WAIT_MS) => new Promise((resolveFound, rejectFound) => {
    const timer = setTimeout(() => {
      waiters.delete(check);
      rejectFound(new Error(`timed out waiting for ${what}`));
    }, timeoutMs);
    function check() {
      const found = frames.slice(from).find(matches);
      if (found === undefined && !closed) return;
      waiters.delete(check);
      clearTimeout(timer);
      if (found !== undefined) resolveFound(found);
      else rejectFound(new Error(`the host closed the link before ${what}`));
    }
    waiters.add(check);
    check();
  });
  const client = {
    frames,
    get closed() { return closed; },
    async whenClosed(timeoutMs = WAIT_MS) {
      const deadline = Date.now() + timeoutMs;
      while (!closed) {
        if (Date.now() > deadline) throw new Error("timed out waiting for the host to close the link");
        await new Promise((done) => setTimeout(done, 10));
      }
    },
    until,
    of: (kind) => frames.filter((frame) => frame.kind === kind),
    async send(message) {
      counter += 1;
      const id = `link-${counter}`;
      transport.send(JSON.stringify({ kind: "request", id, message }));
      return (await until(`the reply to ${message?.type}`, (frame) => frame.kind === "reply" && frame.id === id)).response;
    },
    close() { transport.close(); },
  };
  transport.send(JSON.stringify({ kind: "hello", protocol: 1, version: "0.9.0", name: "Firefox", capabilities: [], ...hello }));
  client.hello = await until("the hello", (frame) => frame.kind === "hello");
  return client;
}

// A link through the relay's /link socket. `hello` overrides the hello's fields.
export async function linkClient(t, port, { origin = EXTENSION_ORIGIN, hello = {} } = {}) {
  const socket = await connectClient(t, port, "/link", origin);
  return startLink({
    send: (text) => socket.send(text),
    close: () => socket.close(),
    attach(feed) {
      socket.onFrame((frame) => {
        if (frame.opcode === 0x8) feed.closed();
        else if (frame.opcode === 0x1) feed.text(frame.payload.toString("utf8"));
      });
      socket.onClose(() => feed.closed());
    },
  }, hello);
}

// The relay the extension's sharing host connects to, in memory: `WebSocket`
// goes into the background, `link` links a browser to that host. Every frame
// is delivered in order on a later turn, as through a socket.
export function memoryRelay() {
  let host = null;
  let next = 0;
  const clients = new Map();
  const later = (callback) => setTimeout(callback, 0);
  const toHost = (frame) => later(() => host?.onmessage?.({ data: JSON.stringify(frame) }));
  const drop = (clientId) => {
    const client = clients.get(clientId);
    if (client === undefined) return;
    clients.delete(clientId);
    later(() => {
      client.feed.closed();
      toHost({ kind: "client-close", clientId });
    });
  };
  class HostSocket {
    constructor(url) {
      this.url = url;
      this.readyState = 0;
      host = this;
      later(() => {
        this.readyState = 1;
        this.onopen?.();
        toHost({ kind: "listening", port: 8771 });
      });
    }

    send(text) {
      const frame = JSON.parse(text);
      if (frame.kind === "send") {
        const client = clients.get(frame.clientId);
        if (client) later(() => client.feed.text(frame.text));
      } else if (frame.kind === "broadcast") {
        for (const client of clients.values()) later(() => client.feed.text(frame.text));
      } else if (frame.kind === "close") {
        drop(frame.clientId);
      } else if (frame.kind === "network") {
        toHost({ kind: "network", enabled: frame.enabled, addresses: [] });
      }
    }

    close() {
      this.readyState = 3;
      if (host === this) host = null;
      later(() => this.onclose?.({}));
    }
  }
  return {
    WebSocket: HostSocket,
    connected: () => host !== null && host.readyState === 1,
    async link(_t, { origin = EXTENSION_ORIGIN, hello = {} } = {}) {
      const clientId = `client-${++next}`;
      toHost({ kind: "client-open", clientId, origin, address: "127.0.0.1" });
      return startLink({
        send: (text) => toHost({ kind: "client-text", clientId, text }),
        close: () => drop(clientId),
        attach(feed) { clients.set(clientId, { feed }); },
      }, hello);
    },
  };
}

// A profile-aware browser, through `link(t, options)`: it opts in with
// `languages` when the host offers profiles. `optIn` is the reply, or null.
export async function awareClient(t, link, languages, options = {}) {
  const client = await link(t, { ...options, hello: { capabilities: [PROFILES_CAPABILITY], ...options.hello } });
  client.optIn = client.hello.capabilities?.includes(PROFILES_CAPABILITY)
    ? await client.send({ target: "hoshidicts-worker", type: OPT_IN, requestId: "opt-in", languages })
    : null;
  return client;
}
