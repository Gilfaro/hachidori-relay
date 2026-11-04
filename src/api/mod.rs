// SPDX-License-Identifier: GPL-3.0-or-later
//! The Yomitan-compatible HTTP API and the dictionary downloads.
//!
//! Tools that speak Yomitan's API (backfill-anki-yomitan, scripts, curl) POST JSON
//! to `/termEntries`, `/kanjiEntries`, `/ankiFields`, `/ankiCardFormats`, `/tokenize`,
//! `/yomitanVersion` and `/serverVersion`. Linked apps list the sharing Hachidori's
//! dictionaries with `GET /dictionaries`, and download one with `GET /dictionaries/<id>`.

mod body;
mod routes;
mod session;

use std::borrow::Cow;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hyper::StatusCode;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpStream;
use tokio::sync::watch;

use crate::listener::Handler;
use crate::relay::Shared;
use session::Session;

/// How long an idle keep-alive connection, or a slow request head, may hold a socket.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// An HTTP status, and the message for its `{"error": ...}` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApiError {
    pub(crate) status: StatusCode,
    pub(crate) message: Cow<'static, str>,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, message: impl Into<Cow<'static, str>>) -> Self {
        Self { status, message: message.into() }
    }

    pub(crate) fn bad_request(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub(crate) fn unavailable(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, message)
    }

    pub(crate) fn timeout() -> Self {
        Self::new(StatusCode::GATEWAY_TIMEOUT, session::HOST_TIMEOUT)
    }

    pub(crate) fn malformed() -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "The sharing Hachidori sent a malformed reply.")
    }

    pub(crate) fn unknown_path() -> Self {
        Self::new(StatusCode::NOT_FOUND, "unknown path")
    }
}

/// The API server, shared by its connections.
pub(crate) struct Server {
    relay: Arc<Shared>,
    version: String,
    lookup_timeout: Duration,
    chunk_timeout: Duration,
    session: Mutex<Option<Arc<Session>>>,
    stop: watch::Receiver<bool>,
}

impl Server {
    pub(crate) fn new(
        relay: Arc<Shared>,
        version: String,
        lookup_timeout: Duration,
        chunk_timeout: Duration,
        stop: watch::Receiver<bool>,
    ) -> Arc<Self> {
        Arc::new(Self { relay, version, lookup_timeout, chunk_timeout, session: Mutex::new(None), stop })
    }

    /// A ready session toward the current host. After the previous host went away, it is a new one.
    async fn session(&self) -> Result<Arc<Session>, ApiError> {
        let session = {
            let mut current = self.session.lock().unwrap_or_else(PoisonError::into_inner);
            match current.as_ref() {
                Some(session) if session.alive() => session.clone(),
                _ => {
                    let session = Session::new(self.relay.clone(), &self.version, self.lookup_timeout);
                    *current = Some(session.clone());
                    session
                }
            }
        };
        session.ready().await?;
        Ok(session)
    }
}

/// The listener handler: each accepted socket is served as HTTP/1.1 with keep-alive.
pub(crate) fn handler(server: Arc<Server>) -> Handler {
    Arc::new(move |stream, _peer| {
        tokio::spawn(serve_connection(server.clone(), stream));
    })
}

async fn serve_connection(server: Arc<Server>, stream: TcpStream) {
    let mut stop = server.stop.clone();
    let service = service_fn(move |request| {
        let server = server.clone();
        async move { Ok::<_, Infallible>(routes::handle(&server, request).await) }
    });
    let connection = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .keep_alive(true)
        .serve_connection(TokioIo::new(stream), service);
    tokio::select! {
        _ = connection => {}
        () = crate::until_true(&mut stop) => {}
    }
}
