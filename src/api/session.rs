// SPDX-License-Identifier: GPL-3.0-or-later
//! The API's in-process relay client.
//!
//! It is an ordinary linked client (origin `relay://yomitan-api`, address
//! `127.0.0.1`). It sends the sharing protocol's `hello`, then correlated `request`
//! frames, and it answers pings. The host replies as it would to any linked browser.

use std::borrow::Cow;
use std::collections::HashMap;
use std::mem;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot, watch};

use super::ApiError;
use crate::json;
use crate::relay::{Role, Shared};
use crate::ws::peer::{ApiEvent, Peer};

/// The capability a host advertises when it serves the API, and that the session advertises too.
pub(crate) const CAPABILITY: &str = "hoshidicts-api-v1";
const API_ORIGIN: &str = "relay://yomitan-api";
const API_ADDRESS: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const API_NAME: &str = "Hachidori Relay API";
const PROTOCOL_VERSION: u32 = 1;
const TARGET: &str = "hoshidicts-offscreen";

pub(crate) const NO_HOST: &str = "No sharing Hachidori is connected to the relay.";
const NO_CAPABILITY: &str = "The sharing Hachidori does not support the relay API; update Hachidori.";
pub(crate) const HOST_TIMEOUT: &str = "The sharing Hachidori did not answer in time.";
const DISCONNECTED: &str = "The sharing Hachidori disconnected.";
const CLOSED_HERE: &str = "The relay closed its API session.";

/// Runs with the reply that arrives after its request stopped waiting. It lets a
/// lease the host granted too late still be released.
pub(crate) type Late = Box<dyn FnOnce(&RawValue) + Send>;

type Reply = Result<Box<RawValue>, ApiError>;

struct Waiter {
    /// `None` once the requester has stopped waiting.
    reply: Option<oneshot::Sender<Reply>>,
    late: Option<Late>,
}

struct State {
    alive: bool,
    failure: Option<String>,
    pending: HashMap<String, Waiter>,
    next_id: u64,
    capabilities: Vec<String>,
}

/// What a pulling session hears from the browser host besides replies (host mode).
/// Each value is the frame's field as the host sent it.
#[derive(Debug)]
#[cfg_attr(not(feature = "host"), allow(dead_code, reason = "used by host mode"))]
pub(crate) enum HostNews {
    /// The answer to the session's `hello`: the host's shared storage.
    Snapshot(Box<RawValue>),
    /// A `storage` broadcast: the changed shared keys, `null` for a removed one.
    Storage(Box<RawValue>),
}

/// One session toward the current host. A new one replaces it after the host goes away.
pub(crate) struct Session {
    relay: Arc<Shared>,
    /// `Some` for host mode's pull session, which always talks to the browser host and
    /// reports what that host shares.
    news: Option<mpsc::UnboundedSender<HostNews>>,
    hello_version: String,
    lookup_timeout: Duration,
    state: Mutex<State>,
    client: OnceLock<Arc<str>>,
    hello: watch::Sender<bool>,
    /// Serialises the first handshake; `true` once the host has answered with the capability.
    opening: tokio::sync::Mutex<bool>,
}

#[derive(Serialize)]
struct RequestFrame<'a> {
    kind: &'static str,
    id: &'a str,
    message: Envelope<'a>,
}

/// A runtime message for the host. The payload never overrides `target` or `type`.
#[derive(Serialize)]
struct Envelope<'a> {
    #[serde(flatten)]
    payload: &'a Map<String, Value>,
    target: &'static str,
    #[serde(rename = "type")]
    kind: &'a str,
}

#[derive(Serialize)]
struct Hello<'a> {
    kind: &'static str,
    protocol: u32,
    version: &'a str,
    name: &'static str,
    capabilities: [&'static str; 1],
}

/// What the session reads from a host frame.
#[derive(Deserialize)]
struct Inbound<'a> {
    #[serde(default, borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(default, borrow, deserialize_with = "json::present")]
    id: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "json::present")]
    response: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "json::present")]
    capabilities: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "json::present")]
    reason: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "json::present")]
    snapshot: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "json::present")]
    changes: Option<&'a RawValue>,
}

impl Session {
    pub(crate) fn new(relay: Arc<Shared>, version: &str, lookup_timeout: Duration) -> Arc<Self> {
        Self::with_news(relay, version, lookup_timeout, None)
    }

    /// Host mode's session toward the browser host, which reports the host's shared storage to `news`.
    #[cfg_attr(not(feature = "host"), allow(dead_code, reason = "used by host mode"))]
    pub(crate) fn pulling(relay: Arc<Shared>, version: &str, lookup_timeout: Duration, news: mpsc::UnboundedSender<HostNews>) -> Arc<Self> {
        Self::with_news(relay, version, lookup_timeout, Some(news))
    }

    fn with_news(relay: Arc<Shared>, version: &str, lookup_timeout: Duration, news: Option<mpsc::UnboundedSender<HostNews>>) -> Arc<Self> {
        Arc::new(Self {
            relay,
            news,
            hello_version: format!("hachidori-relay/{version}"),
            lookup_timeout,
            state: Mutex::new(State { alive: true, failure: None, pending: HashMap::new(), next_id: 0, capabilities: Vec::new() }),
            client: OnceLock::new(),
            hello: watch::channel(false).0,
            opening: tokio::sync::Mutex::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn alive(&self) -> bool {
        self.lock().alive
    }

    fn failure(&self) -> ApiError {
        failure_of(&self.lock())
    }

    /// Connects and shakes hands on first use. Fails with 503 when no host can serve,
    /// 501 when the host lacks the capability, and 504 when it never answers.
    pub(crate) async fn ready(self: &Arc<Self>) -> Result<(), ApiError> {
        let mut ready = self.opening.lock().await;
        if !self.alive() {
            return Err(self.failure());
        }
        if *ready {
            return Ok(());
        }
        if self.client.get().is_none() {
            let (events, receiver) = mpsc::unbounded_channel();
            let connected = if self.news.is_some() {
                self.relay.connect_browser_client(Peer::Api(events), API_ORIGIN, API_ADDRESS)
            } else {
                self.relay.connect_client(Peer::Api(events), API_ORIGIN, API_ADDRESS)
            };
            let Some(Role::Client(id)) = connected else {
                self.lost(NO_HOST, false);
                return Err(ApiError::unavailable(NO_HOST));
            };
            let _ = self.client.set(id);
            tokio::spawn(self.clone().run(receiver));
            self.message(&Hello {
                kind: "hello",
                protocol: PROTOCOL_VERSION,
                version: &self.hello_version,
                name: API_NAME,
                capabilities: [CAPABILITY],
            })?;
        }
        let mut hello = self.hello.subscribe();
        if tokio::time::timeout(self.lookup_timeout, hello.wait_for(|answered| *answered)).await.is_err() {
            if !self.alive() {
                return Err(self.failure());
            }
            self.close();
            return Err(ApiError::timeout());
        }
        if !self.alive() {
            return Err(self.failure());
        }
        if !self.lock().capabilities.iter().any(|capability| capability == CAPABILITY) {
            // The session stays: the answer will not change until the host does.
            return Err(ApiError::new(hyper::StatusCode::NOT_IMPLEMENTED, NO_CAPABILITY));
        }
        *ready = true;
        drop(ready);
        Ok(())
    }

    fn message(&self, frame: &impl Serialize) -> Result<(), ApiError> {
        let Some(id) = self.client.get() else { return Err(self.failure()) };
        let text = serde_json::to_string(frame).map_err(|_| ApiError::malformed())?;
        self.relay.client_text(id, &text);
        Ok(())
    }

    /// The host's response to one `hd_api_*` request. A `{error}` answer is a 500, or a
    /// 404 with `notFound: true`.
    pub(crate) async fn request(&self, kind: &str, payload: &Map<String, Value>, timeout: Duration, late: Option<Late>) -> Reply {
        let (reply, mut answer) = oneshot::channel();
        let id = {
            let mut state = self.lock();
            if !state.alive {
                return Err(failure_of(&state));
            }
            state.next_id += 1;
            let id = format!("api-{}", state.next_id);
            state.pending.insert(id.clone(), Waiter { reply: Some(reply), late });
            drop(state);
            id
        };
        // If the HTTP request is dropped while it waits, its waiter goes too.
        let _forget = Forget { session: self, id: &id };
        self.message(&RequestFrame { kind: "request", id: &id, message: Envelope { payload: &own_keys(payload), target: TARGET, kind } })?;
        let outcome = match tokio::time::timeout(timeout, &mut answer).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(self.failure()),
            Err(_) => {
                // A reply that races the timeout wins. Otherwise the waiter stays
                // only for its late callback.
                let mut state = self.lock();
                let raced = answer.try_recv();
                forget(&mut state, &id);
                drop(state);
                raced.unwrap_or_else(|_| Err(ApiError::timeout()))
            }
        };
        outcome.and_then(host_error)
    }

    /// Sends a request whose reply nobody waits for. Used to release host resources.
    pub(crate) fn notify(&self, kind: &str, payload: &Map<String, Value>) {
        let id = {
            let mut state = self.lock();
            if !state.alive {
                return;
            }
            state.next_id += 1;
            let id = format!("api-{}", state.next_id);
            drop(state);
            id
        };
        let _ = self.message(&RequestFrame {
            kind: "request",
            id: &id,
            message: Envelope { payload: &own_keys(payload), target: TARGET, kind },
        });
    }

    async fn run(self: Arc<Self>, mut events: mpsc::UnboundedReceiver<ApiEvent>) {
        while let Some(event) = events.recv().await {
            match event {
                ApiEvent::Text(text) => self.receive(&text),
                ApiEvent::Closed => self.lost(DISCONNECTED, true),
            }
        }
    }

    /// One frame from the host, or the relay's own ping.
    fn receive(&self, text: &[u8]) {
        let Ok(text) = std::str::from_utf8(text) else { return };
        if !json::is_object_text(text) {
            return;
        }
        let Ok(frame) = serde_json::from_str::<Inbound<'_>>(text) else { return };
        match frame.kind.as_deref() {
            Some("hello") => {
                let capabilities = frame
                    .capabilities
                    .and_then(json::elements)
                    .map(|items| items.into_iter().filter_map(json::as_str).map(Cow::into_owned).collect())
                    .unwrap_or_default();
                self.lock().capabilities = capabilities;
                if let (Some(news), Some(snapshot)) = (&self.news, frame.snapshot) {
                    let _ = news.send(HostNews::Snapshot(snapshot.to_owned()));
                }
                self.hello.send_replace(true);
            }
            Some("storage") => {
                if let (Some(news), Some(changes)) = (&self.news, frame.changes) {
                    let _ = news.send(HostNews::Storage(changes.to_owned()));
                }
            }
            Some("reply") => {
                let Some(id) = frame.id.and_then(json::as_str) else { return };
                let response = frame.response.unwrap_or(RawValue::NULL);
                let mut state = self.lock();
                let Some(waiter) = state.pending.get_mut(id.as_ref()) else { return };
                if let Some(reply) = waiter.reply.take() {
                    state.pending.remove(id.as_ref());
                    drop(state);
                    let _ = reply.send(Ok(response.to_owned()));
                } else if let Some(late) = state.pending.remove(id.as_ref()).and_then(|waiter| waiter.late) {
                    drop(state);
                    late(response);
                }
            }
            Some("ping") => {
                let _ = self.message(&serde_json::json!({ "kind": "pong" }));
            }
            Some("bye") => {
                let reason = frame.reason.map(json::display).unwrap_or_default();
                let message = format!("The sharing Hachidori ended the session: {reason}");
                self.lost(message.trim_end_matches([':', ' ']), true);
            }
            _ => {}
        }
    }

    /// Ends the session. Every request still waiting gets 503. Replies that already
    /// arrived stand.
    fn lost(&self, reason: &str, tell_relay: bool) {
        let waiters = {
            let mut state = self.lock();
            if !state.alive {
                return;
            }
            state.alive = false;
            state.failure = Some(reason.to_owned());
            mem::take(&mut state.pending)
        };
        for reply in waiters.into_values().filter_map(|waiter| waiter.reply) {
            let _ = reply.send(Err(ApiError::unavailable(reason.to_owned())));
        }
        self.hello.send_replace(true);
        if tell_relay && let Some(id) = self.client.get() {
            self.relay.close_client(id);
        }
    }

    /// Ends the session from this side, so the host sees the client close.
    pub(crate) fn close(&self) {
        self.lost(CLOSED_HERE, true);
    }
}

/// The payload without `target` and `type`, which the envelope sets. Without this,
/// both would appear twice in the frame.
fn own_keys(payload: &Map<String, Value>) -> Cow<'_, Map<String, Value>> {
    if payload.contains_key("target") || payload.contains_key("type") {
        let mut own = payload.clone();
        own.remove("target");
        own.remove("type");
        Cow::Owned(own)
    } else {
        Cow::Borrowed(payload)
    }
}

fn failure_of(state: &State) -> ApiError {
    ApiError::unavailable(state.failure.clone().unwrap_or_else(|| NO_HOST.to_owned()))
}

/// Drops a waiter whose requester is gone. It is kept only if it has a late callback.
fn forget(state: &mut State, id: &str) {
    if let Some(waiter) = state.pending.get_mut(id) {
        if waiter.late.is_some() {
            waiter.reply = None;
        } else {
            state.pending.remove(id);
        }
    }
}

struct Forget<'a> {
    session: &'a Session,
    id: &'a str,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        forget(&mut self.session.lock(), self.id);
    }
}

/// Turns a host's `{error}` answer into a 500, or a 404 when it also says `notFound: true`.
fn host_error(response: Box<RawValue>) -> Reply {
    let error = json::fields(&response).and_then(|fields| {
        let message = json::display(fields.get("error")?);
        let not_found = fields.get("notFound").is_some_and(|raw| json::is_true(raw));
        let status = if not_found { hyper::StatusCode::NOT_FOUND } else { hyper::StatusCode::INTERNAL_SERVER_ERROR };
        Some(ApiError::new(status, message))
    });
    error.map_or(Ok(response), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(text: &str) -> Box<RawValue> {
        RawValue::from_string(text.to_owned()).unwrap_or_else(|error| panic!("{error}"))
    }

    #[test]
    fn host_errors_map_to_500_or_404() {
        let error = host_error(raw(r#"{"error": "no dictionary matched"}"#)).err();
        assert_eq!(
            error.map(|error| (error.status, error.message)),
            Some((hyper::StatusCode::INTERNAL_SERVER_ERROR, "no dictionary matched".into()))
        );
        let missing = host_error(raw(r#"{"error": "unknown dictionary", "notFound": true}"#)).err();
        assert_eq!(missing.map(|error| error.status), Some(hyper::StatusCode::NOT_FOUND));
        let null = host_error(raw(r#"{"error": null}"#)).err();
        assert_eq!(null.map(|error| error.message), Some("null".into()));
        assert!(host_error(raw(r#"{"version": "0.9.0"}"#)).is_ok());
        assert!(host_error(raw(r#"[{"error": 1}]"#)).is_ok());
    }

    /// Ports of upstream's `test/test_api.py` session cases. A host is played through an
    /// in-memory socket peer, so nothing touches the network.
    #[allow(clippy::expect_used, reason = "a failed expectation is a failed test")]
    mod upstream {
        use std::sync::Arc;
        use std::time::Duration;

        use serde_json::{Map, Value, json};
        use tokio::sync::{mpsc, watch};

        use super::super::*;
        use crate::Limits;
        use crate::ws::frame::TEXT;
        use crate::ws::peer::{Outgoing, SocketPeer};

        const LOOKUP: Duration = Duration::from_millis(300);
        const WAIT: Duration = Duration::from_secs(5);

        struct Host {
            relay: Arc<Shared>,
            role: Role,
            frames: mpsc::UnboundedReceiver<Outgoing>,
        }

        impl Host {
            fn new() -> Self {
                let relay = Arc::new(Shared::new(0, Limits::default(), watch::channel(None).0));
                let (peer, frames) = SocketPeer::new(usize::MAX);
                let role = relay.connect_host(Peer::Socket(peer)).expect("the first host is accepted");
                let mut host = Self { relay, role, frames };
                assert_eq!(host.try_next().map(|frame| frame["kind"].clone()), Some(json!("listening")));
                host
            }

            fn try_next(&mut self) -> Option<Value> {
                self.frames.try_recv().ok().map(|frame| decode(&frame))
            }

            /// The next frame the relay sends the host.
            async fn next(&mut self) -> Value {
                let frame = tokio::time::timeout(WAIT, self.frames.recv()).await.expect("a frame in time").expect("open");
                decode(&frame)
            }

            /// The next `client-text` from the session, parsed.
            async fn said(&mut self) -> Value {
                loop {
                    let frame = self.next().await;
                    if frame["kind"] == "client-text" {
                        return serde_json::from_str(frame["text"].as_str().expect("text")).expect("JSON");
                    }
                }
            }

            async fn until_client_close(&mut self) {
                while self.next().await["kind"] != "client-close" {}
            }

            /// Sends `frame` to the API client, as the host would.
            async fn says(&self, client: &str, frame: &Value) {
                self.send_text(client, &frame.to_string()).await;
            }

            async fn send_text(&self, client: &str, text: &str) {
                let wrapped = json!({ "kind": "send", "clientId": client, "text": text });
                self.relay.text(&self.role, wrapped.to_string()).await;
            }

            async fn hello(&self, client: &str, capabilities: &[&str]) {
                let hello = json!({ "kind": "hello", "protocol": 1, "version": "9.9", "name": "Stub", "capabilities": capabilities });
                self.says(client, &hello).await;
            }

            async fn reply(&self, client: &str, id: &Value, response: &Value) {
                self.says(client, &json!({ "kind": "reply", "id": id, "response": response })).await;
            }

            async fn lose(&self) {
                self.relay.closed(&self.role).await;
            }
        }

        fn decode(frame: &Outgoing) -> Value {
            assert_eq!(frame.opcode, TEXT);
            serde_json::from_slice(&frame.payload).expect("JSON frame")
        }

        /// A ready session and its client id, after the hello exchange.
        async fn ready(host: &mut Host) -> (Arc<Session>, String) {
            let session = Session::new(host.relay.clone(), "0.0.4", LOOKUP);
            let started = tokio::spawn({
                let session = session.clone();
                async move { session.ready().await }
            });
            let open = host.next().await;
            let client = open["clientId"].as_str().expect("client id").to_owned();
            assert_eq!(host.said().await["kind"], "hello");
            host.hello(&client, &[CAPABILITY]).await;
            started.await.expect("task").expect("ready");
            (session, client)
        }

        fn request(session: &Arc<Session>, kind: &'static str, payload: Value, timeout: Duration) -> tokio::task::JoinHandle<Reply> {
            let session = session.clone();
            tokio::spawn(async move {
                let payload: Map<String, Value> = serde_json::from_value(payload).expect("object");
                session.request(kind, &payload, timeout, None).await
            })
        }

        fn parsed(reply: Reply) -> Value {
            serde_json::from_str(reply.expect("a reply").get()).expect("JSON")
        }

        #[tokio::test]
        async fn no_host_is_503_and_nothing_is_sent() {
            let relay = Arc::new(Shared::new(0, Limits::default(), watch::channel(None).0));
            let error = Session::new(relay, "0.0.4", LOOKUP).ready().await.err();
            assert_eq!(error, Some(ApiError::unavailable(NO_HOST)));
        }

        #[tokio::test]
        async fn hello_handshake_identifies_the_relay_and_ready_is_idempotent() {
            let mut host = Host::new();
            let session = Session::new(host.relay.clone(), "0.0.4", LOOKUP);
            let started = tokio::spawn({
                let session = session.clone();
                async move { session.ready().await }
            });
            let open = host.next().await;
            assert_eq!((&open["origin"], &open["address"]), (&json!(API_ORIGIN), &json!("127.0.0.1")));
            let client = open["clientId"].as_str().expect("id").to_owned();
            assert_eq!(
                host.said().await,
                json!({ "kind": "hello", "protocol": 1, "version": "hachidori-relay/0.0.4", "name": API_NAME, "capabilities": [CAPABILITY] })
            );
            host.hello(&client, &[CAPABILITY]).await;
            started.await.expect("task").expect("ready");
            session.ready().await.expect("ready again");
            assert!(host.try_next().is_none(), "no second hello");
        }

        #[tokio::test]
        async fn host_without_capability_is_501_and_stays_so() {
            let mut host = Host::new();
            let session = Session::new(host.relay.clone(), "0.0.4", LOOKUP);
            let started = tokio::spawn({
                let session = session.clone();
                async move { session.ready().await }
            });
            let client = host.next().await["clientId"].as_str().expect("id").to_owned();
            host.said().await;
            host.hello(&client, &["other"]).await;
            let status = |result: Result<(), ApiError>| result.err().map(|error| error.status);
            assert_eq!(status(started.await.expect("task")), Some(hyper::StatusCode::NOT_IMPLEMENTED));
            assert_eq!(status(session.ready().await), Some(hyper::StatusCode::NOT_IMPLEMENTED));
            assert!(session.alive());
        }

        #[tokio::test]
        async fn hello_that_never_comes_is_504_and_closes_the_session() {
            let mut host = Host::new();
            let session = Session::new(host.relay.clone(), "0.0.4", LOOKUP);
            assert_eq!(session.ready().await.err(), Some(ApiError::timeout()));
            assert!(!session.alive());
            host.until_client_close().await;
        }

        #[tokio::test]
        async fn replies_are_correlated_by_id_even_out_of_order() {
            let mut host = Host::new();
            let (session, client) = ready(&mut host).await;
            let first = request(&session, "hd_api_version", json!({}), LOOKUP * 10);
            let first_id = host.said().await["id"].clone();
            let second = request(&session, "hd_api_term_entries", json!({ "terms": ["a"] }), LOOKUP * 10);
            let frame = host.said().await;
            assert_ne!(frame["id"], first_id);
            assert_eq!(frame["message"], json!({ "target": TARGET, "type": "hd_api_term_entries", "terms": ["a"] }));
            host.reply(&client, &frame["id"], &json!({ "results": [] })).await;
            host.reply(&client, &json!("api-unknown"), &json!({ "ignored": true })).await;
            host.reply(&client, &first_id, &json!({ "version": "9.9" })).await;
            assert_eq!(parsed(second.await.expect("task")), json!({ "results": [] }));
            assert_eq!(parsed(first.await.expect("task")), json!({ "version": "9.9" }));
        }

        #[tokio::test]
        async fn payload_cannot_override_target_or_type() {
            let mut host = Host::new();
            let (session, client) = ready(&mut host).await;
            let pending = request(&session, "hd_api_anki_fields", json!({ "type": "kanji", "target": "elsewhere", "text": "x" }), LOOKUP);
            let client_text = loop {
                let frame = host.next().await;
                if frame["kind"] == "client-text" {
                    break frame["text"].as_str().expect("text").to_owned();
                }
            };
            assert_eq!(client_text.matches("\"type\"").count(), 1, "each key appears once: {client_text}");
            assert_eq!(client_text.matches("\"target\"").count(), 1, "each key appears once: {client_text}");
            let frame: Value = serde_json::from_str(&client_text).expect("JSON");
            assert_eq!(frame["message"], json!({ "target": TARGET, "type": "hd_api_anki_fields", "text": "x" }));
            host.reply(&client, &frame["id"], &json!({ "fields": [] })).await;
            pending.await.expect("task").expect("reply");
        }

        #[tokio::test]
        async fn error_replies_map_to_500_or_404() {
            let mut host = Host::new();
            let (session, client) = ready(&mut host).await;
            let cases = [
                (json!({ "error": "boom" }), hyper::StatusCode::INTERNAL_SERVER_ERROR),
                (json!({ "error": "gone", "notFound": true }), hyper::StatusCode::NOT_FOUND),
                (json!({ "error": "x", "notFound": "yes" }), hyper::StatusCode::INTERNAL_SERVER_ERROR),
            ];
            for (response, status) in cases {
                let pending = request(&session, "hd_api_dictionary_open", json!({ "id": "d" }), LOOKUP);
                let id = host.said().await["id"].clone();
                host.reply(&client, &id, &response).await;
                let error = pending.await.expect("task").expect_err("an error");
                assert_eq!((error.status, error.message.as_ref()), (status, response["error"].as_str().expect("text")));
            }
            assert!(session.alive(), "an error reply does not end the session");
        }

        #[tokio::test]
        async fn timeout_is_504_and_a_late_reply_is_ignored() {
            let mut host = Host::new();
            let (session, client) = ready(&mut host).await;
            let timed_out = request(&session, "hd_api_version", json!({}), Duration::from_millis(50));
            let late_id = host.said().await["id"].clone();
            assert_eq!(timed_out.await.expect("task").err(), Some(ApiError::timeout()));
            host.reply(&client, &late_id, &json!({ "version": "late" })).await;
            assert!(session.alive());
            assert!(session.lock().pending.is_empty(), "no waiter is kept without a late callback");
            let fresh = request(&session, "hd_api_version", json!({}), LOOKUP);
            let id = host.said().await["id"].clone();
            host.reply(&client, &id, &json!({ "version": "fresh" })).await;
            assert_eq!(parsed(fresh.await.expect("task")), json!({ "version": "fresh" }));
        }

        #[tokio::test]
        async fn host_loss_fails_every_pending_request_with_503() {
            let mut host = Host::new();
            let (session, _) = ready(&mut host).await;
            let mut waits = Vec::new();
            for _ in 0..3 {
                waits.push(request(&session, "hd_api_version", json!({}), WAIT));
                host.said().await;
            }
            host.lose().await;
            for wait in waits {
                let error = wait.await.expect("task").expect_err("an error");
                assert_eq!(error.status, hyper::StatusCode::SERVICE_UNAVAILABLE);
                assert!(error.message.contains("disconnected"), "{}", error.message);
            }
            assert!(!session.alive());
            let after = request(&session, "hd_api_version", json!({}), LOOKUP).await.expect("task");
            assert_eq!(after.err().map(|error| error.status), Some(hyper::StatusCode::SERVICE_UNAVAILABLE));
            assert!(session.ready().await.is_err());
        }

        #[tokio::test]
        async fn a_reply_that_arrives_just_before_host_loss_still_stands() {
            let mut host = Host::new();
            let (session, client) = ready(&mut host).await;
            let pending = request(&session, "hd_api_version", json!({}), WAIT);
            let id = host.said().await["id"].clone();
            host.reply(&client, &id, &json!({ "version": "arrived" })).await;
            host.lose().await;
            assert_eq!(parsed(pending.await.expect("task")), json!({ "version": "arrived" }));
            tokio::time::timeout(WAIT, async {
                while session.alive() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the session ends");
        }

        #[tokio::test]
        async fn a_host_close_of_the_client_removes_it_from_the_relay() {
            let mut host = Host::new();
            let (session, client) = ready(&mut host).await;
            host.relay.text(&host.role, json!({ "kind": "close", "clientId": client }).to_string()).await;
            host.until_client_close().await;
            assert!(!session.alive());
        }

        #[tokio::test]
        async fn a_late_reply_after_a_timeout_reaches_the_late_callback() {
            let mut host = Host::new();
            let (session, client) = ready(&mut host).await;
            let (late, mut arrived) = mpsc::unbounded_channel();
            let callback: Late = Box::new(move |response| {
                let _ = late.send(response.get().to_owned());
            });
            let pending = tokio::spawn({
                let session = session.clone();
                async move { session.request("hd_api_dictionary_open", &Map::new(), Duration::from_millis(50), Some(callback)).await }
            });
            let id = host.said().await["id"].clone();
            assert_eq!(pending.await.expect("task").err(), Some(ApiError::timeout()));
            host.reply(&client, &id, &json!({ "token": "dl-9", "size": 1 })).await;
            let response = tokio::time::timeout(WAIT, arrived.recv()).await.expect("the late callback runs").expect("open");
            assert_eq!(serde_json::from_str::<Value>(&response).expect("JSON"), json!({ "token": "dl-9", "size": 1 }));
            assert!(session.lock().pending.is_empty(), "the waiter is gone once its late reply came");
        }

        #[tokio::test]
        async fn bye_from_the_host_ends_the_session_with_its_reason() {
            let mut host = Host::new();
            let (session, client) = ready(&mut host).await;
            let pending = request(&session, "hd_api_version", json!({}), WAIT);
            host.said().await;
            host.says(&client, &json!({ "kind": "bye", "reason": "malformed sharing frame" })).await;
            let error = pending.await.expect("task").expect_err("an error");
            assert_eq!(error.status, hyper::StatusCode::SERVICE_UNAVAILABLE);
            assert!(error.message.contains("malformed sharing frame"), "{}", error.message);
            host.until_client_close().await;
        }

        #[tokio::test]
        async fn ping_is_answered_with_pong_and_other_frames_are_ignored() {
            let mut host = Host::new();
            let (session, client) = ready(&mut host).await;
            host.says(&client, &json!({ "kind": "storage", "changes": {} })).await;
            host.says(&client, &json!("not an object")).await;
            host.send_text(&client, "{not json").await;
            host.says(&client, &json!({ "kind": "ping" })).await;
            assert_eq!(host.said().await, json!({ "kind": "pong" }));
            assert!(session.alive());
        }

        #[tokio::test]
        async fn notify_sends_without_waiting_and_is_silent_after_loss() {
            let mut host = Host::new();
            let (session, _) = ready(&mut host).await;
            let payload: Map<String, Value> = serde_json::from_value(json!({ "token": "t" })).expect("object");
            session.notify("hd_api_dictionary_close", &payload);
            assert_eq!(host.said().await["message"], json!({ "target": TARGET, "type": "hd_api_dictionary_close", "token": "t" }));
            host.lose().await;
            tokio::time::timeout(WAIT, async {
                while session.alive() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the session ends");
            session.notify("hd_api_dictionary_close", &payload);
            assert!(host.try_next().is_none(), "nothing is sent after loss");
        }
    }

    #[test]
    fn request_frames_carry_target_and_type_after_the_payload() {
        let mut payload = Map::new();
        payload.insert("terms".into(), serde_json::json!(["分かる"]));
        let frame = RequestFrame {
            kind: "request",
            id: "api-1",
            message: Envelope { payload: &payload, target: TARGET, kind: "hd_api_term_entries" },
        };
        let text = serde_json::to_string(&frame).unwrap_or_default();
        assert_eq!(
            text,
            r#"{"kind":"request","id":"api-1","message":{"terms":["分かる"],"target":"hoshidicts-offscreen","type":"hd_api_term_entries"}}"#
        );
    }
}
