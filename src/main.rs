// SPDX-License-Identifier: GPL-3.0-or-later
//! `hachidori-relay`: the Hachidori sharing relay as a standalone process.
//!
//! The flags match upstream's `server.py`. The relay announces itself on stdout with
//! `listening <port>`, followed by `api <port>` or `api-failed <reason>` when the API
//! was asked for. With `--config`, it reads the add-on's `config.json` keys instead.
//! It then behaves like the Anki add-on: the API is on by default, and a relay port
//! that cannot be used is warned about once and retried every 10 s.

#![allow(clippy::print_stdout, reason = "stdout announcements are this binary's interface")]

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use hachidori_relay::{ApiStatus, Builder, DEFAULT_API_PORT, DEFAULT_PORT, Limits, Relay};
use serde_json::Value;
use tracing_subscriber::EnvFilter;

const RETRY: Duration = Duration::from_secs(10);

/// Run the Hachidori sharing relay without Anki.
#[derive(Debug, Parser)]
#[command(version, about)]
#[allow(clippy::struct_field_names, reason = "the fields are the flags")]
struct Args {
    /// Port to listen on; 0 picks a free one and prints it.
    #[arg(long, default_value_t = DEFAULT_PORT, conflicts_with = "config")]
    port: u16,

    /// Also serve the Yomitan-compatible HTTP API on this port; 0 picks a free one; omitted disables it.
    #[arg(long, conflicts_with = "config")]
    api_port: Option<u16>,

    /// Read `port` and `yomitan_api_port` from this JSON file (the Anki add-on's config).
    /// The API then defaults to 19633 (`null` turns it off), and a busy relay port is retried every 10 s.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Seconds between the keep-alive pings.
    #[arg(long, default_value_t = 20.0, value_parser = seconds)]
    ping_seconds: f64,

    /// How long the API waits for the sharing Hachidori; by default 30 s for lookups and 60 s for download chunks.
    #[arg(long, value_parser = seconds)]
    api_timeout_seconds: Option<f64>,

    /// Largest WebSocket upgrade request, in bytes.
    #[arg(long, value_name = "BYTES", default_value_t = Limits::default().max_handshake_bytes)]
    max_handshake_bytes: usize,

    /// Largest WebSocket message, in bytes.
    #[arg(long, value_name = "BYTES", default_value_t = Limits::default().max_message_bytes)]
    max_message_bytes: usize,

    /// Most bytes queued for one peer that stopped reading, before it is dropped.
    #[arg(long, value_name = "BYTES", default_value_t = Limits::default().max_queued_bytes)]
    max_queued_bytes: usize,

    /// Runtime worker threads; 1 runs everything on the main thread.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=256))]
    worker_threads: u16,

    /// Log filter for stderr, e.g. `info` or `hachidori_relay=debug` (overridden by `RUST_LOG`).
    #[arg(long, default_value = "warn")]
    log: String,
}

fn seconds(text: &str) -> Result<f64, String> {
    let value: f64 = text.parse().map_err(|error| format!("{error}"))?;
    if value.is_finite() && value > 0.0 && Duration::try_from_secs_f64(value).is_ok() {
        Ok(value)
    } else {
        Err("expected a positive number of seconds".to_owned())
    }
}

/// The Anki add-on's configuration keys, read as the add-on reads them:
/// `int(config["port"])`, and `int(api_port) if api_port else None` with 19633 when
/// the key is missing. So 0, `false`, `""` and `null` all turn the API off.
#[derive(Debug, PartialEq, Eq)]
struct Config {
    port: u16,
    yomitan_api_port: Option<u16>,
}

fn read_config(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    parse_config(&text).map_err(|error| format!("cannot use {}: {error}", path.display()))
}

fn parse_config(text: &str) -> Result<Config, String> {
    let config: serde_json::Map<String, Value> = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let port = match config.get("port") {
        None => DEFAULT_PORT,
        Some(value) => port_of(value).ok_or_else(|| format!("\"port\" is not a port: {value}"))?,
    };
    let yomitan_api_port = match config.get("yomitan_api_port") {
        None => Some(DEFAULT_API_PORT),
        Some(value) if is_falsy(value) => None,
        Some(value) => Some(port_of(value).ok_or_else(|| format!("\"yomitan_api_port\" is not a port: {value}"))?),
    };
    Ok(Config { port, yomitan_api_port })
}

/// Python's truthiness.
fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !flag,
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(fields) => fields.is_empty(),
    }
}

/// Python's `int(value)` for the types JSON has, limited to the port range.
fn port_of(value: &Value) -> Option<u16> {
    let number = match value {
        Value::Bool(flag) => i64::from(*flag),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                integer
            } else {
                // int() truncates a float toward zero.
                let float = number.as_f64()?.trunc();
                if !(0.0..=f64::from(u16::MAX)).contains(&float) {
                    return None;
                }
                // In range, so the conversion is exact.
                #[allow(clippy::cast_possible_truncation, reason = "checked against the port range above")]
                let integer = float as i64;
                integer
            }
        }
        Value::String(text) => text.trim().replace('_', "").parse().ok()?,
        _ => return None,
    };
    u16::try_from(number).ok()
}

fn main() -> ExitCode {
    let args = Args::parse();
    let filter = EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new(&args.log)).unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .init();

    let mut runtime =
        if args.worker_threads == 1 { tokio::runtime::Builder::new_current_thread() } else { tokio::runtime::Builder::new_multi_thread() };
    if args.worker_threads > 1 {
        runtime.worker_threads(usize::from(args.worker_threads));
    }
    match runtime.enable_all().thread_name("hachidori-relay").build() {
        Ok(runtime) => runtime.block_on(run(args)),
        Err(error) => {
            tracing::error!(%error, "cannot start the runtime");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> ExitCode {
    let (port, api_port, retry) = match &args.config {
        Some(path) => match read_config(path) {
            Ok(config) => (config.port, config.yomitan_api_port, true),
            Err(error) => {
                tracing::error!("{error}");
                return ExitCode::FAILURE;
            }
        },
        None => (args.port, args.api_port, false),
    };
    let mut builder =
        Relay::builder().port(port).api_port(api_port).ping_interval(Duration::from_secs_f64(args.ping_seconds)).limits(Limits {
            max_handshake_bytes: args.max_handshake_bytes,
            max_message_bytes: args.max_message_bytes,
            max_queued_bytes: args.max_queued_bytes,
            ..Limits::default()
        });
    if let Some(timeout) = args.api_timeout_seconds {
        let timeout = Duration::from_secs_f64(timeout);
        builder = builder.lookup_timeout(timeout).chunk_timeout(timeout);
    }

    let mut warned_relay = false;
    let mut warned_api = false;
    loop {
        match serve(builder.clone(), retry, &mut warned_api).await {
            Outcome::Stopped => return ExitCode::SUCCESS,
            Outcome::Failed(error) if retry => {
                // Another program holds the port, or the relay lost it. Say so once, then keep trying.
                if !warned_relay {
                    warned_relay = true;
                    tracing::warn!("{error}; retrying every {} s", RETRY.as_secs());
                }
                tokio::select! {
                    () = tokio::time::sleep(RETRY) => {}
                    () = shutdown_signal() => return ExitCode::SUCCESS,
                }
            }
            Outcome::Failed(error) => {
                tracing::error!("{error}");
                return ExitCode::FAILURE;
            }
        }
    }
}

enum Outcome {
    Stopped,
    Failed(hachidori_relay::Error),
}

async fn serve(builder: Builder, retry: bool, warned_api: &mut bool) -> Outcome {
    let mut relay = match builder.start() {
        Ok(relay) => relay,
        Err(error) => return Outcome::Failed(error),
    };
    println!("listening {}", relay.port());
    match relay.api() {
        ApiStatus::Disabled => {}
        ApiStatus::Listening(port) => println!("api {port}"),
        ApiStatus::Failed(error) => {
            println!("api-failed {error}");
            // The relay keeps running without the API. Say so once, naming the port.
            if retry && !*warned_api {
                *warned_api = true;
                tracing::warn!("the Yomitan API could not use its port: {error}; sharing still works");
            }
        }
    }
    tokio::select! {
        closed = relay.closed() => {
            relay.shutdown().await;
            closed.err().map_or(Outcome::Stopped, Outcome::Failed)
        }
        () = shutdown_signal() => {
            relay.shutdown().await;
            Outcome::Stopped
        }
    }
}

/// Ctrl-C everywhere, and SIGTERM on Unix.
async fn shutdown_signal() {
    let interrupt = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = interrupt => {}
            () = terminate => {}
        }
    }
    #[cfg(not(unix))]
    interrupt.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Result<Config, String> {
        parse_config(text)
    }

    #[test]
    fn config_reads_like_the_addon() {
        let both = |port, api| Ok(Config { port, yomitan_api_port: api });
        assert_eq!(config(r#"{"port": 8771, "yomitan_api_port": 19633}"#), both(8771, Some(19633)));
        // An older release's config has no API key: the API gets its default port.
        assert_eq!(config(r#"{"port": 9000}"#), both(9000, Some(19633)));
        for off in ["null", "0", "false", "\"\"", "0.0"] {
            assert_eq!(config(&format!(r#"{{"port": 8771, "yomitan_api_port": {off}}}"#)), both(8771, None), "{off}");
        }
        assert_eq!(config(r#"{"port": "8772", "yomitan_api_port": " 19634 "}"#), both(8772, Some(19634)), "int() takes strings");
        assert_eq!(config(r#"{"port": 8772.9}"#), both(8772, Some(19633)), "int() truncates");
        assert_eq!(config(r#"{"port": 8771, "other": [1, 2]}"#), both(8771, Some(19633)), "other keys are ignored");
        assert!(config(r#"{"port": 70000}"#).is_err());
        assert!(config(r#"{"port": "abc"}"#).is_err());
        assert!(config(r#"{"port": 8771, "yomitan_api_port": [1]}"#).is_err());
        assert!(config("[]").is_err());
    }
}
