// SPDX-License-Identifier: GPL-3.0-or-later
//! The Hachidori sharing relay and its Yomitan-compatible HTTP API.
//!
//! A browser extension cannot listen for connections, so a sharing Hachidori
//! (the *host*) and the browsers linked to it all connect out to this relay:
//! the host on `/host`, linked browsers on `/link`. The relay forwards text
//! frames between them without reading them. It listens on this computer only
//! until the host asks for the network.
//!
//! Beside the relay, an HTTP listener answers the API of
//! [Kuuuube/yomitan-api](https://github.com/Kuuuube/yomitan-api) and serves
//! dictionary downloads. It never reads a dictionary itself. Each question goes
//! to the host over the relay, through an in-process client, and the host answers
//! as it would any linked browser. `docs/host-contract.md` specifies those frames.
//!
//! This is a reimplementation of `bee-san/hachidori-anki` (add-on 0.0.6) for
//! running without Anki, and for embedding:
//!
//! ```no_run
//! # async fn run() -> Result<(), hachidori_relay::Error> {
//! let relay = hachidori_relay::Relay::builder()
//!     .port(8771)
//!     .api_port(Some(19633))
//!     .start()?;
//! println!("relay on {}, API: {:?}", relay.port(), relay.api());
//! relay.shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! The library never prints and never exits the process. It runs on the caller's
//! tokio runtime, which must have the time and I/O drivers enabled.

mod api;
mod json;
mod listener;
mod net;
mod relay;
mod ws;

use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

/// The relay's default WebSocket port, the one Hachidori's Sharing uses.
pub const DEFAULT_PORT: u16 = 8771;
/// The default port of the Yomitan-compatible HTTP API (Yomitan's own default).
pub const DEFAULT_API_PORT: u16 = 19633;
/// How often the relay pings every connection, by default.
pub const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(20);
/// How long the API waits for the host to answer a lookup, by default.
pub const DEFAULT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the API waits for the host to answer one download chunk, by default.
pub const DEFAULT_CHUNK_TIMEOUT: Duration = Duration::from_secs(60);

/// Safety limits. Upstream has none. These defaults sit far above any real traffic,
/// so a well-behaved peer never reaches them.
///
/// A peer that goes over a limit is disconnected, and that is logged at `warn`
/// level. Nothing else is affected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest WebSocket upgrade request head, in bytes. A larger one gets `431`.
    pub max_handshake_bytes: usize,
    /// Largest WebSocket message (one frame, or all fragments together), in bytes.
    pub max_message_bytes: usize,
    /// Most bytes that may wait unsent for one peer that has stopped reading.
    /// Above this, the peer is dropped.
    pub max_queued_bytes: usize,
    /// How long a new connection may take to send its upgrade request.
    pub handshake_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_handshake_bytes: 64 * 1024,
            max_message_bytes: 256 * 1024 * 1024,
            max_queued_bytes: 512 * 1024 * 1024,
            handshake_timeout: Duration::from_secs(30),
        }
    }
}

/// Configures and starts a [`Relay`].
#[derive(Clone, Debug)]
#[must_use]
pub struct Builder {
    port: u16,
    api_port: Option<u16>,
    ping_interval: Duration,
    lookup_timeout: Duration,
    chunk_timeout: Duration,
    version: String,
    limits: Limits,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            api_port: None,
            ping_interval: DEFAULT_PING_INTERVAL,
            lookup_timeout: DEFAULT_LOOKUP_TIMEOUT,
            chunk_timeout: DEFAULT_CHUNK_TIMEOUT,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            limits: Limits::default(),
        }
    }
}

impl Builder {
    /// The relay's WebSocket port (default 8771). `0` picks a free one.
    pub const fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// The HTTP API port, or `None` (the default) to leave the API off. `0` picks a free one.
    pub const fn api_port(mut self, port: Option<u16>) -> Self {
        self.api_port = port;
        self
    }

    /// Time between the keep-alive pings (default 20 s).
    pub const fn ping_interval(mut self, interval: Duration) -> Self {
        self.ping_interval = interval;
        self
    }

    /// How long the API waits for the host to answer a lookup (default 30 s).
    pub const fn lookup_timeout(mut self, timeout: Duration) -> Self {
        self.lookup_timeout = timeout;
        self
    }

    /// How long the API waits for one download chunk (default 60 s).
    pub const fn chunk_timeout(mut self, timeout: Duration) -> Self {
        self.chunk_timeout = timeout;
        self
    }

    /// The version reported by `POST /serverVersion` and in the API session's
    /// `hello` (`hachidori-relay/<version>`). Defaults to this crate's version.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// The safety limits (see [`Limits`]).
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Binds the relay on `127.0.0.1` and, if one is configured, the API port.
    /// Both listeners then run as tasks on the current tokio runtime.
    ///
    /// # Panics
    ///
    /// When called outside a tokio runtime, as `tokio::spawn` does.
    ///
    /// # Errors
    ///
    /// [`Error::Bind`] when the relay port cannot be bound. An API port that cannot
    /// be bound does not fail the start: the relay runs without the API, and
    /// [`Relay::api`] reports why.
    pub fn start(self) -> Result<Relay, Error> {
        relay::start(self)
    }
}

/// The state of the HTTP API listener.
#[derive(Debug, Clone)]
pub enum ApiStatus {
    /// No API port was configured.
    Disabled,
    /// The API listens on this port.
    Listening(u16),
    /// The API port could not be bound; the relay runs without the API.
    Failed(Arc<io::Error>),
}

/// Why a relay could not start or stopped running.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The relay port could not be bound.
    #[error("could not use port {port}: {source}")]
    Bind {
        /// The port asked for.
        port: u16,
        /// The operating system's reason.
        #[source]
        source: io::Error,
    },
    /// The relay could not return to `127.0.0.1` after a network switch, and is
    /// no longer listening.
    #[error("the relay could not listen on this computer again: {0}")]
    Relisten(#[source] Arc<io::Error>),
}

/// A running relay. Dropping it does not stop the tasks; call [`Relay::shutdown`].
pub struct Relay {
    shared: Arc<relay::Shared>,
    port: u16,
    api: ApiStatus,
    shutdown: watch::Sender<bool>,
    fatal: watch::Receiver<Option<Arc<io::Error>>>,
}

impl fmt::Debug for Relay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Relay").field("port", &self.port).field("api", &self.api).finish_non_exhaustive()
    }
}

impl Relay {
    /// A builder with the defaults: port 8771, API off.
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// The bound relay port (useful after asking for port `0`).
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Whether the HTTP API listens, and on which port.
    #[must_use]
    pub const fn api(&self) -> &ApiStatus {
        &self.api
    }

    /// Resolves once the relay can no longer run, which happens when the relay
    /// could not listen on this computer again after a network switch. Until then
    /// it stays pending.
    ///
    /// # Errors
    ///
    /// [`Error::Relisten`] with the reason.
    pub async fn closed(&mut self) -> Result<(), Error> {
        match self.fatal.wait_for(Option::is_some).await {
            Ok(error) => Err(Error::Relisten(error.clone().unwrap_or_else(|| Arc::new(io::Error::other("stopped"))))),
            // The sender lives in `shared`, which `self` keeps alive: never reached.
            Err(_) => Ok(()),
        }
    }

    /// Stops listening on both ports, closes every connection and stops the ping task.
    pub async fn shutdown(self) {
        self.shutdown.send_replace(true);
        self.shared.close_all();
        // Give the connection tasks a moment to send their close frames.
        tokio::task::yield_now().await;
    }
}

/// Resolves once the flag is `true`, or once its sender is gone. Unlike awaiting
/// `wait_for` inside `select!`, it holds no borrow of the value across an await.
pub(crate) async fn until_true(flag: &mut watch::Receiver<bool>) {
    let _ = flag.wait_for(|set| *set).await;
}
