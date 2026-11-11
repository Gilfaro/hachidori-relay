// SPDX-License-Identifier: GPL-3.0-or-later
//! Host mode end to end inside one process: a browser host and a linked browser
//! talk to the relay through in-memory peers. `tests/conformance/host.test.mjs` runs the same story
//! over real sockets against the binary.
#![allow(clippy::expect_used, reason = "a failed expectation is a failed test")]

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use super::archive::tests::archive;
use super::{HostOptions, start};
use crate::Limits;
use crate::relay::{Role, Shared};
use crate::ws::peer::{ApiEvent, Outgoing, Peer, SocketPeer};

const WAIT: Duration = Duration::from_secs(10);
const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const GENERATION_1: &str = "/dicts/.hdw-generation-1/Fixture";
const GENERATION_2: &str = "/dicts/.hdw-generation-2/Fixture";

fn data_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hachidori-relay-unit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A relay in host mode, without sockets.
fn relay(dir: &Path) -> (Arc<Shared>, watch::Sender<bool>) {
    let relay = Arc::new(Shared::new(0, Limits::default(), watch::channel(None).0));
    let (stop, stopped) = watch::channel(false);
    // These tests read storage.json as soon as a change is made.
    let options = HostOptions { write_delay: Duration::ZERO, ..HostOptions::new(dir) };
    start(&relay, &options, "0.1.0", WAIT, WAIT, stopped).unwrap_or_else(|error| panic!("{error}"));
    (relay, stop)
}

fn dictionary_state(path: &str) -> Value {
    json!({ "dictionaries": [{ "id": "fixture", "title": "Fixture", "revision": "1", "path": path }], "groups": [], "revision": 1 })
}

fn files(blobs: &[u8]) -> Vec<(&'static str, &[u8])> {
    vec![(".hoshidicts_5", b""), ("index.json", b"{}"), ("blobs.bin", blobs)]
}

/// A linked browser on an in-memory peer.
struct Linked {
    relay: Arc<Shared>,
    role: Role,
    events: mpsc::UnboundedReceiver<ApiEvent>,
}

impl Linked {
    async fn hello(relay: &Arc<Shared>) -> (Self, Value) {
        let (sender, events) = mpsc::unbounded_channel();
        let role = relay.connect_client(Peer::Api(sender), "chrome-extension://linked", LOCALHOST).expect("host mode takes every client");
        let mut linked = Self { relay: relay.clone(), role, events };
        linked.say(&json!({ "kind": "hello", "protocol": 1, "version": "0.9.0", "name": "Firefox", "capabilities": [] })).await;
        let hello = linked.until("hello").await;
        (linked, hello)
    }

    async fn say(&self, frame: &Value) {
        self.relay.text(&self.role, frame.to_string()).await;
    }

    async fn next(&mut self) -> Value {
        match tokio::time::timeout(WAIT, self.events.recv()).await.expect("a frame in time") {
            Some(ApiEvent::Text(text)) => serde_json::from_slice(&text).expect("JSON"),
            Some(ApiEvent::Closed) | None => panic!("closed"),
        }
    }

    async fn until(&mut self, kind: &str) -> Value {
        loop {
            let frame = self.next().await;
            if frame["kind"] == kind {
                return frame;
            }
        }
    }
}

/// A browser host on an in-memory peer, answering the relay's pull session.
struct Browser {
    relay: Arc<Shared>,
    role: Role,
    frames: mpsc::UnboundedReceiver<Outgoing>,
    client: String,
}

impl Browser {
    async fn connect(relay: &Arc<Shared>, snapshot: Value) -> Self {
        Self::offering(relay, snapshot, &["hoshidicts-api-v1"]).await
    }

    async fn offering(relay: &Arc<Shared>, snapshot: Value, capabilities: &[&str]) -> Self {
        let (peer, frames) = SocketPeer::new(usize::MAX);
        let role = relay.connect_host(Peer::Socket(peer)).expect("the browser host is accepted");
        let mut browser = Self { relay: relay.clone(), role, frames, client: String::new() };
        assert_eq!(browser.next().await["kind"], "listening");
        let open = browser.next().await;
        assert_eq!(open["kind"], "client-open");
        assert_eq!(open["origin"], "relay://hachidori-relay");
        browser.client = open["clientId"].as_str().expect("id").to_owned();
        assert_eq!(browser.said().await["kind"], "hello");
        browser
            .says(&json!({ "kind": "hello", "protocol": 1, "version": "0.9.0", "name": "Chrome", "capabilities": capabilities, "snapshot": snapshot }))
            .await;
        browser
    }

    async fn next(&mut self) -> Value {
        let frame = tokio::time::timeout(WAIT, self.frames.recv()).await.expect("a frame in time").expect("open");
        serde_json::from_slice(&frame.payload).expect("JSON")
    }

    /// The next frame from the pull session.
    async fn said(&mut self) -> Value {
        loop {
            let frame = self.next().await;
            if frame["kind"] == "client-text" {
                return serde_json::from_str(frame["text"].as_str().expect("text")).expect("JSON");
            }
        }
    }

    async fn says(&self, frame: &Value) {
        let wrapped = json!({ "kind": "send", "clientId": self.client, "text": frame.to_string() });
        self.relay.text(&self.role, wrapped.to_string()).await;
    }

    async fn broadcast(&self, frame: &Value) {
        self.relay.text(&self.role, json!({ "kind": "broadcast", "text": frame.to_string() }).to_string()).await;
    }

    async fn serve_download(&mut self, bytes: &[u8]) {
        let open = self.said().await;
        assert_eq!(open["message"]["type"], "hd_api_dictionary_open");
        assert_eq!(open["message"]["id"], "fixture");
        self.says(&json!({ "kind": "reply", "id": open["id"], "response": { "token": "dl-1", "size": bytes.len() } })).await;
        let half = bytes.len() / 2;
        for (offset, part, eof) in [(0, &bytes[..half], false), (half, &bytes[half..], true)] {
            let read = self.said().await;
            assert_eq!(read["message"]["type"], "hd_api_dictionary_read");
            assert_eq!(read["message"]["offset"], offset);
            let data = base64::engine::general_purpose::STANDARD.encode(part);
            self.says(&json!({ "kind": "reply", "id": read["id"], "response": { "data": data, "eof": eof } })).await;
        }
        let close = self.said().await;
        assert_eq!(close["message"]["type"], "hd_api_dictionary_close");
    }

    async fn leave(&self) {
        self.relay.closed(&self.role).await;
    }
}

fn stored(dir: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(dir.join("storage.json")).expect("storage.json")).expect("JSON")
}

fn generation_dir(dir: &Path, path: &str) -> PathBuf {
    dir.join("dictionaries").join(sha1_smol::Sha1::from(path.as_bytes()).digest().to_string())
}

fn installed(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("dictionaries")).expect("dictionaries").count()
}

#[tokio::test]
async fn the_browser_host_is_mirrored_and_the_mirror_outlives_it() {
    let dir = data_dir("mirror");
    let (relay, stop) = relay(&dir);
    let (mut linked, hello) = Linked::hello(&relay).await;
    assert_eq!(hello["name"], "Hachidori Relay");
    assert_eq!(hello["dictionaryCount"], 0);
    assert_eq!(hello["snapshot"]["dictionaryState"], Value::Null);

    let snapshot = json!({ "dictionaryState": dictionary_state(GENERATION_1), "options": { "scanLength": 20 }, "lookupStats": { "generation": "g", "revision": 1 } });
    let mut browser = Browser::connect(&relay, snapshot).await;
    let settings = linked.until("storage").await;
    assert_eq!(settings["changes"]["options"], json!({ "scanLength": 20 }));
    assert!(settings["changes"].get("dictionaryState").is_none(), "the list waits for its files");
    browser.serve_download(&archive("fixture", GENERATION_1, &files(b"first"))).await;
    assert_eq!(linked.until("storage").await["changes"]["dictionaryState"], dictionary_state(GENERATION_1));
    assert_eq!(std::fs::read(generation_dir(&dir, GENERATION_1).join("blobs.bin")).ok(), Some(b"first".to_vec()));
    assert_eq!(stored(&dir)["dictionaryState"], dictionary_state(GENERATION_1));

    browser.broadcast(&json!({ "kind": "storage", "changes": { "options": { "scanLength": 30 }, "lookupStats:g:a": { "lookupCount": 2 }, "sharing": {} } })).await;
    let change = linked.until("storage").await;
    assert_eq!(change["changes"], json!({ "options": { "scanLength": 30 }, "lookupStats:g:a": { "lookupCount": 2 } }));
    assert!(stored(&dir).get("sharing").is_none());

    browser.broadcast(&json!({ "kind": "storage", "changes": { "dictionaryState": dictionary_state(GENERATION_2) } })).await;
    browser.serve_download(&archive("fixture", GENERATION_2, &files(b"second"))).await;
    assert_eq!(linked.until("storage").await["changes"]["dictionaryState"], dictionary_state(GENERATION_2));
    assert_eq!(installed(&dir), 1, "the replaced generation is removed");
    assert!(generation_dir(&dir, GENERATION_2).is_dir());

    browser.leave().await;
    // Managed updates are answered here: with nothing managed installed, a check finds nothing.
    linked.say(&json!({ "kind": "request", "id": 7, "message": { "target": "hachidori-updates", "type": "hd_updates_check", "requestId": "q" } })).await;
    let checked = linked.until("reply").await["response"].clone();
    assert_eq!((&checked["type"], &checked["ok"], &checked["outcomes"]), (&json!("hd_updates_check_result"), &json!(true), &json!([])));
    assert_eq!(checked["settings"]["schedule"], "off");
    // An import from a URL downloads only what a managed update or the catalogue names.
    let remote =
        json!({ "target": "hoshidicts-offscreen", "type": "hd_import", "requestId": "q", "archiveUrl": "https://example.com/d.zip" });
    linked.say(&json!({ "kind": "request", "id": 8, "message": remote })).await;
    let refused = linked.until("reply").await["response"].clone();
    assert_eq!(refused["ok"], false);
    assert_eq!(refused["error"], "The dictionary archive: checking the import request failed. the import request carried no archive URL");
    // The recommended-dictionary installer answers; an empty request only observes.
    linked
        .say(&json!({ "kind": "request", "id": 9,
        "message": { "target": "hachidori-setup", "type": "hd_setup_install", "requestId": "q", "sourceIds": [] } }))
        .await;
    assert_eq!(
        linked.until("reply").await["response"],
        json!({ "type": "hd_setup_install_result", "requestId": "q", "ok": true, "error": null,
            "runId": null, "sequence": 0, "finished": true, "entries": [] })
    );
    // A request no browser sends is still named as unsupported.
    linked.say(&json!({ "kind": "request", "id": 11, "message": { "target": "hachidori-setup", "type": "hd_setup_other" } })).await;
    assert_eq!(
        linked.until("reply").await["response"]["error"],
        json!("unsupported shared request \"hachidori-setup\" \"hd_setup_other\"")
    );
    stop.send_replace(true);

    // A new relay on the same directory, and no browser host at all.
    let (relay, _stop) = self::relay(&dir);
    let (_linked, hello) = Linked::hello(&relay).await;
    assert_eq!(hello["dictionaryCount"], 1);
    assert_eq!(hello["snapshot"]["options"], json!({ "scanLength": 30 }));
    assert_eq!(hello["snapshot"]["dictionaryState"], dictionary_state(GENERATION_2));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_browser_host_with_profiles_is_mirrored_as_stored() {
    let dir = data_dir("opt-in");
    let (relay, stop) = relay(&dir);
    let (mut linked, _) = Linked::hello(&relay).await;
    let japanese = json!({ "options": { "scanLength": 20 } });
    let mut browser = Browser::offering(&relay, japanese, &["hoshidicts-api-v1", "language-profiles-v1"]).await;
    let opt_in = browser.said().await;
    assert_eq!(opt_in["message"]["type"], "hd_language_profiles_opt_in");
    let languages = opt_in["message"]["languages"].as_array().expect("languages");
    assert!(languages.contains(&json!("en")) && !languages.contains(&json!("ja")), "{languages:?}");
    // A change frame before the reply is the Japanese view, older than the reply.
    browser.broadcast(&json!({ "kind": "storage", "changes": { "options": { "scanLength": 21 } } })).await;
    let options = json!({ "scanLength": 40, "profiles": { "en": { "scanLength": 9 } } });
    browser.says(&json!({ "kind": "reply", "id": opt_in["id"], "response": { "ok": true, "optIn": "accepted", "snapshot": { "options": options } } })).await;
    assert_eq!(linked.until("storage").await["changes"]["options"], options, "only the records as stored are mirrored");
    assert_eq!(stored(&dir)["options"], options);
    browser.leave().await;
    let _ = stop.send(true);
}

#[tokio::test]
async fn a_failed_download_publishes_nothing_until_a_newer_list_installs() {
    let dir = data_dir("retry");
    let (relay, _stop) = relay(&dir);
    let (mut linked, _) = Linked::hello(&relay).await;
    let mut browser = Browser::connect(&relay, json!({ "dictionaryState": dictionary_state(GENERATION_1), "options": {} })).await;
    linked.until("storage").await;
    let open = browser.said().await;
    browser.says(&json!({ "kind": "reply", "id": open["id"], "response": { "error": "busy" } })).await;
    tokio::task::yield_now().await;
    assert!(stored(&dir).get("dictionaryState").is_none());

    browser.broadcast(&json!({ "kind": "storage", "changes": { "dictionaryState": dictionary_state(GENERATION_2) } })).await;
    browser.serve_download(&archive("fixture", GENERATION_2, &files(b"ok"))).await;
    assert_eq!(linked.until("storage").await["changes"]["dictionaryState"], dictionary_state(GENERATION_2));
    assert_eq!(installed(&dir), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn the_browser_cannot_reach_or_switch_away_linked_browsers() {
    let dir = data_dir("isolation");
    let (relay, _stop) = relay(&dir);
    let (mut linked, _) = Linked::hello(&relay).await;
    let Role::Client(linked_id) = linked.role.clone() else { panic!("a client") };
    let browser = Browser::connect(&relay, json!({})).await;
    // A browser host may only talk to its own clients.
    relay.text(&browser.role, json!({ "kind": "close", "clientId": &*linked_id }).to_string()).await;
    relay.text(&browser.role, json!({ "kind": "send", "clientId": &*linked_id, "text": "{\"kind\":\"x\"}" }).to_string()).await;
    browser.leave().await;
    linked.say(&json!({ "kind": "request", "id": "after", "message": { "target": "t", "type": "hd_x" } })).await;
    let reply = linked.next().await;
    assert_eq!(
        (reply["kind"].as_str(), reply["id"].as_str()),
        (Some("reply"), Some("after")),
        "the linked browser stays, and never saw the browser's frames"
    );
    let _ = std::fs::remove_dir_all(dir);
}
