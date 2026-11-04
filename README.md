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
| `--config FILE` | — | Read `port` (8771) and `yomitan_api_port` (19633, `null` turns it off) from an add-on `config.json`. A relay port that is busy is warned about once and retried every 10 s. Cannot be combined with `--port`/`--api-port`. |
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
```

Rust 1.88 or later, x86_64 Windows (MSVC or GNU) or Linux.

## Testing

```sh
cargo test                 # unit tests, limit tests, and upstream's Node suites (needs Node 22+)
scripts/check.sh           # fmt, clippy -D warnings, tests, cargo audit
scripts/check.sh --upgrade # first cargo upgrade --incompatible and cargo update, then the same
```

`tests/conformance/` holds upstream's `sharing-relay.test.mjs` and `api.test.mjs` with their
fake host, changed only to start this binary and expect its version. Helpers that are not
from upstream are marked as such. `limits.test.mjs` and `tests/limits.rs` test the safety
limits. `HACHIDORI_STRESS=1` (Node) or `cargo test --release --test limits -- --ignored` (Rust)
adds the 128 MiB run.

For manual runs next to a running Anki relay, use `relay.dev.json` (ports 8772/19634).

`docs/host-contract.md` is upstream's description of what a sharing Hachidori implements.

## Licence

GPL-3.0-or-later, like upstream.
