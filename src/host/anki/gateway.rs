// SPDX-License-Identifier: GPL-3.0-or-later
//! The AnkiConnect gateway (`anki.js`' `createAnkiGateway`): API v6 requests over
//! HTTP, a few lanes per endpoint, and the reader-facing text of AnkiConnect's errors.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::templates::{self, Resolved};
use crate::host::settings::options;

/// `maximumActive`: requests in flight to one endpoint at a time.
const LANES: usize = 4;
/// The gateway's default deadline.
pub(crate) const TIMEOUT_MS: u64 = 10_000;
/// The longest AnkiConnect reply the relay reads.
const MAX_REPLY_BYTES: u64 = 256 * 1024 * 1024;

/// A failed Anki request: its reader-facing message, and whether it was an
/// `AnkiTransportError` (the endpoint could not be reached) and was sent at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) message: String,
    /// `Some(dispatched)` for a transport failure.
    pub(crate) transport: Option<bool>,
}

impl Failure {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), transport: None }
    }

    fn transport(message: impl Into<String>, dispatched: bool) -> Self {
        Self { message: message.into(), transport: Some(dispatched) }
    }

    /// `isUndispatchedAnkiTransportError`.
    pub(crate) fn undispatched(&self) -> bool {
        self.transport == Some(false)
    }

    /// `isAnkiEditorConflict`: AnkiConnect refused, before writing anything, to
    /// change a note that Anki's Browser or an Edit window shows (#698).
    pub(crate) fn editor_conflict(&self) -> bool {
        contains_folded(&self.message, EDITOR_CONFLICT)
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

pub(crate) type Result<T> = std::result::Result<T, Failure>;

/// `isEnvelope`: exactly `{result, error}`, with a string or null error.
fn is_envelope(payload: &Value) -> bool {
    payload.as_object().is_some_and(|object| {
        object.len() == 2 && object.contains_key("result") && object.get("error").is_some_and(|error| error.is_null() || error.is_string())
    })
}

fn invalid_response() -> Failure {
    Failure::new("AnkiConnect returned an invalid response. Check the add-on and retry.")
}

/// `isAnkiClozeRefusal`.
pub(crate) fn is_cloze_refusal(error: &str) -> bool {
    error.eq_ignore_ascii_case("cannot create note for unknown reason")
}

/// `ankiClozeRefusal`.
pub(crate) fn cloze_refusal(refused: &str) -> String {
    format!(
        "{refused} because of its cloze deletions ({{{{c1::…}}}}): a Cloze note type needs one in its cloze field, and no other \
         field or note type may have one. Check the field mapping in Anki Settings."
    )
}

/// The text after `prefix`, matched case-insensitively, if `text` starts with it.
fn after_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}

/// `EDITOR_CONFLICT`, lower-cased.
const EDITOR_CONFLICT: &str = "close the affected editor before this api write";

fn contains_folded(text: &str, needle: &str) -> bool {
    text.to_ascii_lowercase().contains(needle)
}

/// `explainAnkiConnectError`: the reader-facing explanation of a known AnkiConnect
/// error string, or `None`.
pub(crate) fn explain(error: &str) -> Option<String> {
    let keep = |explanation: String| Some(format!("{explanation} (AnkiConnect: {error})"));
    if contains_folded(error, "api key") {
        return Some("AnkiConnect requires a valid API key. Enter the key from its add-on configuration.".to_owned());
    }
    if contains_folded(error, "collection is not available") {
        return keep("Anki has no open collection. Open your profile in Anki, then retry.".to_owned());
    }
    // `.+$`: one or more characters, none of them a line break.
    let rest =
        |prefix: &str| after_prefix(error, prefix).filter(|rest| !rest.is_empty() && !rest.contains(['\n', '\r', '\u{2028}', '\u{2029}']));
    if let Some(deck) = rest("deck was not found: ") {
        return keep(format!("Anki has no deck named “{deck}”. Choose an available deck in Anki Settings."));
    }
    if let Some(model) = rest("model was not found: ") {
        return keep(format!("Anki has no note type named “{model}”. Choose an available note type in Anki Settings."));
    }
    if error.eq_ignore_ascii_case("cannot create note because it is empty") {
        return keep("Anki refused the note because its first field is empty. Map the first field to content this result has.".to_owned());
    }
    if error.eq_ignore_ascii_case("cannot create note because it is a duplicate") {
        return keep("Anki refused the note because a note with the same first field already exists.".to_owned());
    }
    if is_cloze_refusal(error) {
        return Some(cloze_refusal("Anki refused the note"));
    }
    if let Some(id) = rest("note was not found: ") {
        return keep(format!("Anki no longer has note {id}. It was deleted or moved to another collection; refresh and retry."));
    }
    if contains_folded(error, EDITOR_CONFLICT) {
        return keep(
            "Anki did not change the note because its Browser or an Edit window has it open. Close that window, or select another card in the Browser, then try again."
                .to_owned(),
        );
    }
    if contains_folded(error, "unsupported action") || contains_folded(error, "unknown action") {
        return keep("The installed AnkiConnect add-on is too old for this request. Update AnkiConnect in Anki.".to_owned());
    }
    None
}

/// `describeAnkiConnectError`.
pub(crate) fn describe(error: &str) -> String {
    explain(error).unwrap_or_else(|| format!("AnkiConnect: {error}"))
}

/// `unwrap`: an envelope's result, or its error described.
fn unwrap(reply: &Value) -> Result<Value> {
    match reply.get("error") {
        Some(Value::String(error)) => Err(Failure::new(describe(error))),
        _ => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
    }
}

/// `ankiMultiResults`: the sub-action results of one `multi`, failing as the first
/// failed sub-action would.
pub(crate) fn multi_results(replies: &Value) -> Result<Vec<Value>> {
    replies.as_array().map_or(Ok(Vec::new()), |replies| replies.iter().map(unwrap).collect())
}

/// `names`: a deck, note type or field list, deduplicated in Anki's order.
fn names(action: &str, reply: &Value) -> Result<Vec<String>> {
    let result = unwrap(reply)?;
    let invalid = || Failure::new(format!("AnkiConnect returned an invalid {action} list."));
    let list = result.as_array().ok_or_else(invalid)?;
    let mut names: Vec<String> = Vec::new();
    for name in list {
        let name = name.as_str().filter(|name| !crate::host::js::trim(name).is_empty()).ok_or_else(invalid)?;
        if !names.iter().any(|known| known == name) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// What one endpoint's queue holds.
#[derive(Default)]
struct LaneState {
    active: usize,
    waiting: usize,
    /// The transport failure that ended this queue's generation.
    failure: Option<String>,
}

#[derive(Default)]
struct Lane {
    state: Mutex<LaneState>,
    freed: Condvar,
}

impl Lane {
    fn lock(&self) -> MutexGuard<'_, LaneState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Anki's discovery of decks, note types and the configured note type's fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Discovery {
    pub(crate) connected: bool,
    pub(crate) model: String,
    pub(crate) decks: Vec<String>,
    pub(crate) models: Vec<String>,
    pub(crate) fields: Vec<String>,
    pub(crate) errors: Vec<String>,
}

impl Discovery {
    pub(crate) fn to_json(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("connected".to_owned(), json!(self.connected));
        map.insert("model".to_owned(), json!(self.model));
        map.insert("decks".to_owned(), json!(self.decks));
        map.insert("models".to_owned(), json!(self.models));
        map.insert("fields".to_owned(), json!(self.fields));
        map.insert("errors".to_owned(), json!(self.errors));
        map
    }
}

/// The relay's one AnkiConnect gateway.
#[derive(Default)]
pub(crate) struct Gateway {
    lanes: Mutex<HashMap<String, Arc<Lane>>>,
}

impl Gateway {
    fn lanes(&self) -> MutexGuard<'_, HashMap<String, Arc<Lane>>> {
        self.lanes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `invoke`: one AnkiConnect action (or `multi`) to `endpoint` with `api_key`.
    /// A `multi` returns its sub-action envelopes, in order.
    pub(crate) fn invoke(&self, action: &str, params: &Value, api_key: &str, timeout_ms: u64, endpoint: Option<&Value>) -> Result<Value> {
        let default_url = options::defaults()["anki"]["url"].clone();
        let url = options::normalise_anki_connect_url(Some(endpoint.unwrap_or(&default_url)))
            .ok_or_else(|| Failure::new("Enter a valid HTTP or HTTPS AnkiConnect URL in Settings, without a username or password."))?;
        let key = if api_key.is_empty() { String::new() } else { format!(",\"key\":{}", Value::from(api_key)) };
        let actions = (action == "multi").then(|| {
            let entries = params.get("actions").and_then(Value::as_array).cloned().unwrap_or_default();
            // Rebuilt from their action and params, so nothing else reaches the wire.
            entries
                .iter()
                .map(|entry| {
                    let mut parts = Vec::new();
                    if let Some(action) = entry.get("action") {
                        parts.push(format!("\"action\":{action}"));
                    }
                    if let Some(params) = entry.get("params") {
                        parts.push(format!("\"params\":{params}"));
                    }
                    parts.push("\"version\":6".to_owned());
                    format!("{{{}{key}}}", parts.join(","))
                })
                .collect::<Vec<_>>()
        });
        // A null `params` stands for an undefined one, which JSON.stringify leaves out.
        let params = match &actions {
            Some(actions) => format!(",\"params\":{{\"actions\":[{}]}}", actions.join(",")),
            None if params.is_null() => String::new(),
            None => format!(",\"params\":{params}"),
        };
        let body = format!("{{\"action\":{},\"version\":6{params}{key}}}", Value::from(action));
        let result = self.enqueue(&url, &body, timeout_ms)?;
        if let Some(actions) = actions
            && !result.as_array().is_some_and(|replies| replies.len() == actions.len() && replies.iter().all(is_envelope))
        {
            return Err(invalid_response());
        }
        Ok(result)
    }

    fn enqueue(&self, url: &str, body: &str, timeout_ms: u64) -> Result<Value> {
        let lane = Arc::clone(self.lanes().entry(url.to_owned()).or_default());
        {
            let mut state = lane.lock();
            state.waiting += 1;
            while state.failure.is_none() && state.active >= LANES {
                state = lane.freed.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
            state.waiting -= 1;
            if let Some(failure) = &state.failure {
                return Err(Failure::transport(failure.clone(), false));
            }
            state.active += 1;
        }
        let outcome = dispatch(url, body, timeout_ms);
        let mut state = lane.lock();
        state.active -= 1;
        let outcome = match outcome {
            // A sibling's failure aborted this request mid-flight: it may have run.
            _ if state.failure.is_some() => Err(Failure::transport(state.failure.clone().unwrap_or_default(), true)),
            Err(failure) if failure.transport.is_some() => {
                state.failure = Some(failure.message.clone());
                Err(failure)
            }
            other => other,
        };
        let finished = state.failure.is_some() || (state.active == 0 && state.waiting == 0);
        drop(state);
        lane.freed.notify_all();
        if finished {
            let mut lanes = self.lanes();
            if lanes.get(url).is_some_and(|current| Arc::ptr_eq(current, &lane)) {
                lanes.remove(url);
            }
        }
        outcome
    }

    /// `discover`: decks, note types and, speculatively, `model`'s fields, in one `multi`.
    pub(crate) fn discover(&self, config: &Map<String, Value>) -> Discovery {
        let model = config.get("model").and_then(Value::as_str).unwrap_or_default().to_owned();
        let api_key = config.get("apiKey").and_then(Value::as_str).unwrap_or_default();
        let request = json!({ "actions": [
            { "action": "deckNames", "params": {} },
            { "action": "modelNames", "params": {} },
            { "action": "modelFieldNames", "params": { "modelName": model } },
        ] });
        let replies = match self.invoke("multi", &request, api_key, TIMEOUT_MS, config.get("url")) {
            Ok(replies) => replies,
            Err(error) => return Discovery { model, errors: vec![error.message], ..Discovery::default() },
        };
        let mut discovery = Discovery { model, ..Discovery::default() };
        let read = |action: &str, reply: &Value, discovery: &mut Discovery| match names(action, reply) {
            Ok(names) => {
                discovery.connected = true;
                names
            }
            Err(error) => {
                if !discovery.errors.contains(&error.message) {
                    discovery.errors.push(error.message);
                }
                Vec::new()
            }
        };
        discovery.decks = read("deckNames", &replies[0], &mut discovery);
        discovery.models = read("modelNames", &replies[1], &mut discovery);
        discovery.fields =
            if discovery.models.contains(&discovery.model) { read("modelFieldNames", &replies[2], &mut discovery) } else { Vec::new() };
        discovery
    }
}

/// `dispatch`: one POST, read as an API-v6 envelope.
fn dispatch(url: &str, body: &str, timeout_ms: u64) -> Result<Value> {
    let unavailable = |timed_out: bool| {
        Failure::transport(
            if timed_out {
                "AnkiConnect timed out. Check its URL in Settings, open Anki and retry."
            } else {
                "Open Anki with the AnkiConnect add-on installed, check its URL in Settings, then retry."
            },
            true,
        )
    };
    let agent = crate::host::loopback::agent(
        ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_millis(timeout_ms)))
            .build(),
    );
    let response = agent
        .post(url)
        .header("Content-Type", "application/json")
        .send(body)
        .map_err(|error| unavailable(matches!(error, ureq::Error::Timeout(_))))?;
    let status = response.status();
    // `redirect: "error"`: fetch refuses a redirect as it refuses an unreachable endpoint.
    if status.is_redirection() {
        return Err(unavailable(false));
    }
    if !status.is_success() {
        return Err(Failure::new(if status.as_u16() == 403 {
            "AnkiConnect denied permission. Allow this extension in AnkiConnect’s webCorsOriginList, then retry.".to_owned()
        } else {
            format!("AnkiConnect returned HTTP {}.", status.as_u16())
        }));
    }
    let mut bytes = Vec::new();
    if let Err(error) = response.into_body().into_reader().take(MAX_REPLY_BYTES).read_to_end(&mut bytes) {
        let timed_out = error.kind() == std::io::ErrorKind::TimedOut
            || error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<ureq::Error>())
                .is_some_and(|inner| matches!(inner, ureq::Error::Timeout(_)));
        return Err(unavailable(timed_out));
    }
    let payload: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if !is_envelope(&payload) {
        return Err(invalid_response());
    }
    unwrap(&payload)
}

/// `ankiAvailability`: what keeps `config` from adding notes, given Anki's discovery.
pub(crate) fn availability(config: &Map<String, Value>, discovery: Option<&Discovery>, resolved: Option<&Resolved>) -> Vec<String> {
    let Some(discovery) = discovery else { return vec!["Refresh Anki to check this configuration.".to_owned()] };
    if !discovery.connected {
        return discovery.errors.clone();
    }
    let text = |key: &str| config.get(key).and_then(Value::as_str).unwrap_or_default();
    let (deck, model) = (text("deck"), text("model"));
    let mut errors = discovery.errors.clone();
    if !discovery.decks.iter().any(|known| known == deck) {
        errors.push(if deck.is_empty() {
            "Choose an available deck.".to_owned()
        } else {
            format!("Anki has no deck named “{deck}”. Choose an available deck.")
        });
    }
    if !discovery.models.iter().any(|known| known == model) {
        errors.push(if model.is_empty() {
            "Choose an available note type.".to_owned()
        } else {
            format!("Anki has no note type named “{model}”. Choose an available note type.")
        });
    }
    if model != discovery.model {
        errors.push(format!("Refresh fields for the selected note type, “{model}”."));
        return errors;
    }
    let owned;
    let resolved = if let Some(resolved) = resolved {
        resolved
    } else {
        owned = templates::resolve(config, &discovery.fields);
        &owned
    };
    errors.extend(resolved.errors.iter().cloned());
    if let Some(first) = discovery.fields.first() {
        let value = resolved.value(first);
        if crate::host::js::trim(value).is_empty() {
            errors.push(format!("Map the first field, “{first}”, of note type “{model}” before adding notes. Anki requires it."));
        }
        let markers = templates::marker_names(value);
        if markers.iter().any(|marker| matches!(marker.as_str(), "screenshot" | "sentence-audio" | "gif")) {
            errors.push(format!("Captured media cannot be mapped to the first field, “{first}”."));
        }
    }
    if !model.is_empty() && discovery.fields.is_empty() && errors.is_empty() {
        errors.push("The selected note type has no fields.".to_owned());
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_errors_are_explained_and_others_named() {
        assert_eq!(
            describe("deck was not found: Mining"),
            "Anki has no deck named “Mining”. Choose an available deck in Anki Settings. (AnkiConnect: deck was not found: Mining)"
        );
        assert_eq!(
            describe("valid api key must be provided"),
            "AnkiConnect requires a valid API key. Enter the key from its add-on configuration."
        );
        assert_eq!(describe("cannot create note for unknown reason"), cloze_refusal("Anki refused the note"));
        assert_eq!(describe("something else"), "AnkiConnect: something else");
        assert!(describe("Unsupported action: findModelsByName").ends_with("(AnkiConnect: Unsupported action: findModelsByName)"));
    }

    /// #698: an open editor's refusal is explained, keeps AnkiConnect's text, and
    /// is told apart from a write that may have happened.
    #[test]
    fn an_open_editor_refusal_is_a_definite_failure() {
        let raw = "Close the affected editor before this API write, even if its content is saved.";
        let described = describe(raw);
        assert!(described.starts_with("Anki did not change the note because its Browser or an Edit window has it open."), "{described}");
        assert!(described.ends_with(&format!("(AnkiConnect: {raw})")));
        assert!(Failure::new(described).editor_conflict());
        assert!(!Failure::new(describe("collection is not available")).editor_conflict());
    }

    #[test]
    fn envelopes_are_exact() {
        assert!(is_envelope(&json!({ "result": 1, "error": null })));
        assert!(!is_envelope(&json!({ "result": 1, "error": null, "extra": 0 })));
        assert!(!is_envelope(&json!({ "result": 1, "error": 3 })));
        assert!(!is_envelope(&json!([1])));
    }

    /// A loopback AnkiConnect that answers each request after `hold`, counting
    /// the most it held at once; an action named `fail` gets an API error.
    fn held_anki(hold: Duration) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read as _, Write as _};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (active, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let peak = Arc::clone(&most);
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { continue };
                let (active, most) = (Arc::clone(&active), Arc::clone(&most));
                std::thread::spawn(move || {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 4096];
                    let body = loop {
                        let Ok(read) = socket.read(&mut buffer) else { return };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..read]);
                        let text = String::from_utf8_lossy(&request).into_owned();
                        if let Some((head, body)) = text.split_once("\r\n\r\n") {
                            let length = head
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|n| n.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if body.len() >= length {
                                break body.to_owned();
                            }
                        }
                    };
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(hold);
                    active.fetch_sub(1, Ordering::SeqCst);
                    let reply = if body.contains("\"action\":\"fail\"") {
                        r#"{"result":null,"error":"failed"}"#
                    } else {
                        r#"{"result":6,"error":null}"#
                    };
                    let _ = write!(
                        socket,
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                });
            }
        });
        (url, peak)
    }

    /// Upstream's anki.test.mjs "Anki requests use bounded endpoint lanes and
    /// start each timeout only when transport dispatches", "Anki queues are
    /// isolated by normalized endpoint" and "Anki API failures do not poison
    /// unrelated queued requests".
    #[test]
    fn requests_use_bounded_lanes_per_endpoint() {
        use std::sync::atomic::Ordering;
        let hold = Duration::from_millis(600);
        let (busy, peak) = held_anki(hold);
        let (quiet, _) = held_anki(Duration::ZERO);
        let gateway = Arc::new(Gateway::default());
        let started = std::time::Instant::now();
        let calls: Vec<_> = (0..2 * LANES)
            .map(|n| {
                let (gateway, busy) = (Arc::clone(&gateway), busy.clone());
                let action = if n == 1 { "fail" } else { "version" };
                // Each waits about one hold for a lane: a timeout started at enqueue would expire.
                std::thread::spawn(move || gateway.invoke(action, &Value::Null, "", 900, Some(&json!(busy))))
            })
            .collect();
        std::thread::sleep(Duration::from_millis(100));
        let elsewhere = std::time::Instant::now();
        assert_eq!(gateway.invoke("version", &Value::Null, "", 900, Some(&json!(quiet))).ok(), Some(json!(6)));
        assert!(elsewhere.elapsed() < hold / 2, "another endpoint's queue does not wait: {:?}", elsewhere.elapsed());
        let outcomes: Vec<_> = calls.into_iter().map(|call| call.join().unwrap()).collect();
        assert!(started.elapsed() >= hold * 2, "the second half waited for lanes");
        assert_eq!(peak.load(Ordering::SeqCst), LANES);
        for (n, outcome) in outcomes.iter().enumerate() {
            match n {
                1 => assert_eq!(outcome.as_ref().unwrap_err().message, "AnkiConnect: failed"),
                _ => assert_eq!(outcome.as_ref().ok(), Some(&json!(6)), "request {n}: {:?}", outcome.as_ref().err().map(|e| &e.message)),
            }
        }
    }

    #[test]
    fn an_unreachable_endpoint_is_a_dispatched_transport_failure() {
        let gateway = Gateway::default();
        let error = gateway.invoke("version", &json!({}), "", 2_000, Some(&json!("http://127.0.0.1:9"))).unwrap_err();
        assert_eq!(error.transport, Some(true));
        // A closed port is refused, unless a firewall drops the connection until the timeout.
        assert!(error.message.starts_with("Open Anki") || error.message.starts_with("AnkiConnect timed out"), "{}", error.message);
        let error = gateway.invoke("version", &json!({}), "", 2_000, Some(&json!("ftp://x"))).unwrap_err();
        assert_eq!(error.transport, None);
    }
}
