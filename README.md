# hachidori-relay

A Rust reimplementation of the [Hachidori Anki add-on](https://github.com/bee-san/hachidori-anki)'s
sharing relay (upstream v0.0.6, commit `e614794`), as a standalone program for Windows and Linux,
and as a library that other programs can embed.

It speaks the same protocol: the `/host` and `/link` WebSocket endpoints on port 8771, the
host-controlled network switch, and the Yomitan-compatible HTTP API with streamed dictionary
downloads on port 19633. Upstream's own socket and HTTP test suites pass against it unchanged
(see [Testing](#testing)). It needs no Anki and no Python. An idle relay uses about 1 MB of
private memory, against 14 MB for upstream.

## Running

```sh
hachidori-relay                          # relay on 127.0.0.1:8771, API off (like server.py)
hachidori-relay --api-port 19633         # also serve the Yomitan API
hachidori-relay --config config.json     # read the add-on's config keys (see below)
```

The relay prints `listening <port>` on stdout, then `api <port>` or `api-failed <reason>` if
the API was asked for. Logs go to stderr, warnings only by default. Ctrl-C or SIGTERM
stops it.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--port` | 8771 | Relay port. `0` picks a free one. |
| `--api-port` | off | Yomitan API port. `0` picks a free one. |
| `--config FILE` | — | Read `port` (8771) and `yomitan_api_port` (19633, `null` turns it off) from an add-on `config.json`. A relay port that is busy is warned about once and retried every 10 s. Cannot be combined with `--port`/`--api-port`. The file may also set every other flag below, by its name with `_` (`ping_seconds`, `log`, `host`, `data_dir`, ...); a flag on the command line wins over it, and `null` leaves a key out. |
| `--ping-seconds` | 20 | Keep-alive ping interval. |
| `--api-timeout-seconds` | 30 / 60 | How long the API waits for the sharing Hachidori (lookups / download chunks). |
| `--max-handshake-bytes` | 65536 | Largest WebSocket upgrade request (over it: `431`). |
| `--max-message-bytes` | 268435456 | Largest WebSocket message (over it: that peer is closed). |
| `--max-queued-bytes` | 536870912 | Bytes queued for a peer that stopped reading before it is dropped. |
| `--worker-threads` | 2 | Runtime threads. `1` runs on one thread. |
| `--log` | `warn` | Log filter, e.g. `info` or `hachidori_relay=debug`. `RUST_LOG` overrides it. |

The relay listens on `127.0.0.1` until the sharing Hachidori turns network sharing on. To keep
it running, use your platform's supervisor (a systemd unit, Task Scheduler, NSSM, …).

### Differences from upstream

Behaviour is meant to be 1:1. The deliberate differences:

- It reports its own version: `/serverVersion` answers the crate version, and the API's
  hello says `hachidori-relay/<version>`.
- JSON replies are compact. Same meaning, different spacing.
- A non-string `text` in a host `send`/`broadcast` is forwarded as JSON text, not as a Python repr.
- Other edge cases (a missing `text` is forwarded as `null` rather than `None`, chunked request
  bodies are accepted, and so on).
- The safety limits above. Upstream has none. They are far above real traffic, are tested not
  to trip on 128 MiB messages, and log a warning when they are hit.

### Host mode

Built with `--features host`, `hachidori-relay --host` is itself the sharing Hachidori:
linked browsers use the configuration and dictionaries kept in a data directory
(`%APPDATA%\hachidori-relay`, or `$XDG_DATA_HOME/hachidori-relay`; `--data-dir` to choose).
A browser that connects as host is only mirrored into it: its settings as plain JSON, and
its dictionaries as Hoshidicts files. Everything persists across restarts, and `--network`
also admits linked browsers from the network. Lookups, for linked browsers and the HTTP
API, run on a native build of the extension's Hoshidicts engine (`git submodule update
--init --recursive`; it needs cmake and a C++17 compiler). It covers mirroring, serving the configuration, lookups, and the settings and
dictionary changes linked browsers make, custom dictionary and imports included, and Anki over
AnkiConnect: the duplicate index, word status, "open in Anki", the add check, and adding or
overwriting notes with their fields rendered from the Templates, dictionary images, pronunciations
and screenshots (also for the HTTP API's `ankiFields`), and managed dictionary updates with their
schedule, and installing the recommended dictionaries, both downloaded by the relay. [docs/host-mode.md](docs/host-mode.md)
describes it. It follows Hachidori 0.5.1
[`d41c37a`](https://github.com/bee-san/hachidori/commit/d41c37abe4ea54ef6ec885e9517bbf5655f3d718)
with language profiles: it hosts every language pack the extension ships, and
shares, mines and answers the HTTP API per profile, as a 0.5.1 browser host does.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--host` | off | Host mode. |
| `--data-dir DIR` | platform data dir | Where `storage.json` and `dictionaries/` live. |
| `--network` | off | Accept linked browsers from the network too. |
| `--write-delay SECONDS` | 5 | Write `storage.json` at most this often, with every change made meanwhile (0: each change at once). A new dictionary list is written at once, and what is pending on shutdown. |

In a config file these are `"host": true`, `"data_dir"` (a relative path is read from the file's
directory), `"network": true` and `"write_delay"`. [`relay.host.example.json`](relay.host.example.json) runs host
mode on the usual ports with its data in `host-data/` next to it:

```sh
cargo build --release --features host
target/release/hachidori-relay --config relay.host.example.json
```

It uses the Anki add-on's ports, 8771 and 19633, so the browser finds it where it finds the
add-on's relay: close Anki's relay first (or change both ports in the file and in the browser).
Connect a browser at Sharing to fill it from that browser's settings and dictionaries, then
link the others; AnkiConnect is reached at the URL in the shared Anki settings.

## As a library

```toml
hachidori-relay = { path = "../relay", default-features = false }
```

```rust
let mut relay = hachidori_relay::Relay::builder()
    .port(8771)
    .api_port(Some(19633))
    .version("chidoripop/0.2.0") // what the API hello reports
    .start()?;                   // binds now; must run inside a tokio runtime
println!("relay on {}", relay.port());
// relay.api() says whether the API is listening.
// relay.closed().await resolves if the relay loses its port.
relay.shutdown().await;
```

The library never prints and never exits the process. The `cli` feature (on by default) adds
only what the binary needs: clap, the log subscriber, and the multi-thread runtime and signals.

## Building

```sh
cargo build --release      # target/release/hachidori-relay
cargo build --release --features host   # with host mode
```

Rust 1.99 or later, x86_64 Windows (MSVC or GNU) or Linux.

## Testing

```sh
cargo test                 # unit tests, limit tests, and upstream's Node suites (needs Node 22+)
scripts/check.sh           # fmt, clippy -D warnings, tests, cargo audit
scripts/check.sh --upgrade # first cargo upgrade --incompatible and cargo update, then the same
```

`tests/conformance/` holds upstream's `sharing-relay.test.mjs` and `api.test.mjs` with their
fake host, changed only to start this binary and expect its version. Helpers that are not
from upstream are marked as such. `limits.test.mjs` and `tests/limits.rs` test the safety
limits. `host.test.mjs` and `src/host/tests.rs` test host mode, the first over sockets against
the binary and the second in process. `host-oracle.test.mjs`, `settings-oracle.test.mjs`,
`mutations-oracle.test.mjs`, `profiles-oracle.test.mjs`, `sharing-oracle.test.mjs`, `import-oracle.test.mjs` and `anki-oracle.test.mjs` compare host mode with the extension itself; they need a Hachidori checkout next to this one
(or `HACHIDORI_SOURCE`) and run with `node --test`, as do `anki-index.test.mjs` and `anki-index-languages.test.mjs`, which follow the
Anki duplicate index through relay restarts and language profiles with Hachidori's AnkiConnect fake. `anki-oracle.test.mjs` also needs jsdom (and `host-oracle.test.mjs` uses it for the API's Anki fields), found where
Hachidori's own tests look for it (`HACHIDORI_JSDOM`, Hachidori's `test/tooling`, or
`~/.cache/hachidori-e2e`), to render the extension's fields. `HACHIDORI_STRESS=1` (Node) or `cargo test --release --test limits -- --ignored` (Rust)
adds the 128 MiB run. `HACHIDORI_LIVE_TESTS=1` runs `updates-live.test.mjs`, which downloads Bee's
Ultimate Grammar Dictionary from GitHub (about 35 MB) to test managed updates and the recommended-dictionary installer against its real releases.

For manual runs next to a running Anki relay, use `relay.dev.json` (ports 8772/19634).

`docs/host-contract.md` is upstream's description of what a sharing Hachidori implements.

## Licence

GPL-3.0-or-later, like upstream.
