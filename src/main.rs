// SPDX-License-Identifier: GPL-3.0-or-later
//! `hachidori-relay`: the Hachidori sharing relay as a standalone process.
//!
//! The flags match upstream's `server.py`. The relay announces itself on stdout with
//! `listening <port>`, followed by `api <port>` or `api-failed <reason>` when the API
//! was asked for. With `--config`, it reads the add-on's `config.json` keys instead,
//! and behaves like the Anki add-on: the API is on by default, and a relay port that
//! cannot be used is warned about once and retried every 10 s. The same file may set
//! every other flag under its own name (`ping_seconds`, `host`, `data_dir`, ...); a
//! flag given on the command line wins over the file.

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
const DEFAULT_PING_SECONDS: f64 = 20.0;
const DEFAULT_WORKER_THREADS: u16 = 2;
const MAX_WORKER_THREADS: i64 = 256;
const DEFAULT_LOG: &str = "warn";
/// Host mode writes its storage at most this often.
const DEFAULT_WRITE_DELAY: f64 = 5.0;

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
    /// The file may also set every other flag, by its name with `_` (`"host": true`); a flag wins over it.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Seconds between the keep-alive pings [default: 20].
    #[arg(long, value_parser = seconds)]
    ping_seconds: Option<f64>,

    /// How long the API waits for the sharing Hachidori; by default 30 s for lookups and 60 s for download chunks.
    #[arg(long, value_parser = seconds)]
    api_timeout_seconds: Option<f64>,

    /// Largest WebSocket upgrade request, in bytes [default: 65536].
    #[arg(long, value_name = "BYTES")]
    max_handshake_bytes: Option<usize>,

    /// Largest WebSocket message, in bytes [default: 268435456].
    #[arg(long, value_name = "BYTES")]
    max_message_bytes: Option<usize>,

    /// Most bytes queued for one peer that stopped reading, before it is dropped [default: 536870912].
    #[arg(long, value_name = "BYTES")]
    max_queued_bytes: Option<usize>,

    /// Runtime worker threads; 1 runs everything on the main thread [default: 2].
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=MAX_WORKER_THREADS))]
    worker_threads: Option<u16>,

    /// Log filter for stderr, e.g. `info` or `hachidori_relay=debug` (overridden by `RUST_LOG`) [default: warn].
    #[arg(long)]
    log: Option<String>,

    /// Host mode: the relay is itself the sharing Hachidori. Linked browsers use the
    /// configuration and dictionaries kept in the data directory, and a browser that
    /// connects as host is only mirrored into it.
    #[cfg(feature = "host")]
    #[arg(long)]
    host: bool,

    /// Host mode's data directory. Default: %APPDATA%\hachidori-relay on Windows,
    /// $XDG_DATA_HOME/hachidori-relay (~/.local/share/hachidori-relay) elsewhere.
    /// In a config file, a relative `data_dir` is read from the file's directory.
    #[cfg(feature = "host")]
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// Host mode: also accept linked browsers from the network, not only from this computer.
    #[cfg(feature = "host")]
    #[arg(long)]
    network: bool,

    /// Host mode: write storage.json at most once per this many seconds, with every
    /// change made meanwhile (default 5). 0 writes each change at once. A change of
    /// the dictionary list is always written at once.
    #[cfg(feature = "host")]
    #[arg(long, value_name = "SECONDS", value_parser = delay_seconds)]
    write_delay: Option<f64>,
}

impl Args {
    /// `--host`, `--data-dir`, `--network` and `--write-delay`, in a build with or
    /// without host mode.
    #[cfg_attr(not(feature = "host"), allow(clippy::unused_self, reason = "the flags exist only in host mode"))]
    fn host_flags(&self) -> (bool, Option<PathBuf>, bool, Option<f64>) {
        #[cfg(feature = "host")]
        return (self.host, self.data_dir.clone(), self.network, self.write_delay);
        #[cfg(not(feature = "host"))]
        (false, None, false, None)
    }
}

fn seconds(text: &str) -> Result<f64, String> {
    positive_seconds(text.parse().map_err(|error| format!("{error}"))?)
}

#[cfg_attr(not(feature = "host"), allow(dead_code, reason = "parses a host-mode flag"))]
fn delay_seconds(text: &str) -> Result<f64, String> {
    non_negative_seconds(text.parse().map_err(|error| format!("{error}"))?)
}

fn non_negative_seconds(value: f64) -> Result<f64, String> {
    if value == 0.0 { Ok(0.0) } else { positive_seconds(value).map_err(|_| "expected 0 or a positive number of seconds".to_owned()) }
}

fn positive_seconds(value: f64) -> Result<f64, String> {
    if value.is_finite() && value > 0.0 && Duration::try_from_secs_f64(value).is_ok() {
        Ok(value)
    } else {
        Err("expected a positive number of seconds".to_owned())
    }
}

/// The Anki add-on's configuration keys, read as the add-on reads them:
/// `int(config["port"])`, and `int(api_port) if api_port else None` with 19633 when
/// the key is missing. So 0, `false`, `""` and `null` all turn the API off. The other
/// flags are optional keys of their own, which the add-on ignores; `null` leaves one out.
#[derive(Debug, PartialEq, Default)]
struct Config {
    port: u16,
    yomitan_api_port: Option<u16>,
    ping_seconds: Option<f64>,
    api_timeout_seconds: Option<f64>,
    max_handshake_bytes: Option<usize>,
    max_message_bytes: Option<usize>,
    max_queued_bytes: Option<usize>,
    worker_threads: Option<u16>,
    log: Option<String>,
    host: Option<bool>,
    data_dir: Option<PathBuf>,
    network: Option<bool>,
    write_delay: Option<f64>,
}

fn read_config(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut config = parse_config(&text).map_err(|error| format!("cannot use {}: {error}", path.display()))?;
    // A relative data directory is next to the config file, wherever the relay was started.
    if let (Some(dir), Some(parent)) = (config.data_dir.as_mut(), path.parent())
        && dir.is_relative()
    {
        *dir = parent.join(&*dir);
    }
    Ok(config)
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
    // An optional key: `None` when missing or null, else `read` of it or an error naming the key.
    let optional = |name: &str, what: &str, read: &dyn Fn(&Value) -> Option<Value>| -> Result<Option<Value>, String> {
        match config.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => read(value).map(Some).ok_or_else(|| format!("\"{name}\" is not {what}: {value}")),
        }
    };
    let seconds = |name: &str| -> Result<Option<f64>, String> {
        let read = |value: &Value| value.as_f64().and_then(|value| positive_seconds(value).ok()).map(Value::from);
        Ok(optional(name, "a positive number of seconds", &read)?.and_then(|value| value.as_f64()))
    };
    let delay = |name: &str| -> Result<Option<f64>, String> {
        let read = |value: &Value| value.as_f64().and_then(|value| non_negative_seconds(value).ok()).map(Value::from);
        Ok(optional(name, "0 or a positive number of seconds", &read)?.and_then(|value| value.as_f64()))
    };
    let bytes = |name: &str| -> Result<Option<usize>, String> {
        let read = |value: &Value| value.as_u64().filter(|&bytes| bytes > 0 && usize::try_from(bytes).is_ok()).map(Value::from);
        Ok(optional(name, "a positive number of bytes", &read)?
            .and_then(|value| value.as_u64())
            .and_then(|bytes| usize::try_from(bytes).ok()))
    };
    let flag = |name: &str| -> Result<Option<bool>, String> {
        Ok(optional(name, "true or false", &|value| value.as_bool().map(Value::from))?.and_then(|value| value.as_bool()))
    };
    let text = |name: &str| -> Result<Option<String>, String> {
        let read = |value: &Value| value.as_str().filter(|text| !text.is_empty()).map(Value::from);
        Ok(optional(name, "a non-empty text", &read)?.and_then(|value| value.as_str().map(str::to_owned)))
    };
    let threads = |value: &Value| value.as_i64().filter(|threads| (1..=MAX_WORKER_THREADS).contains(threads)).map(Value::from);
    let worker_threads = optional("worker_threads", &format!("1 to {MAX_WORKER_THREADS} threads"), &threads)?
        .and_then(|value| value.as_i64())
        .and_then(|threads| u16::try_from(threads).ok());
    Ok(Config {
        port,
        yomitan_api_port,
        ping_seconds: seconds("ping_seconds")?,
        api_timeout_seconds: seconds("api_timeout_seconds")?,
        max_handshake_bytes: bytes("max_handshake_bytes")?,
        max_message_bytes: bytes("max_message_bytes")?,
        max_queued_bytes: bytes("max_queued_bytes")?,
        worker_threads,
        log: text("log")?,
        host: flag("host")?,
        data_dir: text("data_dir")?.map(PathBuf::from),
        network: flag("network")?,
        write_delay: delay("write_delay")?,
    })
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

/// What the relay runs with: the flags, over the config file, over the defaults.
#[derive(Debug)]
struct Settings {
    port: u16,
    api_port: Option<u16>,
    /// A busy relay port is retried (with `--config`, as the add-on does).
    retry: bool,
    ping_seconds: f64,
    api_timeout_seconds: Option<f64>,
    limits: Limits,
    worker_threads: u16,
    log: String,
    /// Host mode: its data directory, if one was chosen, whether it admits the
    /// network, and its write delay in seconds.
    #[cfg_attr(not(feature = "host"), allow(dead_code, reason = "read only by host mode"))]
    host: Option<(Option<PathBuf>, bool, f64)>,
}

/// The flags over `config`, and the defaults under both.
fn resolve(args: &Args, config: Option<Config>) -> Result<Settings, String> {
    let (port, api_port, retry) = match &config {
        Some(config) => (config.port, config.yomitan_api_port, true),
        None => (args.port, args.api_port, false),
    };
    let config = config.unwrap_or_default();
    let (host_flag, data_dir_flag, network_flag, write_delay_flag) = args.host_flags();
    let host = host_flag || config.host == Some(true);
    let data_dir = data_dir_flag.or(config.data_dir);
    let network = network_flag || config.network == Some(true);
    let write_delay = write_delay_flag.or(config.write_delay);
    if !host && (data_dir.is_some() || network || write_delay.is_some()) {
        return Err("a data directory, network access and a write delay apply only in host mode: add --host (or \"host\": true)".to_owned());
    }
    if host && !cfg!(feature = "host") {
        return Err("this build has no host mode: build it with --features host".to_owned());
    }
    let defaults = Limits::default();
    Ok(Settings {
        port,
        api_port,
        retry,
        ping_seconds: args.ping_seconds.or(config.ping_seconds).unwrap_or(DEFAULT_PING_SECONDS),
        api_timeout_seconds: args.api_timeout_seconds.or(config.api_timeout_seconds),
        limits: Limits {
            max_handshake_bytes: args.max_handshake_bytes.or(config.max_handshake_bytes).unwrap_or(defaults.max_handshake_bytes),
            max_message_bytes: args.max_message_bytes.or(config.max_message_bytes).unwrap_or(defaults.max_message_bytes),
            max_queued_bytes: args.max_queued_bytes.or(config.max_queued_bytes).unwrap_or(defaults.max_queued_bytes),
            ..defaults
        },
        worker_threads: args.worker_threads.or(config.worker_threads).unwrap_or(DEFAULT_WORKER_THREADS),
        log: args.log.clone().or(config.log).unwrap_or_else(|| DEFAULT_LOG.to_owned()),
        host: host.then_some((data_dir, network, write_delay.unwrap_or(DEFAULT_WRITE_DELAY))),
    })
}

fn main() -> ExitCode {
    let args = Args::parse();
    let settings = args.config.as_deref().map(read_config).transpose().and_then(|config| resolve(&args, config));
    let log = settings.as_ref().map_or(DEFAULT_LOG, |settings| settings.log.as_str());
    let filter = EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new(log)).unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .init();
    let settings = match settings {
        Ok(settings) => settings,
        Err(error) => {
            tracing::error!("{error}");
            return ExitCode::FAILURE;
        }
    };

    let mut runtime = if settings.worker_threads == 1 {
        tokio::runtime::Builder::new_current_thread()
    } else {
        tokio::runtime::Builder::new_multi_thread()
    };
    if settings.worker_threads > 1 {
        runtime.worker_threads(usize::from(settings.worker_threads));
    }
    match runtime.enable_all().thread_name("hachidori-relay").build() {
        Ok(runtime) => runtime.block_on(run(settings)),
        Err(error) => {
            tracing::error!(%error, "cannot start the runtime");
            ExitCode::FAILURE
        }
    }
}

async fn run(settings: Settings) -> ExitCode {
    let retry = settings.retry;
    let mut builder = Relay::builder()
        .port(settings.port)
        .api_port(settings.api_port)
        .ping_interval(Duration::from_secs_f64(settings.ping_seconds))
        .limits(settings.limits);
    if let Some(timeout) = settings.api_timeout_seconds {
        let timeout = Duration::from_secs_f64(timeout);
        builder = builder.lookup_timeout(timeout).chunk_timeout(timeout);
    }
    #[cfg(feature = "host")]
    if let Some((data_dir, network, write_delay)) = settings.host {
        let Some(data_dir) = data_dir.or_else(hachidori_relay::HostOptions::default_data_dir) else {
            tracing::error!("no data directory: pass --data-dir (or \"data_dir\")");
            return ExitCode::FAILURE;
        };
        let mut options = hachidori_relay::HostOptions::new(data_dir);
        options.network = network;
        options.write_delay = Duration::from_secs_f64(write_delay);
        builder = builder.host(Some(options));
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
    // Closing the console window, logging off or shutting down Windows: Windows ends
    // the process soon after, so this is a best effort to save host mode's storage.
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_close, ctrl_logoff, ctrl_shutdown};
        let close = async {
            match ctrl_close() {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        let logoff = async {
            match ctrl_logoff() {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        let shutdown = async {
            match ctrl_shutdown() {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = interrupt => {}
            () = close => {}
            () = logoff => {}
            () = shutdown => {}
        }
    }
    #[cfg(not(any(unix, windows)))]
    interrupt.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Result<Config, String> {
        parse_config(text)
    }

    fn args(extra: &[&str]) -> Args {
        Args::parse_from([&["hachidori-relay"][..], extra].concat())
    }

    #[test]
    fn config_reads_like_the_addon() {
        let both = |port, api| Ok(Config { port, yomitan_api_port: api, ..Config::default() });
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

    const EVERYTHING: &str = r#"{"port": 8771, "yomitan_api_port": 19633, "ping_seconds": 5, "api_timeout_seconds": 90.5,
        "max_handshake_bytes": 1000, "max_message_bytes": 2000, "max_queued_bytes": 3000, "worker_threads": 4,
        "log": "info", "host": true, "data_dir": "data", "network": true, "write_delay": 0}"#;

    #[test]
    fn config_sets_every_other_flag() {
        let expected = Config {
            port: 8771,
            yomitan_api_port: Some(19633),
            ping_seconds: Some(5.0),
            api_timeout_seconds: Some(90.5),
            max_handshake_bytes: Some(1000),
            max_message_bytes: Some(2000),
            max_queued_bytes: Some(3000),
            worker_threads: Some(4),
            log: Some("info".to_owned()),
            host: Some(true),
            data_dir: Some(PathBuf::from("data")),
            network: Some(true),
            write_delay: Some(0.0),
        };
        assert_eq!(config(EVERYTHING), Ok(expected));
        // `null` is the same as leaving a key out.
        let plain = Config { port: DEFAULT_PORT, yomitan_api_port: Some(DEFAULT_API_PORT), ..Config::default() };
        assert_eq!(config(r#"{"ping_seconds": null, "host": null}"#), Ok(plain));
        for bad in [
            r#"{"ping_seconds": 0}"#,
            r#"{"ping_seconds": "5"}"#,
            r#"{"max_message_bytes": -1}"#,
            r#"{"max_message_bytes": 0}"#,
            r#"{"worker_threads": 0}"#,
            r#"{"worker_threads": 257}"#,
            r#"{"host": "yes"}"#,
            r#"{"log": ""}"#,
            r#"{"data_dir": 3}"#,
            r#"{"write_delay": -1}"#,
        ] {
            assert!(config(bad).is_err(), "{bad}");
        }
        assert_eq!(config(r#"{"host": "yes"}"#), Err(r#""host" is not true or false: "yes""#.to_owned()));
    }

    #[test]
    fn flags_win_over_the_config_and_defaults_fill_the_rest() {
        let everything = || config(EVERYTHING).ok();
        // The same file without its host-mode keys, for a build with or without host mode.
        let relay_only =
            config(EVERYTHING).ok().map(|config| Config { host: None, data_dir: None, network: None, write_delay: None, ..config });
        let settings = resolve(&args(&["--config", "c.json", "--ping-seconds", "7", "--log", "debug"]), relay_only);
        let settings = settings.unwrap_or_else(|error| panic!("{error}"));
        assert_eq!((settings.port, settings.api_port, settings.retry), (8771, Some(19633), true));
        assert_eq!((settings.ping_seconds, settings.api_timeout_seconds, settings.worker_threads), (7.0, Some(90.5), 4));
        assert_eq!(settings.log, "debug");
        let limits = settings.limits;
        assert_eq!((limits.max_handshake_bytes, limits.max_message_bytes, limits.max_queued_bytes), (1000, 2000, 3000));
        assert_eq!(settings.host, None);
        if cfg!(feature = "host") {
            let host = resolve(&args(&["--config", "c.json"]), everything()).map(|settings| settings.host);
            assert_eq!(host, Ok(Some((Some(PathBuf::from("data")), true, 0.0))));
            let host = resolve(&args(&["--config", "c.json", "--write-delay", "2.5"]), everything()).map(|settings| settings.host);
            assert_eq!(host, Ok(Some((Some(PathBuf::from("data")), true, 2.5))), "the flag wins");
            let host = resolve(&args(&["--host"]), None).map(|settings| settings.host);
            assert_eq!(host, Ok(Some((None, false, DEFAULT_WRITE_DELAY))));
        } else {
            assert!(resolve(&args(&["--config", "c.json"]), everything()).is_err(), "host mode needs the host feature");
        }

        // Without a file, everything has its default.
        let plain = resolve(&args(&[]), None).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            (plain.port, plain.api_port, plain.retry, plain.ping_seconds, plain.worker_threads),
            (DEFAULT_PORT, None, false, 20.0, 2)
        );
        assert_eq!((plain.log.as_str(), plain.limits, plain.host), ("warn", Limits::default(), None));
        // Host-mode settings without host mode are refused.
        assert!(resolve(&args(&[]), config(r#"{"network": true}"#).ok()).is_err());
        assert!(resolve(&args(&[]), config(r#"{"write_delay": 1}"#).ok()).is_err());
    }
}
