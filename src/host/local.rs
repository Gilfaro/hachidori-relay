// SPDX-License-Identifier: GPL-3.0-or-later
//! The relay's own sharing host: what a linked browser talks to in host mode.
//!
//! It speaks the client side of sharing protocol 1 as the extension's
//! `sharing-host.js` does: a `hello` with the mirrored snapshot, `storage`
//! broadcasts whenever the store changes, and a `reply` for every `request`.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use bytes::Bytes;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

use super::anki::{self, Anki};
use super::engine::{self, Engine};
use super::remote::{self, Fetch};
use super::settings::worker::{self, Storage};
use super::setup::Setup;
use super::store::{self, Store};
use super::updates::{self, Updates};
use super::upload::{self, Request, Uploads};
use super::yomitan::{self, Api};
use crate::json;
use crate::relay::{LocalHost, Shared};

const PROTOCOL_VERSION: u64 = 1;
/// What linked browsers call this host.
pub(crate) const NAME: &str = "Hachidori Relay";
/// The answer to a request this relay does not serve (yet).
pub(crate) const UNSUPPORTED: &str = "This request is not supported by the Hachidori relay.";
/// The host contract's API capability (`API_CAPABILITY`): this host answers `hd_api_*`.
const CAPABILITY_API: &str = "hoshidicts-api-v1";

/// A frame from a linked client. Everything else in it is ignored.
#[derive(Deserialize)]
struct ClientFrame<'a> {
    #[serde(default, borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(default, borrow, deserialize_with = "json::present")]
    protocol: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "json::present")]
    id: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "json::present")]
    message: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "json::present")]
    capabilities: Option<&'a RawValue>,
}

/// One linked client.
#[derive(Debug, Default)]
struct Client {
    origin: String,
    address: Option<IpAddr>,
    greeted: bool,
    /// What its hello said it supports (`parseCapabilities`).
    capabilities: Vec<String>,
}

/// `parseCapabilities`: a hello's list of short, distinct strings.
fn capabilities(raw: Option<&RawValue>) -> Option<Vec<String>> {
    let Some(raw) = raw else { return Some(Vec::new()) };
    let items: Vec<Value> = serde_json::from_str(raw.get()).ok()?;
    if items.len() > 32 {
        return None;
    }
    let mut list: Vec<String> = Vec::new();
    for item in items {
        let item = item.as_str().filter(|item| !item.is_empty() && item.encode_utf16().count() <= 100)?.to_owned();
        if !list.contains(&item) {
            list.push(item);
        }
    }
    Some(list)
}

/// The store as the worker requests see it: their writes are published to linked clients.
pub(crate) struct Published {
    anki: Arc<Anki>,
}

impl Storage for Published {
    fn read(&self, key: &str) -> Option<Value> {
        self.anki.read(key)
    }

    fn write(&self, values: Map<String, Value>) -> Result<(), String> {
        self.anki.publish(values).map(drop).map_err(|error| format!("could not save the relay's storage: {error}"))
    }
}

/// Every write to the store, told to linked clients in the order it was stored.
pub(crate) struct Publisher {
    store: Arc<Store>,
    relay: Weak<Shared>,
}

impl Publisher {
    pub(crate) fn read(&self, key: &str) -> Option<Value> {
        self.store.get(key)
    }

    /// Stores `changes` and tells every linked client.
    pub(crate) fn publish(&self, changes: Map<String, Value>) -> std::io::Result<Map<String, Value>> {
        self.publish_with(|_| changes)
    }

    /// Stores the changes `derive` makes of what is stored, as one write, and tells
    /// every linked client: shared keys as a `storage` frame, and a duplicate index
    /// change as a `word-status` frame.
    pub(crate) fn publish_with(
        &self,
        derive: impl FnOnce(&BTreeMap<String, Value>) -> Map<String, Value>,
    ) -> std::io::Result<Map<String, Value>> {
        let previous = RefCell::new(None);
        self.store.apply_with(
            |storage| {
                let mut changes = derive(storage);
                anki::derive_index(storage, &mut changes);
                *previous.borrow_mut() = anki::stored_index(storage);
                changes
            },
            |changed| {
                let Some(relay) = self.relay.upgrade() else { return };
                let shared: Map<String, Value> =
                    changed.iter().filter(|(key, _)| store::is_shared_key(key)).map(|(k, v)| (k.clone(), v.clone())).collect();
                if !shared.is_empty() {
                    relay.broadcast_local(&Bytes::from(owned_json!({ "kind": "storage", "changes": shared }).to_string()));
                }
                if let Some(frame) = anki::word_status(previous.borrow().as_ref(), changed) {
                    relay.broadcast_local(&Bytes::from(frame.to_string()));
                }
            },
        )
    }
}

/// The relay's sharing host.
pub(crate) struct Local {
    relay: Weak<Shared>,
    store: Arc<Store>,
    engine: Arc<Engine>,
    api: Arc<Api>,
    version: String,
    clients: Mutex<HashMap<Arc<str>, Client>>,
    uploads: Arc<Uploads>,
    anki: Arc<Anki>,
    /// One storage request at a time (`serialiseStorage`).
    storage: Arc<Mutex<()>>,
    /// Where remote imports download from.
    fetch: Arc<dyn Fetch>,
    updates: Arc<Updates<Published>>,
    setup: Arc<Setup<Published>>,
}

impl Local {
    pub(crate) fn new(relay: &Arc<Shared>, store: Arc<Store>, version: &str) -> Arc<Self> {
        let engine_store = store.clone();
        let engine = Arc::new(Engine::new(store.clone()));
        let anki = Anki::new(Arc::new(Publisher { store: engine_store.clone(), relay: Arc::downgrade(relay) }), engine.clone());
        let api = Arc::new(Api::new(engine.clone(), anki.clone(), store.clone(), version));
        let (storage, fetch): (Arc<Mutex<()>>, Arc<dyn Fetch>) = (Arc::default(), Arc::new(remote::Http));
        let updates = Arc::new(Updates::new(Published { anki: anki.clone() }, storage.clone(), engine.clone(), fetch.clone()));
        let setup = Arc::new(Setup::new(Published { anki: anki.clone() }, storage.clone(), engine.clone(), fetch.clone()));
        Arc::new(Self {
            relay: Arc::downgrade(relay),
            store,
            engine,
            api,
            version: format!("hachidori-relay/{version}"),
            clients: Mutex::default(),
            uploads: Uploads::new(Arc::clone(&engine_store)),
            anki,
            storage,
            fetch,
            updates,
            setup,
        })
    }

    /// Starts what runs on its own: the duplicate index's refresh and the update schedule.
    pub(crate) fn start(&self) {
        self.anki.reconcile();
        updates::run_schedule(self.updates.clone(), self.relay.clone());
    }

    fn send(&self, client: &str, frame: &Value) {
        if let Some(relay) = self.relay.upgrade() {
            relay.send_to_client(client, Bytes::from(frame.to_string()));
        }
    }

    /// Stores `changes` and tells every linked client, in the order they were stored.
    pub(crate) fn publish(&self, changes: Map<String, Value>) -> std::io::Result<Map<String, Value>> {
        self.anki.publish(changes)
    }

    fn published(&self) -> Published {
        Published { anki: self.anki.clone() }
    }

    /// The hello for one client. Built while the store is consistent with what it has broadcast.
    fn hello(&self) -> Value {
        let snapshot = self.store.snapshot();
        let dictionary_count = snapshot.get(store::DICTIONARY_STATE).map_or(0, |state| store::generations(state).len());
        owned_json!({
            "kind": "hello",
            "protocol": PROTOCOL_VERSION,
            "version": self.version.as_str(),
            "name": NAME,
            "dictionaryCount": dictionary_count,
            "capabilities": json!([CAPABILITY_API, upload::CAPABILITY, worker::LINKED_ANKI_CAPABILITY]),
            "snapshot": snapshot,
        })
    }

    fn bye(&self, client: &str, reason: &str) {
        self.send(client, &json!({ "kind": "bye", "reason": reason }));
        if let Some(relay) = self.relay.upgrade() {
            relay.close_local_client(client);
        }
    }

    /// `dispatchSharedRequest`: answers one request, off the runtime when it needs the engine.
    fn request(&self, client: &str, id: &RawValue, message: Option<&RawValue>) {
        let message: Value = message.and_then(|raw| serde_json::from_str(raw.get()).ok()).unwrap_or(Value::Null);
        let id: Value = serde_json::from_str(id.get()).unwrap_or(Value::Null);
        let reply = move |relay: &Weak<Shared>, client: &str, response: Value| {
            if let Some(relay) = relay.upgrade() {
                relay.send_to_client(client, Bytes::from(reply_frame(&id, &response)));
            }
        };
        match route(&message) {
            Route::Engine => {
                let (engine, relay, client) = (self.engine.clone(), self.relay.clone(), client.to_owned());
                tokio::task::spawn_blocking(move || reply(&relay, &client, engine.handle(&message)));
            }
            Route::Api => {
                let (api, relay, client) = (self.api.clone(), self.relay.clone(), client.to_owned());
                tokio::task::spawn_blocking(move || {
                    let response = api.answer(&message).unwrap_or_else(|error| failure(&message, &error));
                    reply(&relay, &client, response);
                });
            }
            Route::Worker => {
                let capabilities =
                    self.clients.lock().unwrap_or_else(PoisonError::into_inner).get(client).map(|entry| entry.capabilities.clone());
                let storage = self.published();
                let (lock, relay, client) = (self.storage.clone(), self.relay.clone(), client.to_owned());
                tokio::task::spawn_blocking(move || {
                    let response = {
                        let _serialised = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        worker::handle(&storage, &message, &capabilities.unwrap_or_default())
                    };
                    reply(&relay, &client, response);
                });
            }
            Route::Mutation => {
                let storage = self.published();
                let (lock, engine, relay, client) = (self.storage.clone(), self.engine.clone(), self.relay.clone(), client.to_owned());
                tokio::task::spawn_blocking(move || {
                    let response = {
                        let _serialised = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        engine.mutate(&storage, &message)
                    };
                    reply(&relay, &client, response);
                });
            }
            Route::RemoteImport => {
                let storage = self.published();
                let (lock, engine, fetch, relay, client) =
                    (self.storage.clone(), self.engine.clone(), self.fetch.clone(), self.relay.clone(), client.to_owned());
                tokio::task::spawn_blocking(move || {
                    reply(&relay, &client, engine.import_remote(&storage, &lock, fetch.as_ref(), &message));
                });
            }
            Route::Updates => {
                let (updates, relay, client) = (self.updates.clone(), self.relay.clone(), client.to_owned());
                tokio::task::spawn_blocking(move || reply(&relay, &client, updates.handle(&message)));
            }
            Route::Setup => {
                let (setup, relay, client) = (self.setup.clone(), self.relay.clone(), client.to_owned());
                tokio::task::spawn_blocking(move || reply(&relay, &client, setup.handle(&message)));
            }
            Route::Upload => self.upload(client, message, reply),
            Route::Anki => {
                let (anki, relay, client) = (self.anki.clone(), self.relay.clone(), client.to_owned());
                tokio::task::spawn_blocking(move || {
                    let response = match (message["target"].as_str(), message["type"].as_str()) {
                        (Some("hoshidicts-worker"), Some("hd_anki_discover")) => anki.discover(&message),
                        (Some("hoshidicts-worker"), Some("hd_anki_setup")) => anki.setup(&message),
                        _ => anki.handle(&message),
                    };
                    reply(&relay, &client, response);
                });
            }
            Route::Fail(error) => reply(&self.relay, client, failure(&message, &error)),
        }
    }

    /// `answerUploadRequest`: one request of a linked browser's dictionary upload.
    fn upload(&self, client: &str, message: Value, reply: impl FnOnce(&Weak<Shared>, &str, Value) + Send + 'static) {
        let request = match upload::allow(&message) {
            Ok(request) => request,
            Err(error) => return reply(&self.relay, client, failure(&message, &error)),
        };
        let owner: Arc<str> = Arc::from(client);
        let (uploads, relay, echo) = (self.uploads.clone(), self.relay.clone(), message.clone());
        // `workerReply`, with the request's own requestId.
        let answered = move |result: Result<Value, String>| match result {
            Ok(Value::Object(payload)) => {
                let mut response = json!({ "type": format!("{}_result", echo["type"].as_str().unwrap_or_default()),
                    "requestId": upload::request_id(&echo), "ok": true, "error": null });
                if let Some(fields) = response.as_object_mut() {
                    fields.extend(payload);
                }
                response
            }
            Ok(other) => other,
            Err(error) => failure(&echo, &error),
        };
        match request {
            Request::Begin { file_name, size, replace } => {
                reply(&relay, client, answered(uploads.begin(&owner, &file_name, size, replace)));
            }
            Request::Abort { token } => reply(&relay, client, answered(Ok(uploads.abort(client, &token)))),
            Request::Chunk { token, offset, data } => {
                tokio::task::spawn_blocking(move || {
                    let result = uploads.chunk(&owner, &token, &offset, &data);
                    reply(&relay, &owner, answered(result));
                });
            }
            Request::Commit { token } => {
                let storage = self.published();
                let (lock, engine) = (self.storage.clone(), self.engine.clone());
                tokio::task::spawn_blocking(move || {
                    let request_id = upload::request_id(&message);
                    let imported = {
                        let _serialised = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        uploads.commit(&owner, &token).and_then(|complete| {
                            engine.import_upload(&storage, &complete.file, &complete.file_name, complete.replace, &request_id)
                        })
                    };
                    let response = match imported {
                        Ok(Value::Object(mut response)) => {
                            response.insert("type".to_owned(), Value::from("hd_import_commit_result"));
                            response.insert("requestId".to_owned(), request_id);
                            Value::Object(response)
                        }
                        Ok(other) => other,
                        Err(error) => failure(&message, &error),
                    };
                    reply(&relay, &owner, response);
                });
            }
        }
    }
}

/// Where a linked client's request goes.
enum Route {
    Engine,
    /// An engine request that changes the dictionaries, which also uses storage.
    Mutation,
    Api,
    Worker,
    /// `hd_import` from an `archiveUrl`.
    RemoteImport,
    /// A `hachidori-updates` request.
    Updates,
    /// A `hachidori-setup` request.
    Setup,
    /// A linked browser's dictionary upload.
    Upload,
    /// A `hachidori-anki` request, or Settings' Anki discovery and setup check.
    Anki,
    Fail(String),
}

/// `FORWARDED_REQUESTS` (`sharing-protocol.js`) for the targets that check it.
fn forwarded(target: &str) -> Option<&'static [&'static str]> {
    Some(match target {
        "hoshidicts-offscreen" => &[
            "hd_lookup",
            "hd_lookup_dictionary",
            "hd_segment",
            "hd_kanji",
            "hd_styles",
            "hd_media",
            "hd_status",
            "hd_memory",
            "hd_memory_total",
            "hd_custom_append",
            "hd_custom_save",
            "hd_apply_state",
            "hd_reload",
            "hd_remove",
            "hd_import",
        ],
        "hoshidicts-worker" => &worker::REQUESTS,
        "hachidori-updates" => &["hd_updates_schedule", "hd_updates_check", "hd_updates_install"],
        "hachidori-setup" => &["hd_setup_install"],
        _ => return None,
    })
}

/// JavaScript's `JSON.stringify` of a field that may be missing.
fn shown(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".to_owned(), Value::to_string)
}

fn route(message: &Value) -> Route {
    let target = message.get("target");
    let kind = message.get("type").and_then(Value::as_str).unwrap_or_default();
    if target.and_then(Value::as_str) == Some("hoshidicts-offscreen") && yomitan::REQUESTS.contains(&kind) {
        return Route::Api;
    }
    if target.and_then(Value::as_str) == Some("hoshidicts-worker") && matches!(kind, "hd_anki_discover" | "hd_anki_setup") {
        return Route::Anki;
    }
    let Some(types) = target.and_then(Value::as_str).and_then(forwarded) else {
        return match target.and_then(Value::as_str) {
            Some(upload::TARGET) => Route::Upload,
            Some("hachidori-anki") => Route::Anki,
            _ => Route::Fail(format!("unsupported shared request target {}", shown(target))),
        };
    };
    let archive_ok = kind != "hd_import" || (message.get("archiveUrl").is_some_and(Value::is_string) && message.get("blobUrl").is_none());
    if !types.contains(&kind) || !archive_ok {
        return Route::Fail(format!("unsupported shared request {} {}", shown(target), shown(message.get("type"))));
    }
    match target.and_then(Value::as_str) {
        Some("hachidori-updates") => return Route::Updates,
        Some("hachidori-setup") => return Route::Setup,
        Some("hoshidicts-offscreen") if kind == "hd_import" => return Route::RemoteImport,
        _ => {}
    }
    if target.and_then(Value::as_str) == Some("hoshidicts-offscreen") && engine::REQUESTS.contains(&kind) {
        Route::Engine
    } else if target.and_then(Value::as_str) == Some("hoshidicts-offscreen") && engine::MUTATIONS.contains(&kind) {
        Route::Mutation
    } else if target.and_then(Value::as_str) == Some("hoshidicts-worker") && worker::REQUESTS.contains(&kind) {
        Route::Worker
    } else {
        Route::Fail(UNSUPPORTED.to_owned())
    }
}

/// The extension's `failureReply`: `{type: "<type>_result", requestId, ok: false, error}`.
/// `{ kind: "reply", id, response }` as text, written from the parts as they are.
fn reply_frame(id: &Value, response: &Value) -> String {
    #[derive(serde::Serialize)]
    struct Reply<'a> {
        kind: &'static str,
        id: &'a Value,
        response: &'a Value,
    }
    serde_json::to_string(&Reply { kind: "reply", id, response }).unwrap_or_default()
}

pub(crate) fn failure(message: &Value, error: &str) -> Value {
    let kind = message.get("type").and_then(Value::as_str).unwrap_or("hd_unknown");
    json!({
        "type": format!("{kind}_result"),
        "requestId": message.get("requestId").cloned().unwrap_or(Value::Null),
        "ok": false,
        "error": error,
        "generation": 0,
    })
}

impl LocalHost for Local {
    fn opened(&self, client: &Arc<str>, origin: &str, address: IpAddr) {
        let entry = Client { origin: origin.to_owned(), address: Some(address), ..Client::default() };
        self.clients.lock().unwrap_or_else(PoisonError::into_inner).insert(client.clone(), entry);
        tracing::info!(%client, origin, %address, "a browser linked to the relay");
    }

    fn shutdown(&self) {
        if let Err(error) = self.store.flush() {
            tracing::warn!(%error, "could not write the storage before shutting down");
        }
    }

    fn text(&self, client: &Arc<str>, text: &str) {
        let frame = json::is_object_text(text).then(|| serde_json::from_str::<ClientFrame<'_>>(text).ok()).flatten();
        let Some(frame) = frame else { return self.bye(client, "malformed sharing frame") };
        match frame.kind.as_deref() {
            Some("hello") => {
                let protocol = frame.protocol.and_then(|raw| raw.get().trim().parse::<u64>().ok());
                if protocol != Some(PROTOCOL_VERSION) {
                    let shown = frame.protocol.map_or("undefined", RawValue::get);
                    return self.bye(client, &format!("unsupported sharing protocol {shown}"));
                }
                let Some(capabilities) = capabilities(frame.capabilities) else {
                    return self.bye(client, "malformed sharing capabilities");
                };
                if let Some(entry) = self.clients.lock().unwrap_or_else(PoisonError::into_inner).get_mut(client) {
                    entry.greeted = true;
                    entry.capabilities = capabilities;
                }
                self.send(client, &self.hello());
            }
            Some("request") => match frame.id {
                Some(id) if json::as_str(id).is_some() || id.get().trim().parse::<f64>().is_ok() => self.request(client, id, frame.message),
                _ => self.bye(client, "malformed sharing request"),
            },
            Some("pong") => {}
            Some(other) => self.bye(client, &format!("unknown sharing frame {}", Value::from(other))),
            None => self.bye(client, "unknown sharing frame undefined"),
        }
    }

    fn closed(&self, client: &Arc<str>) {
        self.uploads.drop_owner(client);
        let gone = self.clients.lock().unwrap_or_else(PoisonError::into_inner).remove(client);
        if let Some(gone) = gone {
            tracing::info!(%client, origin = gone.origin, address = ?gone.address, greeted = gone.greeted, "a linked browser left");
        }
    }
}
