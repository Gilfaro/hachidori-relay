// SPDX-License-Identifier: GPL-3.0-or-later
//! What the relay's sharing host sends each linked browser once its records
//! have language profiles (upstream docs/languages.md 17.3-17.6, decision
//! 0009): a browser that opted in gets the records as stored; every other one,
//! older ones included, gets their Japanese view in its hello, change frames and
//! replies, and its requests are read as requests that name no profile.
//!
//! Ports `sharing-languages.js`, `japanese-view.js` and background-sharing.js'
//! `HOST_LANGUAGE_HOOKS` at Hachidori 0.5.0. A host whose records have no
//! profiles keeps today's frames, replies and broadcast.

use std::collections::{BTreeSet, HashMap};

use serde_json::{Map, Value, json};

use super::engine::languages;
use super::profile::{JAPANESE, language_code};
use super::registry;
use super::settings::{custom, options, profiles, worker};
use super::store;

/// `LANGUAGE_PROFILES_CAPABILITY`.
pub(crate) const CAPABILITY: &str = "language-profiles-v1";
/// `CHANGING_LANGUAGES`.
pub(crate) const CHANGING_LANGUAGES: &str = "The linked Hachidori is changing its languages. Try again.";
/// `PROFILES_NOT_SHARED`.
pub(crate) const PROFILES_NOT_SHARED: &str = "Update Hachidori in this browser to change the linked Hachidori's languages.";
/// `API_CLIENT_ORIGIN`: the relay's API client keeps no mirror.
const API_CLIENT_ORIGIN: &str = "relay://yomitan-api";

/// `PROFILE_REQUEST_TYPES`.
const PROFILE_REQUEST_TYPES: [&str; 2] = ["hd_profile_add", "hd_profile_remove"];
/// `RECORD_REPLY_TYPES`: requests whose replies carry options, dictionary state or
/// a personal document (17.3 item 4).
const RECORD_REPLY_TYPES: [&str; 9] = [
    "hd_options_write",
    "hd_state_read",
    "hd_state_cas",
    "hd_custom_read",
    "hd_custom_cas",
    "hd_custom_save",
    "hd_custom_append",
    "hd_apply_state",
    "hd_remove",
];

/// `HELD_SLOTS`: the shared records whose languages are followed.
const HELD_KEYS: [&str; 5] = [
    worker::OPTIONS_KEY,
    store::DICTIONARY_STATE,
    custom::CUSTOM_DICTIONARY_SOURCE_KEY,
    worker::LOOKUP_STATS_KEY,
    worker::WORD_STATUS_OVERRIDES_KEY,
];

/// A connection's opt-in (`connection.optIn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OptIn {
    Accepted,
    Refused,
}

/// What the hooks keep on one connection.
#[derive(Debug, Default)]
pub(crate) struct Connection {
    pub(crate) origin: String,
    pub(crate) opt_in: Option<OptIn>,
    /// The pack codes its opt-in named, Japanese included.
    pub(crate) packs: BTreeSet<String>,
    /// It gets no change frames and is closed once its running requests are answered.
    pub(crate) closing: bool,
    pub(crate) closed: bool,
    pub(crate) running: usize,
}

impl Connection {
    /// `optedIn`.
    fn opted_in(&self) -> bool {
        self.opt_in == Some(OptIn::Accepted)
    }

    /// `reconnects`.
    fn reconnects(&self) -> bool {
        self.origin != API_CLIENT_ORIGIN
    }
}

/// `packCodes`: this build's packs; Japanese is built in.
pub(crate) fn pack_codes() -> Vec<String> {
    languages::host_languages().into_iter().map(str::to_owned).collect()
}

/// `hasLanguagePack`.
fn has_language_pack(code: &str) -> bool {
    pack_codes().iter().any(|pack| pack == code)
}

/// `optInLanguages`: the pack codes an opt-in names, or `None` when it names
/// none that can be read.
pub(crate) fn opt_in_languages(message: &Value) -> Option<BTreeSet<String>> {
    let codes = message.get("languages")?.as_array()?;
    let mut packs: BTreeSet<String> =
        codes.iter().map(|code| code.as_str().filter(|code| language_code(code)).map(str::to_owned)).collect::<Option<_>>()?;
    packs.insert(JAPANESE.to_owned());
    Some(packs)
}

/// `recordLanguages`: the languages one stored record holds, Japanese left out.
fn record_languages(key: &str, value: Option<&Value>) -> BTreeSet<String> {
    let Some(value) = value.filter(|value| !value.is_null()) else { return BTreeSet::new() };
    let mut held: BTreeSet<String> = if key == worker::OPTIONS_KEY {
        profiles::profiles(Some(value)).iter().filter_map(|profile| profile["language"].as_str().map(str::to_owned)).collect()
    } else {
        let mut records = profiles::Records { options: None, state: None, document: None, lookup_stats: None, word_status_overrides: None };
        match key {
            store::DICTIONARY_STATE => records.state = Some(value),
            custom::CUSTOM_DICTIONARY_SOURCE_KEY => records.document = Some(value),
            worker::LOOKUP_STATS_KEY => records.lookup_stats = Some(value),
            worker::WORD_STATUS_OVERRIDES_KEY => records.word_status_overrides = Some(value),
            _ => return BTreeSet::new(),
        }
        profiles::held_languages(records)
    };
    held.remove(JAPANESE);
    held
}

/// `snapshotLanguages`: every language the shared records hold.
pub(crate) fn snapshot_languages(snapshot: &Map<String, Value>) -> BTreeSet<String> {
    HELD_KEYS.iter().flat_map(|key| record_languages(key, snapshot.get(*key))).collect()
}

/// `missingLanguages`.
pub(crate) fn missing_languages(held: &BTreeSet<String>, packs: &BTreeSet<String>) -> Vec<String> {
    held.iter().filter(|code| code.as_str() != JAPANESE && !packs.contains(*code)).cloned().collect()
}

/// `withoutLanguages`: a record's own part, without the other languages' entries.
fn without_languages(record: &Value) -> Value {
    match record {
        Value::Object(fields) if fields.contains_key("languages") => {
            let mut japanese = fields.clone();
            japanese.remove("languages");
            Value::Object(japanese)
        }
        other => other.clone(),
    }
}

/// `japaneseRecord`: the Japanese view of one stored record; null and absent
/// records stay so.
pub(crate) fn japanese_record(key: &str, value: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    match key {
        worker::OPTIONS_KEY if profiles::has_profiles(Some(value)) => {
            let mut flat = options::project_stored(Some(value));
            flat.insert("revision".to_owned(), revision_of(value));
            Value::Object(flat)
        }
        store::DICTIONARY_STATE => profiles::japanese_dictionary_view(Some(value)).unwrap_or_else(|| value.clone()),
        custom::CUSTOM_DICTIONARY_SOURCE_KEY | worker::LOOKUP_STATS_KEY | worker::WORD_STATUS_OVERRIDES_KEY => without_languages(value),
        _ => value.clone(),
    }
}

/// `revisionOf`.
fn revision_of(record: &Value) -> Value {
    record.get("revision").and_then(Value::as_u64).map_or(json!(0), Value::from)
}

/// `japaneseView`: the Japanese view of the shared records, in the same order.
pub(crate) fn japanese_view(snapshot: &Map<String, Value>) -> Map<String, Value> {
    snapshot.iter().map(|(key, value)| (key.clone(), japanese_record(key, value))).collect()
}

/// `japaneseLookupRow`: the rows of the descriptor's top-level generation, which
/// is Japanese's.
fn japanese_lookup_row(key: &str, descriptor: Option<&Value>) -> bool {
    let Some(generation) = descriptor.and_then(|descriptor| descriptor.get("generation")).filter(|generation| !generation.is_null()) else {
        return false;
    };
    key.starts_with(&format!("{}{generation}:", store::LOOKUP_STATS_ROW_PREFIX))
}

/// `japaneseChanges`: a change frame's changes as a browser that has not opted
/// in receives them; `None` when nothing is left.
pub(crate) fn japanese_changes(changes: &Map<String, Value>, descriptor: Option<&Value>) -> Option<Map<String, Value>> {
    let mut result = Map::new();
    for (key, value) in changes {
        if key.starts_with(store::LOOKUP_STATS_ROW_PREFIX) {
            if value.is_null() || japanese_lookup_row(key, descriptor) {
                result.insert(key.clone(), value.clone());
            }
        } else {
            result.insert(key.clone(), japanese_record(key, value));
        }
    }
    (!result.is_empty()).then_some(result)
}

/// `japaneseReply`: a reply as a browser that has not opted in receives it.
pub(crate) fn japanese_reply(kind: &str, response: Value) -> Value {
    let Value::Object(mut next) = response else { return response };
    if !RECORD_REPLY_TYPES.contains(&kind) {
        return Value::Object(next);
    }
    if let Some(options) = next.get_mut("options") {
        *options = japanese_record(worker::OPTIONS_KEY, options);
    }
    if let Some(state) = next.get_mut("state") {
        *state = japanese_record(store::DICTIONARY_STATE, state);
    }
    if let Some(document) = next.get_mut("document") {
        *document = without_languages(document);
    }
    // Today's hd_state_read reply has no language codes.
    if kind == "hd_state_read" {
        next.remove("languages");
    }
    Value::Object(next)
}

/// `withoutProfile`: a request from a browser that has not opted in names no profile.
fn without_profile(message: Value) -> Value {
    match message {
        Value::Object(mut fields) => {
            fields.remove("profileId");
            Value::Object(fields)
        }
        other => other,
    }
}

/// `replyHasProfiles`.
fn reply_has_profiles(response: &Value) -> bool {
    profiles::has_profiles(response.get("options"))
        || profiles::has_dictionary_profiles(response.get("state"))
        || response.get("document").and_then(Value::as_object).is_some_and(|document| document.contains_key("languages"))
        || response.get("languages").is_some_and(Value::is_array)
}

/// `createLanguageTracker`: the languages each shared record holds and the count
/// descriptor, kept from the commits in order after a first read of the records.
#[derive(Debug, Default)]
struct Tracker {
    known: bool,
    descriptor: Option<Value>,
    held: HashMap<String, BTreeSet<String>>,
}

impl Tracker {
    fn record(&mut self, key: &str, value: Option<&Value>) {
        if key == worker::LOOKUP_STATS_KEY {
            self.descriptor = value.filter(|value| !value.is_null()).cloned();
        }
        if HELD_KEYS.contains(&key) {
            self.held.insert(key.to_owned(), record_languages(key, value));
        }
    }

    /// A read of the shared records, before any commit that follows it.
    fn read(&mut self, snapshot: &Map<String, Value>) {
        if self.known {
            return;
        }
        self.known = true;
        for (key, value) in snapshot {
            self.record(key, Some(value));
        }
    }

    /// One commit's new values: the languages the changed records hold.
    fn observe(&mut self, changes: &Map<String, Value>) -> BTreeSet<String> {
        let mut carried = BTreeSet::new();
        for (key, value) in changes {
            carried.extend(record_languages(key, Some(value)));
            if self.known {
                self.record(key, Some(value));
            }
        }
        carried
    }

    fn held(&self) -> BTreeSet<String> {
        self.held.values().flatten().cloned().collect()
    }
}

/// How one commit is told to the connections once the records have profiles
/// (`plan`): the count descriptor that [`frame`] reads.
pub(crate) struct Plan {
    pub(crate) descriptor: Option<Value>,
}

/// What a connection's request becomes (`languages.request`).
pub(crate) enum Handled {
    /// Dispatch it.
    Message(Value),
    /// Answer it with this reply.
    Response(Value),
}

/// The host's language hooks (`HOST_LANGUAGE_HOOKS`) and what they follow.
#[derive(Debug, Default)]
pub(crate) struct Hooks {
    /// Whether the records have profiles, as the last read or commit gave them.
    host_profiles: bool,
    tracker: Option<Tracker>,
}

/// The reply a refused request gets (`failureReply`).
fn refused(message: &Value, error: &str) -> Value {
    super::local::failure(message, error)
}

impl Hooks {
    fn tracker(&mut self) -> &mut Tracker {
        self.tracker.get_or_insert_with(Tracker::default)
    }

    /// `hello`: the Japanese view and the profile capability once the records have
    /// profiles; `None` for today's hello.
    pub(crate) fn hello(&mut self, snapshot: &Map<String, Value>) -> Option<Map<String, Value>> {
        if !profiles::has_profiles(snapshot.get(worker::OPTIONS_KEY)) {
            return None;
        }
        self.host_profiles = true;
        self.tracker().read(snapshot);
        Some(japanese_view(snapshot))
    }

    /// `answerLanguageOptIn`: the snapshot read, the client's packs checked against
    /// every language it holds, and the outcome recorded for this connection. The
    /// caller holds the store from the read until the reply is sent, which stands
    /// for upstream's held change frames: no commit is told to the connection
    /// between the snapshot and its reply.
    pub(crate) fn opt_in(&mut self, message: &Value, connection: &mut Connection, snapshot: Map<String, Value>) -> Value {
        let Some(packs) = opt_in_languages(message) else {
            return refused(message, "The language opt-in named no language packs.");
        };
        if connection.closing {
            return refused(message, CHANGING_LANGUAGES);
        }
        self.host_profiles = profiles::has_profiles(snapshot.get(worker::OPTIONS_KEY));
        self.tracker().read(&snapshot);
        let missing = missing_languages(&snapshot_languages(&snapshot), &packs);
        connection.packs = packs;
        connection.opt_in = Some(if missing.is_empty() { OptIn::Accepted } else { OptIn::Refused });
        let (outcome, snapshot) = if missing.is_empty() { ("accepted", snapshot) } else { ("refused", japanese_view(&snapshot)) };
        let fields = json!({ "optIn": outcome, "snapshot": snapshot, "missingLanguages": missing, "hostLanguages": pack_codes() });
        worker::reply(message, fields.as_object().cloned().unwrap_or_default())
    }

    /// `request`, for every request but the opt-in: a connection that has not opted
    /// in names no profile; one that opted in may add a language this host has.
    pub(crate) fn request(&self, message: Value, connection: &Connection) -> Handled {
        if !self.host_profiles && !connection.closing && connection.opt_in.is_none() {
            return Handled::Message(message);
        }
        let kind = message.get("type").and_then(Value::as_str).unwrap_or_default();
        let profile_request =
            message.get("target").and_then(Value::as_str) == Some("hoshidicts-offscreen") && PROFILE_REQUEST_TYPES.contains(&kind);
        if connection.closing && (RECORD_REPLY_TYPES.contains(&kind) || profile_request) {
            return Handled::Response(refused(&message, CHANGING_LANGUAGES));
        }
        if !connection.opted_in() {
            if profile_request {
                return Handled::Response(refused(&message, PROFILES_NOT_SHARED));
            }
            return Handled::Message(without_profile(message));
        }
        if profile_request && kind == "hd_profile_add" {
            let language = message.get("language");
            if !language.and_then(Value::as_str).is_some_and(has_language_pack) {
                let error = format!("The linked Hachidori does not have {}. Update it and try again.", registry::name_of(language));
                return Handled::Response(refused(&message, &error));
            }
        }
        Handled::Message(message)
    }

    /// `reply`: a connection that would receive a language it lacks gets the
    /// Japanese view from that commit and reconnects (17.3 item 6).
    pub(crate) fn reply(response: Value, kind: &str, connection: &mut Connection) -> Value {
        if !reply_has_profiles(&response) {
            return response;
        }
        if connection.opted_in() {
            let mut carried = record_languages(worker::OPTIONS_KEY, response.get("options"));
            carried.extend(record_languages(store::DICTIONARY_STATE, response.get("state")));
            carried.extend(record_languages(custom::CUSTOM_DICTIONARY_SOURCE_KEY, response.get("document")));
            if missing_languages(&carried, &connection.packs).is_empty() {
                return response;
            }
            connection.opt_in = Some(OptIn::Refused);
            if connection.reconnects() {
                connection.closing = true;
            }
        }
        japanese_reply(kind, response)
    }

    /// `storageChanged`: follows every commit, connected or not. `None` for
    /// today's broadcast; otherwise the plan for each connection's own frame,
    /// after marking the connections that must reconnect.
    pub(crate) fn storage_changed<'a>(
        &mut self,
        changes: &Map<String, Value>,
        old_options: Option<&Value>,
        connections: impl Iterator<Item = &'a mut Connection>,
    ) -> Option<Plan> {
        let options = changes.get(worker::OPTIONS_KEY);
        let left_single_profile = options.is_some() && !profiles::has_profiles(old_options) && profiles::has_profiles(options);
        if let Some(options) = options {
            self.host_profiles = profiles::has_profiles(Some(options));
        }
        let mut connections: Vec<&mut Connection> = connections.collect();
        // With nobody connected nothing is followed; the next hello reads afresh.
        if connections.is_empty() {
            self.tracker = None;
            return None;
        }
        let followed = connections.iter().any(|connection| connection.closing || connection.opt_in.is_some());
        if !self.host_profiles && !left_single_profile && !followed {
            return None;
        }
        // Records that gain profiles are read afresh by the next hello.
        if left_single_profile {
            self.tracker = None;
        }
        let tracker = self.tracker();
        let carried = tracker.observe(changes);
        let held = tracker.known.then(|| tracker.held());
        for connection in &mut connections {
            if connection.closing || !connection.reconnects() {
                continue;
            }
            if closes_after_change(connection, left_single_profile, &carried, held.as_ref()) {
                connection.closing = true;
            }
        }
        Some(Plan { descriptor: self.tracker().descriptor.clone() })
    }
}

/// The changes one connection receives (`plan.frame`).
pub(crate) fn frame(connection: &Connection, values: &Map<String, Value>, descriptor: Option<&Value>) -> Option<Map<String, Value>> {
    if connection.opted_in() { Some(values.clone()) } else { japanese_changes(values, descriptor) }
}

/// `closesAfterChange`: records that gain profiles close every connection; an
/// opt-in that now lacks one of the host's packs is refused; a refused one whose
/// packs now cover the held languages reconnects to opt in again.
fn closes_after_change(
    connection: &mut Connection,
    left_single_profile: bool,
    carried: &BTreeSet<String>,
    held: Option<&BTreeSet<String>>,
) -> bool {
    if left_single_profile {
        return true;
    }
    if connection.opted_in() && !missing_languages(carried, &connection.packs).is_empty() {
        connection.opt_in = Some(OptIn::Refused);
        return true;
    }
    connection.opt_in == Some(OptIn::Refused) && held.is_some_and(|held| missing_languages(held, &connection.packs).is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(fields) => fields,
            _ => Map::new(),
        }
    }

    fn row(id: &str, language: &str) -> Value {
        json!({ "id": id, "title": format!("{language} words"), "path": format!("/dicts/{id}"), "language": language })
    }

    /// upstream's `profileRecords`: a profile per language, each with its own dictionary.
    fn profile_records(languages: &[&str]) -> Map<String, Value> {
        let ids = |code: &str| match code {
            "ja" => "1",
            "en" => "2",
            _ => "3",
        };
        let profiles: Vec<Value> = languages
            .iter()
            .map(|code| {
                let scan = if *code == "ja" { 12 } else { 20 };
                json!({ "id": code, "language": code, "settings": { "scanLength": scan }, "packSettings": {} })
            })
            .collect();
        let state_profiles: Map<String, Value> = languages
            .iter()
            .map(|code| ((*code).to_owned(), json!({ "dictionaries": [{ "id": ids(code), "enabled": true }], "groups": [] })))
            .collect();
        object(json!({
            "options": { "revision": 10, "hoverEnabled": true, "profiles": profiles },
            "dictionaryState": { "schemaVersion": 2, "revision": 30, "dictionaries": [row("1", "ja"), row("2", "en"), row("3", "ar")],
                "profiles": state_profiles },
            "lookupStats": { "generation": "ja-generation", "revision": 3 },
        }))
    }

    #[test]
    fn the_japanese_view_of_change_frames_and_replies() {
        let records = profile_records(&["ja", "en"]);
        let changes = object(json!({ "options": records["options"], "lookupStats:\"en\":[\"a\",\"\"]": { "term": "a" },
            "lookupStats:\"en\":[\"b\",\"\"]": null }));
        let descriptor = json!({ "generation": "ja", "revision": 1 });
        let japanese = japanese_changes(&changes, Some(&descriptor)).unwrap();
        assert_eq!(japanese.keys().collect::<Vec<_>>(), ["lookupStats:\"en\":[\"b\",\"\"]", "options"]);
        assert!(!japanese["options"].as_object().unwrap().contains_key("profiles"));
        assert_eq!(japanese["options"]["scanLength"], 12);
        assert_eq!(japanese["options"]["revision"], 10);
        let lone = object(json!({ "lookupStats:\"en\":[\"a\",\"\"]": { "term": "a" } }));
        assert_eq!(japanese_changes(&lone, Some(&descriptor)), None);
        let reply = japanese_reply("hd_state_read", json!({ "ok": true, "state": records["dictionaryState"], "languages": ["ja", "en"] }));
        assert_eq!(reply["state"]["schemaVersion"], 1);
        assert!(reply.get("languages").is_none());
        assert_eq!(japanese_reply("hd_lookup", json!({ "ok": true, "results": [] })), json!({ "ok": true, "results": [] }));
        assert_eq!(record_languages(worker::OPTIONS_KEY, Some(&records["options"])), BTreeSet::from(["en".to_owned()]));
        let mut held = records;
        held.insert(
            worker::WORD_STATUS_OVERRIDES_KEY.to_owned(),
            json!({ "revision": 1, "known": [], "ignored": [], "languages": { "ar": { "known": ["x"], "ignored": [] } } }),
        );
        assert_eq!(snapshot_languages(&held), BTreeSet::from(["ar".to_owned(), "en".to_owned()]));
        let document = json!({ "schemaVersion": 1, "revision": 1, "text": "", "semanticRevision": "0", "languages": { "en": {} } });
        assert_eq!(
            japanese_record(custom::CUSTOM_DICTIONARY_SOURCE_KEY, &document),
            json!({ "schemaVersion": 1, "revision": 1, "text": "", "semanticRevision": "0" })
        );
    }

    #[test]
    fn an_opt_in_is_accepted_unless_the_records_hold_a_language_it_lacks() {
        let mut hooks = Hooks::default();
        let plain =
            object(json!({ "options": { "revision": 1 }, "wordStatusOverrides": { "revision": 1, "known": ["猫"], "ignored": [] } }));
        let mut connection = Connection::default();
        let accepted = hooks.opt_in(&json!({ "type": "hd_language_profiles_opt_in", "languages": [] }), &mut connection, plain.clone());
        assert_eq!((&accepted["optIn"], &accepted["snapshot"]), (&json!("accepted"), &Value::Object(plain.clone())));
        assert_eq!(accepted["hostLanguages"], json!(pack_codes()));
        assert_eq!(connection.opt_in, Some(OptIn::Accepted));
        for languages in [json!(null), json!(["EN"]), json!(["english"]), json!([7]), json!("en")] {
            let mut connection = Connection::default();
            let refused = hooks.opt_in(&json!({ "languages": languages }), &mut connection, plain.clone());
            assert_eq!(refused["error"], "The language opt-in named no language packs.");
            assert_eq!(connection.opt_in, None);
        }
        let held = object(json!({
            "customDictionarySource": { "revision": 1, "text": "", "languages": { "fr": { "text": "chat,,cat" }, "de": { "text": "" } } },
            "lookupStats": { "generation": "g", "revision": 2, "languages": { "en": "g-en" } },
            "wordStatusOverrides": { "revision": 3, "known": [], "ignored": [],
                "languages": { "ar": { "known": [], "ignored": ["x"] }, "es": { "known": [] } } },
        }));
        let mut connection = Connection::default();
        let refused = hooks.opt_in(&json!({ "languages": ["en"] }), &mut connection, held.clone());
        assert_eq!(refused["optIn"], "refused");
        assert_eq!(refused["missingLanguages"], json!(["ar", "fr"]));
        assert_eq!(refused["snapshot"]["lookupStats"], json!({ "generation": "g", "revision": 2 }));
        assert_eq!(connection.opt_in, Some(OptIn::Refused));
        let mut closing = Connection { closing: true, ..Connection::default() };
        assert_eq!(hooks.opt_in(&json!({ "languages": [] }), &mut closing, held)["error"], CHANGING_LANGUAGES);
    }

    #[test]
    fn requests_of_a_browser_that_has_not_opted_in_name_no_profile() {
        let mut hooks = Hooks::default();
        let older = Connection::default();
        let named = json!({ "target": "hoshidicts-worker", "type": "hd_options_write", "profileId": "en" });
        // A single-profile host leaves every request as it is.
        assert!(matches!(hooks.request(named.clone(), &older), Handled::Message(message) if message == named));
        assert!(hooks.hello(&profile_records(&["ja", "en"])).is_some());
        let Handled::Message(stripped) = hooks.request(named.clone(), &older) else { panic!("not dispatched") };
        assert!(stripped.get("profileId").is_none());
        let add = json!({ "target": "hoshidicts-offscreen", "type": "hd_profile_add", "language": "fr" });
        let Handled::Response(refused) = hooks.request(add, &older) else { panic!("not refused") };
        assert_eq!(refused["error"], PROFILES_NOT_SHARED);
        let aware = Connection { opt_in: Some(OptIn::Accepted), ..Connection::default() };
        let missing = json!({ "target": "hoshidicts-offscreen", "type": "hd_profile_add", "language": "zz" });
        let Handled::Response(refused) = hooks.request(missing, &aware) else { panic!("not refused") };
        let expected = format!("The linked Hachidori does not have {}. Update it and try again.", registry::name("zz"));
        assert_eq!(refused["error"], expected.as_str());
        assert!(matches!(hooks.request(named.clone(), &aware), Handled::Message(message) if message == named));
        let closing = Connection { closing: true, opt_in: Some(OptIn::Accepted), ..Connection::default() };
        let Handled::Response(refused) = hooks.request(json!({ "type": "hd_state_read" }), &closing) else { panic!("not refused") };
        assert_eq!(refused["error"], CHANGING_LANGUAGES);
        assert!(matches!(hooks.request(json!({ "type": "hd_lookup" }), &closing), Handled::Message(_)));
    }

    #[test]
    fn a_reply_carrying_a_language_the_browser_lacks_closes_it_with_the_japanese_view() {
        let records = profile_records(&["ja", "en", "ar"]);
        let reply = json!({ "ok": false, "conflict": true, "options": records["options"] });
        let packs = |codes: &[&str]| codes.iter().map(|code| (*code).to_owned()).collect::<BTreeSet<_>>();
        let mut aware = Connection { opt_in: Some(OptIn::Accepted), packs: packs(&["ja", "en"]), ..Connection::default() };
        let seen = Hooks::reply(reply.clone(), "hd_options_write", &mut aware);
        assert!(seen["options"].get("profiles").is_none());
        assert_eq!((aware.opt_in, aware.closing), (Some(OptIn::Refused), true));
        let mut every = Connection { opt_in: Some(OptIn::Accepted), packs: packs(&["ja", "en", "ar"]), ..Connection::default() };
        assert_eq!(Hooks::reply(reply.clone(), "hd_options_write", &mut every), reply);
        let mut api = Connection { origin: API_CLIENT_ORIGIN.to_owned(), opt_in: Some(OptIn::Accepted), ..Connection::default() };
        Hooks::reply(reply, "hd_options_write", &mut api);
        assert!(!api.closing, "the API client keeps no mirror");
    }

    #[test]
    fn records_that_gain_profiles_close_every_connection() {
        let mut hooks = Hooks::default();
        let single = json!({ "revision": 4, "scanLength": 12 });
        let gained = profile_records(&["ja", "en"]);
        let mut connections = [Connection::default(), Connection { origin: API_CLIENT_ORIGIN.to_owned(), ..Connection::default() }];
        let changes = object(json!({ "options": gained["options"] }));
        assert!(hooks.storage_changed(&changes, Some(&single), connections.iter_mut()).is_some());
        assert_eq!(connections.each_ref().map(|connection| connection.closing), [true, false]);
        // A single-profile commit with nobody following keeps today's broadcast.
        let mut hooks = Hooks::default();
        let mut connections = [Connection::default()];
        let changes = object(json!({ "options": { "revision": 5 } }));
        assert!(hooks.storage_changed(&changes, Some(&single), connections.iter_mut()).is_none());
    }

    #[test]
    fn a_refused_browser_reconnects_once_the_held_languages_are_its_own() {
        let mut hooks = Hooks::default();
        let mut records = profile_records(&["ja", "en"]);
        records.insert(
            "lookupStats".to_owned(),
            json!({ "generation": "ja-generation", "revision": 3, "languages": { "ar": "ar-generation" } }),
        );
        let mut connections = [Connection::default()];
        hooks.opt_in(&json!({ "languages": ["en"] }), &mut connections[0], records.clone());
        assert_eq!(connections[0].opt_in, Some(OptIn::Refused));
        let unchanged = object(json!({ "options": records["options"] }));
        let plan = hooks.storage_changed(&unchanged, Some(&records["options"]), connections.iter_mut());
        assert!(!connections[0].closing, "Arabic's counts are still held");
        let frame = frame(&connections[0], &unchanged, plan.and_then(|plan| plan.descriptor).as_ref()).unwrap();
        assert!(frame["options"].get("profiles").is_none());
        let erased = object(json!({ "lookupStats": { "generation": "ja-generation", "revision": 4 } }));
        hooks.storage_changed(&erased, None, connections.iter_mut());
        assert!(connections[0].closing);
    }
}
