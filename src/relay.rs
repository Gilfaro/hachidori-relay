// SPDX-License-Identifier: GPL-3.0-or-later
//! The relay's state: one host, its linked clients, and the network switch.
//!
//! A single mutex holds the state and fixes the order of frames. While it is held,
//! sends only push onto a peer's queue (see [`crate::ws::peer`]), so the lock never
//! waits for a socket. The network switch rebinds sockets, and it is serialised by
//! its own async mutex rather than the state lock.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tokio::sync::watch;

use crate::listener::{Handler, Listener};
use crate::net::{self, LocalAddress};
use crate::ws::peer::Peer;
use crate::{ApiStatus, Builder, Error, Limits, Relay, api, json, ws};

/// Which side of the relay a connection is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Role {
    /// The sharing Hachidori. The number tells one host connection from the next.
    Host(u64),
    /// A linked browser, or the API session.
    Client(Arc<str>),
}

/// Frames the relay sends to the host.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
enum ToHost<'a> {
    Listening {
        port: u16,
    },
    ListenFailed {
        error: &'a str,
    },
    ClientOpen {
        client_id: &'a str,
        origin: &'a str,
        address: String,
    },
    ClientText {
        client_id: &'a str,
        text: &'a str,
    },
    ClientClose {
        client_id: &'a str,
    },
    Network {
        enabled: bool,
        addresses: Vec<LocalAddress>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

impl ToHost<'_> {
    fn encode(&self) -> Bytes {
        // Room for the envelope plus the text, so a large text is not copied while the
        // buffer doubles. Escapes may still grow it.
        let text = match self {
            Self::ClientText { text, .. } => text.len(),
            _ => 0,
        };
        let mut buffer = Vec::with_capacity(text + 128);
        // Serialising these plain structs into a Vec cannot fail.
        if serde_json::to_writer(&mut buffer, self).is_err() {
            return Bytes::new();
        }
        Bytes::from(buffer)
    }
}

const PING: &[u8] = br#"{"kind":"ping"}"#;
const ANOTHER_HOST: &str = "Another browser on this computer is already sharing through Anki.";

/// What the relay reads from a host frame. Everything else in the frame is ignored.
#[derive(Deserialize)]
struct HostFrame<'a> {
    #[serde(default, borrow)]
    kind: Option<std::borrow::Cow<'a, str>>,
    #[serde(default, borrow, rename = "clientId", deserialize_with = "json::present")]
    client_id: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "json::present")]
    text: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "json::present")]
    enabled: Option<&'a RawValue>,
}

/// The text a host frame carries for its clients: a string as itself, anything else as its JSON.
fn text_of(raw: Option<&RawValue>) -> Bytes {
    raw.map_or_else(|| Bytes::from_static(b"null"), |raw| Bytes::from(json::display(raw)))
}

struct Host {
    id: u64,
    peer: Peer,
}

struct Client {
    peer: Peer,
    address: IpAddr,
}

#[derive(Default)]
struct State {
    host: Option<Host>,
    clients: HashMap<Arc<str>, Client>,
    next_client: u64,
    next_host: u64,
}

/// The relay, shared by every connection task.
pub(crate) struct Shared {
    state: Mutex<State>,
    port: u16,
    limits: Limits,
    listener: OnceLock<Listener>,
    api_listener: OnceLock<Listener>,
    switching: tokio::sync::Mutex<()>,
    fatal: watch::Sender<Option<Arc<io::Error>>>,
}

impl Shared {
    pub(crate) fn new(port: u16, limits: Limits, fatal: watch::Sender<Option<Arc<io::Error>>>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            port,
            limits,
            listener: OnceLock::new(),
            api_listener: OnceLock::new(),
            switching: tokio::sync::Mutex::new(()),
            fatal,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock leaves the state valid (every update is a
        // single insert, remove or replace), so a poisoned lock is still usable.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) const fn limits(&self) -> &Limits {
        &self.limits
    }

    pub(crate) fn has_host(&self) -> bool {
        self.lock().host.is_some()
    }

    /// Takes a new host connection. If another host already shares, the newcomer is
    /// told so and gets `None`.
    pub(crate) fn connect_host(&self, peer: Peer) -> Option<Role> {
        let mut state = self.lock();
        if state.host.is_some() {
            peer.send(ToHost::ListenFailed { error: ANOTHER_HOST }.encode());
            return None;
        }
        state.next_host += 1;
        let id = state.next_host;
        peer.send(ToHost::Listening { port: self.port }.encode());
        state.host = Some(Host { id, peer });
        drop(state);
        Some(Role::Host(id))
    }

    /// Takes a new linked client, or gives `None` while no host is connected.
    pub(crate) fn connect_client(&self, peer: Peer, origin: &str, address: IpAddr) -> Option<Role> {
        let mut state = self.lock();
        let host = state.host.as_ref()?.peer.clone();
        state.next_client += 1;
        let id: Arc<str> = format!("client-{}", state.next_client).into();
        host.send(ToHost::ClientOpen { client_id: &id, origin, address: address.to_string() }.encode());
        state.clients.insert(id.clone(), Client { peer, address });
        drop(state);
        Some(Role::Client(id))
    }

    /// One whole text message from a connection.
    pub(crate) async fn text(&self, role: &Role, text: String) {
        match role {
            Role::Client(id) => self.client_text(id, &text),
            Role::Host(id) => self.host_frame(*id, &text).await,
        }
    }

    /// Text from a client, wrapped for the host.
    pub(crate) fn client_text(&self, client_id: &str, text: &str) {
        // The JSON escaping happens before the lock is taken, so a large text does
        // not hold up the other connections.
        let frame = ToHost::ClientText { client_id, text }.encode();
        if let Some(host) = &self.lock().host {
            host.peer.send(frame);
        }
    }

    /// One frame from the host: text for one or every client, a close, or the network switch.
    async fn host_frame(&self, host_id: u64, text: &str) {
        if !json::is_object_text(text) {
            return;
        }
        let Ok(frame) = serde_json::from_str::<HostFrame<'_>>(text) else { return };
        match frame.kind.as_deref() {
            Some("network") => {
                let enabled = frame.enabled.is_some_and(json::is_true);
                let Some(reply) = self.set_network(enabled).await else { return };
                if let Some(host) = self.lock().host.as_ref().filter(|host| host.id == host_id) {
                    host.peer.send(reply);
                }
            }
            Some("broadcast") => {
                let payload = text_of(frame.text);
                for client in self.lock().clients.values() {
                    client.peer.send(payload.clone());
                }
            }
            Some(kind @ ("send" | "close")) => {
                let Some(client_id) = frame.client_id.and_then(json::as_str) else { return };
                let payload = (kind == "send").then(|| text_of(frame.text));
                if let Some(client) = self.lock().clients.get(client_id.as_ref()) {
                    match payload {
                        Some(payload) => client.peer.send(payload),
                        None => client.peer.close(),
                    }
                }
            }
            _ => {}
        }
    }

    /// A connection ended.
    pub(crate) async fn closed(&self, role: &Role) {
        match role {
            Role::Client(id) => self.close_client(id),
            Role::Host(id) => {
                let network = {
                    let mut state = self.lock();
                    if state.host.as_ref().map(|host| host.id) != Some(*id) {
                        return;
                    }
                    state.host = None;
                    for (_, client) in state.clients.drain() {
                        client.peer.close();
                    }
                    drop(state);
                    self.listener.get().is_some_and(Listener::network)
                };
                // Losing the host returns the relay to this computer.
                if network {
                    let _ = self.set_network(false).await;
                }
            }
        }
    }

    /// A client is gone. The host hears of it once, if the client was still known.
    pub(crate) fn close_client(&self, client_id: &str) {
        let mut state = self.lock();
        if state.clients.remove(client_id).is_none() {
            return;
        }
        if let Some(host) = &state.host {
            host.peer.send(ToHost::ClientClose { client_id }.encode());
        }
    }

    /// Rebinds to every interface or to this computer only, and answers the host's
    /// `network` frame. A rebind that fails falls back to this computer and says why.
    /// `None` means even that failed: the relay is no longer listening.
    async fn set_network(&self, enabled: bool) -> Option<Bytes> {
        let _switching = self.switching.lock().await;
        let listener = self.listener.get()?;
        if enabled != listener.network()
            && let Err(error) = listener.open(enabled).await
        {
            if let Err(fatal) = listener.open(false).await {
                self.fail(fatal);
                return None;
            }
            let _ = self.follow_network(false).await;
            return Some(ToHost::Network { enabled: false, addresses: Vec::new(), error: Some(error.to_string()) }.encode());
        }
        if !enabled {
            for client in self.lock().clients.values().filter(|client| !client.address.is_loopback()) {
                client.peer.close();
            }
        }
        let addresses = if enabled { net::local_addresses() } else { Vec::new() };
        let error = self.follow_network(enabled).await;
        Some(ToHost::Network { enabled, addresses, error }.encode())
    }

    /// The API listener follows the relay's network switch. Returns the reason if it could not.
    async fn follow_network(&self, enabled: bool) -> Option<String> {
        let api = self.api_listener.get()?;
        if enabled == api.network() {
            return None;
        }
        let error = api.open(enabled).await.err()?;
        match api.open(false).await {
            Ok(()) => Some(format!("Yomitan API stays on this computer: {error}")),
            Err(again) => Some(again.to_string()),
        }
    }

    /// The relay can no longer listen. Ends the host's connection and tells whoever waits on [`Relay::closed`].
    fn fail(&self, error: io::Error) {
        tracing::error!(%error, "the relay could not listen on this computer again");
        if let Some(host) = &self.lock().host {
            host.peer.close();
        }
        self.fatal.send_replace(Some(Arc::new(error)));
    }

    /// Text traffic keeps a Chrome service worker alive.
    fn ping(&self) {
        let ping = Bytes::from_static(PING);
        let state = self.lock();
        if let Some(host) = &state.host {
            host.peer.send(ping.clone());
        }
        for client in state.clients.values() {
            client.peer.send(ping.clone());
        }
    }

    /// Closes every connection (shutdown).
    pub(crate) fn close_all(&self) {
        let mut state = self.lock();
        if let Some(host) = state.host.take() {
            host.peer.close();
        }
        for (_, client) in state.clients.drain() {
            client.peer.close();
        }
    }
}

/// Binds and starts everything a [`Builder`] describes.
pub(crate) fn start(builder: Builder) -> Result<Relay, Error> {
    let Builder { port, api_port, ping_interval, lookup_timeout, chunk_timeout, version, limits } = builder;
    let bind_error = |source| Error::Bind { port, source };
    let listener = net::bind(net::listen_address(false, port)).map_err(bind_error)?;
    let port = listener.local_addr().map_err(bind_error)?.port();
    let (shutdown, stop) = watch::channel(false);
    let (fatal, fatal_watch) = watch::channel(None);
    let shared = Arc::new(Shared::new(port, limits, fatal));

    let handler: Handler = {
        let shared = shared.clone();
        Arc::new(move |stream, peer| {
            tokio::spawn(ws::conn::serve(shared.clone(), stream, peer));
        })
    };
    let relay_listener = Listener::spawn(listener, handler, stop.clone()).map_err(bind_error)?;
    let _ = shared.listener.set(relay_listener);

    let api = match api_port {
        None => ApiStatus::Disabled,
        Some(api_port) => match net::bind(net::listen_address(false, api_port)) {
            Err(error) => ApiStatus::Failed(Arc::new(error)),
            Ok(api_socket) => {
                let server = api::Server::new(shared.clone(), version, lookup_timeout, chunk_timeout, stop.clone());
                match Listener::spawn(api_socket, api::handler(server), stop.clone()) {
                    Ok(api_listener) => {
                        let bound = api_listener.port();
                        let _ = shared.api_listener.set(api_listener);
                        ApiStatus::Listening(bound)
                    }
                    Err(error) => ApiStatus::Failed(Arc::new(error)),
                }
            }
        },
    };

    tokio::spawn(ping_forever(shared.clone(), ping_interval, stop));
    Ok(Relay { shared, port, api, shutdown, fatal: fatal_watch })
}

async fn ping_forever(shared: Arc<Shared>, interval: Duration, mut stop: watch::Receiver<bool>) {
    let interval = interval.max(Duration::from_millis(1));
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = crate::until_true(&mut stop) => return,
            _ = ticks.tick() => shared.ping(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_frames_encode_with_upstream_field_names() {
        let open = ToHost::ClientOpen { client_id: "client-1", origin: "hoshi://hoshidicts", address: "127.0.0.1".into() }.encode();
        assert_eq!(
            open,
            Bytes::from_static(br#"{"kind":"client-open","clientId":"client-1","origin":"hoshi://hoshidicts","address":"127.0.0.1"}"#)
        );
        let failed = ToHost::ListenFailed { error: ANOTHER_HOST }.encode();
        assert!(failed.starts_with(br#"{"kind":"listen-failed","error":"Another"#));
        let network = ToHost::Network { enabled: false, addresses: Vec::new(), error: None }.encode();
        assert_eq!(network, Bytes::from_static(br#"{"kind":"network","enabled":false,"addresses":[]}"#));
        let text = ToHost::ClientText { client_id: "client-2", text: "a\"b" }.encode();
        assert_eq!(text, Bytes::from_static(br#"{"kind":"client-text","clientId":"client-2","text":"a\"b"}"#));
    }

    #[test]
    fn host_frame_text_passes_strings_through_and_shows_other_values_as_json() {
        let frame: HostFrame<'_> = serde_json::from_str(r#"{"kind":"send","clientId":"client-1","text":"{\"kind\":\"reply\"}"}"#)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(text_of(frame.text), Bytes::from_static(br#"{"kind":"reply"}"#));
        let frame: HostFrame<'_> = serde_json::from_str(r#"{"kind":"broadcast","text":{"a":1}}"#).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(text_of(frame.text), Bytes::from_static(br#"{"a":1}"#));
        assert_eq!(text_of(None), Bytes::from_static(b"null"));
    }
}
