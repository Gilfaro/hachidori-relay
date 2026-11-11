// SPDX-License-Identifier: GPL-3.0-or-later
//! The service worker's storage requests a linked browser forwards to its host
//! (`WORKER_HANDLERS` in the extension's `background-requests.js`, with the commit
//! rules of `background-core.js`): dictionary state and custom-source CAS, options
//! writes, lookup counts and word-status overrides.
//!
//! The caller runs one request at a time (`serialiseStorage`).

use serde_json::{Map, Value, json};

use super::profile_worker::{self, Coded};
use super::{custom, options, profiles};
use crate::host::js::{self, strict_eq};
use crate::host::profile;
use crate::host::store;

/// The worker requests a linked browser forwards (`FORWARDED_REQUESTS`).
pub(crate) const REQUESTS: [&str; 9] = [
    "hd_state_read",
    "hd_state_cas",
    "hd_custom_read",
    "hd_custom_cas",
    "hd_options_write",
    "hd_lookup_stats_read",
    "hd_lookup_stats_record",
    "hd_word_status_override",
    "hd_known_words_add",
];
/// The capability of a linked client that edits Templates and Custom Buttons itself.
pub(crate) const LINKED_ANKI_CAPABILITY: &str = "linked-anki-v2";
/// The capability of a linked client that mines with the first Template and edits
/// its legacy Anki fields and Custom Links.
pub(crate) const LEGACY_LINKED_ANKI_CAPABILITY: &str = "linked-anki-v1";

pub(crate) const OPTIONS_KEY: &str = "options";
const LEGACY_DICTIONARIES_KEY: &str = "dictionaries";
pub(crate) const LOOKUP_STATS_KEY: &str = "lookupStats";
/// `JAPANESE_PROFILE_ID`: the language of a request that names no profile.
const JAPANESE: &str = profile::JAPANESE;
pub(crate) const WORD_STATUS_OVERRIDES_KEY: &str = "wordStatusOverrides";
const OVERRIDE_STATUSES: [&str; 2] = ["known", "ignored"];
const UPDATE_SCHEDULES: [&str; 5] = ["off", "hourly", "daily", "weekly", "monthly"];
const SELECTION_KEYS: [&str; 4] =
    ["frequencyDictionary", "definitionBlurFrequencyDictionary", "compactDefinitionSummaryDictionary", "pitchAccentFuriganaDictionary"];
const MAX_OPTIONS_FRAME_BYTES: usize = 1024 * 1024;
const OPTIONS_LIMIT: &str = "reader options message exceeds the 1 MiB serialized limit";
const LINKED_SETTINGS_UPDATE_REQUIRED: &str = "Update the linked Hachidori before editing Templates or Custom Buttons.";

/// Where the requests read and write.
pub(crate) trait Storage {
    fn read(&self, key: &str) -> Option<Value>;
    /// Writes `values` at once (`null` removes a key).
    fn write(&self, values: Map<String, Value>) -> std::result::Result<(), String>;
    /// Every stored key with `prefix`; a store that cannot list its keys lists none.
    fn keys_with(&self, _prefix: &str) -> Vec<String> {
        Vec::new()
    }
}

type Result<T> = std::result::Result<T, String>;

/// Who sent a worker request: this host's own engine, or a linked browser.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sender {
    Engine,
    Linked,
}

/// Answers one request of a linked browser with its reply (`workerReply` or `failureReply`).
pub(crate) fn handle(storage: &impl Storage, message: &Value, capabilities: &[String]) -> Value {
    handle_from(storage, message, capabilities, Sender::Linked)
}

/// Answers one worker request from `sender`.
pub(crate) fn handle_from(storage: &impl Storage, message: &Value, capabilities: &[String], sender: Sender) -> Value {
    let kind = message.get("type").and_then(Value::as_str).unwrap_or_default();
    let result = match kind {
        "hd_state_read" => Ok(state_read(storage)),
        "hd_state_cas" => state_cas(storage, message, sender),
        "hd_custom_read" => custom_read(storage, message),
        "hd_custom_cas" => custom_cas(storage, message, sender),
        "hd_options_write" => return options_write(storage, message, capabilities),
        "hd_lookup_stats_read" => lookup_stats(storage, message, false),
        "hd_lookup_stats_record" => lookup_stats(storage, message, true),
        "hd_word_status_override" => word_status_override(storage, message),
        "hd_known_words_add" => known_words_add(storage, message),
        // The engine's one compare-and-set of hd_profile_add and hd_profile_remove.
        "hd_profile_cas" if sender == Sender::Engine => profile_worker::profile_cas(storage, message),
        "hd_profile_cas" => Err("Languages are added and removed only by the dictionary engine.".into()),
        other => Err(format!("unknown worker request type {}", Value::from(other)).into()),
    };
    match result {
        Ok(result) => reply(message, result),
        Err(Coded { error, code }) => crate::host::local::failure_with_code(message, &error, code),
    }
}

/// `workerReply`.
pub(crate) fn reply(message: &Value, mut result: Map<String, Value>) -> Value {
    let ok = result.remove("ok").unwrap_or(Value::Bool(true));
    let error = result.remove("error").unwrap_or(Value::Null);
    let mut reply = Map::new();
    reply.insert("type".to_owned(), json!(format!("{}_result", message["type"].as_str().unwrap_or_default())));
    reply.insert("requestId".to_owned(), message.get("requestId").cloned().unwrap_or(Value::Null));
    reply.insert("ok".to_owned(), ok);
    reply.insert("error".to_owned(), error);
    reply.extend(result);
    Value::Object(reply)
}

pub(crate) fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(fields) => fields,
        _ => Map::new(),
    }
}

/// `{ ...value }` for what storage holds: an object's fields, nothing otherwise.
pub(crate) fn spread(value: Option<&Value>) -> Map<String, Value> {
    value.and_then(Value::as_object).cloned().unwrap_or_default()
}

/// `value ?? []` where the list is then used as an array.
fn listed<'a>(value: Option<&'a Value>, call: &str) -> Result<&'a [Value]> {
    match value {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(format!("{call} is not a function")),
    }
}

/// `readDictionaryStorage`: the dictionary state, `null` when there is none.
pub(crate) fn stored_state(storage: &impl Storage) -> Value {
    storage.read(store::DICTIONARY_STATE).unwrap_or(Value::Null)
}

/// `assertDictionaryState`: schema 1, or a schema-2 state with profiles.
pub(crate) fn assert_state(state: &Value) -> Result<()> {
    if !state.is_null() && !strict_eq(state.get("schemaVersion"), Some(&json!(1))) && !profiles::has_dictionary_profiles(Some(state)) {
        return Err(format!("unsupported dictionary state schema {}", js::to_string(state.get("schemaVersion"))));
    }
    Ok(())
}

/// `current?.revision ?? 0`.
pub(crate) fn state_revision(state: &Value) -> Value {
    state.get("revision").filter(|revision| !revision.is_null()).cloned().unwrap_or(json!(0))
}

/// `optionsRevision`.
pub(crate) fn options_revision(options: Option<&Value>) -> f64 {
    js::whole(options.and_then(|options| options.get("revision"))).unwrap_or(0.0)
}

fn state_read(storage: &impl Storage) -> Map<String, Value> {
    let state = stored_state(storage);
    let legacy = if state.is_null() { storage.read(LEGACY_DICTIONARIES_KEY).filter(Value::is_array) } else { None };
    let mut read = object(owned_json!({ "state": state, "legacyDictionaries": legacy }));
    let stored_options = storage.read(OPTIONS_KEY);
    if profiles::has_profiles(stored_options.as_ref()) {
        read.insert("languages".to_owned(), Value::from(profile_worker::distinct_languages(stored_options.as_ref())));
    }
    read
}

/// A `Set` key for a JSON value under `SameValueZero`; arrays and objects match nothing.
fn set_key(value: Option<&Value>) -> Option<String> {
    Some(match value {
        None => "u".to_owned(),
        Some(Value::Null) => "n".to_owned(),
        Some(Value::Bool(flag)) => format!("b{flag}"),
        Some(Value::Number(number)) => format!("f{}", number.as_f64().unwrap_or(f64::NAN) + 0.0),
        Some(Value::String(text)) => format!("s{text}"),
        Some(Value::Array(_) | Value::Object(_)) => return None,
    })
}

/// `pruneGroupMemberships`.
pub(crate) fn prune_groups(value: Option<&Value>, dictionaries: &[Value]) -> Result<Value> {
    let Some(groups) = value.and_then(Value::as_array) else { return Ok(json!([])) };
    let mut installed = std::collections::HashSet::new();
    for dictionary in dictionaries {
        if let Some(key) = set_key(js::property(Some(dictionary), "id")?) {
            installed.insert(key);
        }
    }
    let mut pruned = Vec::with_capacity(groups.len());
    for group in groups {
        let ids = js::property(Some(group), "dictionaryIds")?;
        let mut fields = match group {
            Value::Object(fields) => fields.clone(),
            Value::Array(items) => items.iter().enumerate().map(|(index, item)| (index.to_string(), item.clone())).collect(),
            Value::String(text) => {
                text.encode_utf16().enumerate().map(|(index, unit)| (index.to_string(), json!(String::from_utf16_lossy(&[unit])))).collect()
            }
            _ => Map::new(),
        };
        let mut seen = std::collections::HashSet::new();
        let retained: Vec<Value> = ids
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|id| set_key(Some(id)).is_some_and(|key| installed.contains(&key) && seen.insert(key)))
            .cloned()
            .collect();
        fields.insert("dictionaryIds".to_owned(), Value::Array(retained));
        pruned.push(Value::Object(fields));
    }
    Ok(Value::Array(pruned))
}

/// `assertDictionaryUpdateSchedule` for each dictionary.
pub(crate) fn assert_update_schedules(dictionaries: &[Value]) -> Result<()> {
    for dictionary in dictionaries {
        if let Some(schedule) = js::property(Some(dictionary), "updateScheduleOverride")?.filter(|schedule| !schedule.is_null())
            && !schedule.as_str().is_some_and(|schedule| UPDATE_SCHEDULES.contains(&schedule))
        {
            return Err("The dictionary update schedule is invalid.".to_owned());
        }
    }
    Ok(())
}

/// `committedSelectionTitle`: a selected dictionary's title after a commit renamed or removed it.
fn committed_title(title: &Value, current: &Value, dictionaries: &[Value]) -> Result<Value> {
    if current.is_null() {
        return Ok(title.clone());
    }
    let before = current.get("dictionaries");
    let Some(before) = before.and_then(Value::as_array) else {
        return Err(match before {
            None | Some(Value::Null) => js::null_property(before, "find"),
            Some(_) => "current?.dictionaries.find is not a function".to_owned(),
        });
    };
    let mut selected = None;
    for entry in before {
        if strict_eq(js::property(Some(entry), "title")?, Some(title)) {
            selected = Some(entry);
            break;
        }
    }
    let Some(selected) = selected else { return Ok(title.clone()) };
    for entry in dictionaries {
        if strict_eq(js::property(Some(entry), "id")?, selected.get("id")) {
            return Ok(entry.get("title").filter(|title| !title.is_null()).cloned().unwrap_or(json!("")));
        }
    }
    Ok(json!(""))
}

/// `migrateCommittedDictionarySelections`.
pub(crate) fn migrate_selections(mut options: Map<String, Value>, current: &Value, dictionaries: &[Value]) -> Result<Map<String, Value>> {
    let migrate = |title: &Value| -> Result<Value> {
        if title.as_str().is_some_and(|title| !title.is_empty()) {
            committed_title(title, current, dictionaries)
        } else {
            Ok(title.clone())
        }
    };
    for key in SELECTION_KEYS {
        if let Some(title) = options.get(key) {
            let migrated = migrate(title)?;
            options.insert(key.to_owned(), migrated);
        }
    }
    match options.get("kanjiClickDictionary") {
        Some(title @ Value::String(_)) => {
            let migrated = migrate(title)?;
            options.insert("kanjiClickDictionary".to_owned(), migrated);
        }
        Some(Value::Object(selection)) if js::truthy(selection.get("title")) => {
            let title = migrate(&selection["title"])?;
            let next = if title.as_str() == Some("") {
                json!("")
            } else {
                let mut selection = selection.clone();
                selection.insert("title".to_owned(), title);
                Value::Object(selection)
            };
            options.insert("kanjiClickDictionary".to_owned(), next);
        }
        _ => {}
    }
    if let Some(Value::Object(source)) = options.get("popupImageSource")
        && source.get("kind").and_then(Value::as_str) == Some("dictionary")
    {
        let title = migrate(source.get("title").unwrap_or(&Value::Null))?;
        let next = if js::truthy(Some(&title)) { json!({ "kind": "dictionary", "title": title }) } else { Value::Null };
        options.insert("popupImageSource".to_owned(), next);
    }
    Ok(options)
}

/// `dictionaryCommit`: the next state, and the options its library implies.
fn dictionary_commit(
    current: &Value,
    stored_options: Option<&Value>,
    dictionaries: &[Value],
    groups: Option<&Value>,
) -> Result<(Value, Map<String, Value>)> {
    assert_update_schedules(dictionaries)?;
    let revision = js::to_number(Some(&state_revision(current))) + 1.0;
    let groups = prune_groups(groups.filter(|groups| !groups.is_null()).or_else(|| current.get("groups")), dictionaries)?;
    let state = json!({ "schemaVersion": 1, "revision": js::number(revision), "dictionaries": dictionaries, "groups": groups });
    let mut values = Map::new();
    values.insert(store::DICTIONARY_STATE.to_owned(), state.clone());
    if let Some(stored) = stored_options {
        let revision = options_revision(Some(stored));
        let mut projected = options::project_stored(Some(stored));
        projected.insert("revision".to_owned(), js::number(revision));
        let migrated = migrate_selections(projected, current, dictionaries)?;
        let groups = state["groups"].as_array().map_or(&[][..], Vec::as_slice);
        let next = options::normalise_dictionary_selections(&Value::Object(migrated), dictionaries, groups)?;
        let mut base = spread(Some(stored));
        base.insert("revision".to_owned(), js::number(revision));
        if !js::same_json(&next, &Value::Object(base)) {
            let mut next = object(next);
            next.insert("revision".to_owned(), js::number(revision + 1.0));
            values.insert(OPTIONS_KEY.to_owned(), Value::Object(next));
        }
    }
    Ok((state, values))
}

/// `assertOwnStateWrite`: only this host's engine writes a whole record.
fn assert_own_state_write(sender: Sender) -> Result<()> {
    if sender == Sender::Linked {
        return Err("A linked Hachidori cannot replace this Hachidori's dictionary list.".to_owned());
    }
    Ok(())
}

fn state_cas(storage: &impl Storage, message: &Value, sender: Sender) -> profile_worker::Result<Map<String, Value>> {
    if js::whole(message.get("baseRevision")).is_none() {
        return Err("the dictionary state write request carried no valid base revision".into());
    }
    // The engine's own commits write the whole schema-2 record.
    if message.get("state").is_some() {
        assert_own_state_write(sender)?;
        return profile_worker::whole_state_cas(storage, message);
    }
    let Some(dictionaries) = message.get("dictionaries").and_then(Value::as_array) else {
        return Err("the dictionary state write request carried no list".into());
    };
    let groups = message.get("groups");
    if groups.is_some_and(|groups| !groups.is_array()) {
        return Err("the dictionary state write request carried invalid groups".into());
    }
    let current = stored_state(storage);
    let legacy = current.is_null() && storage.read(LEGACY_DICTIONARIES_KEY).is_some_and(|rows| rows.is_array());
    let stored_options = storage.read(OPTIONS_KEY);
    assert_state(&current)?;
    if !strict_eq(message.get("baseRevision"), Some(&state_revision(&current))) {
        return Ok(object(json!({
            "ok": false, "conflict": true, "error": "the dictionary state changed while it was being written", "state": current,
        })));
    }
    if profiles::has_profiles(stored_options.as_ref()) {
        return match profile_worker::profile_view_commit(message, &current, stored_options.as_ref()) {
            Ok((state, values)) => {
                storage.write(values)?;
                Ok(object(json!({ "state": state })))
            }
            Err(profile_worker::ViewRefusal::Profile(refused)) => Err(refused),
            Err(profile_worker::ViewRefusal::Protected(error)) => {
                Ok(object(json!({ "ok": false, "protected": true, "error": error, "state": current })))
            }
        };
    }
    if message.get("profileId").is_some() {
        profile_worker::request_profile_id(message, stored_options.as_ref())?;
    }
    let transition = listed(current.get("dictionaries"), "currentDictionaries.findIndex")
        .and_then(|before| custom::assert_ordinary_transition(before, dictionaries));
    if let Err(error) = transition {
        return Ok(object(json!({ "ok": false, "protected": true, "error": error, "state": current })));
    }
    let (state, mut values) = dictionary_commit(&current, stored_options.as_ref(), dictionaries, groups)?;
    if legacy {
        values.insert(LEGACY_DICTIONARIES_KEY.to_owned(), Value::Null);
    }
    storage.write(values)?;
    Ok(object(owned_json!({ "state": state })))
}

fn custom_read(storage: &impl Storage, message: &Value) -> profile_worker::Result<Map<String, Value>> {
    let state = stored_state(storage);
    assert_state(&state)?;
    let language = profile_worker::custom_request_language(message, storage.read(OPTIONS_KEY).as_ref())?;
    let document = custom::document_for(storage.read(custom::CUSTOM_DICTIONARY_SOURCE_KEY).as_ref(), &language)?;
    Ok(object(owned_json!({ "document": document, "state": state })))
}

fn custom_cas(storage: &impl Storage, message: &Value, sender: Sender) -> profile_worker::Result<Map<String, Value>> {
    if js::whole(message.get("baseDocumentRevision")).is_none() {
        return Err("the custom dictionary write carried no valid document revision".into());
    }
    if js::whole(message.get("baseRevision")).is_none() {
        return Err("the custom dictionary write carried no valid dictionary revision".into());
    }
    let (Some(text), Some(semantic)) =
        (message.get("text").and_then(Value::as_str), message.get("semanticRevision").and_then(Value::as_str))
    else {
        return Err("the custom dictionary write carried no source document".into());
    };
    // `customCasShape`: with the engine's whole record, the list fields are not read.
    let whole = message.get("state").is_some();
    let dictionaries = match message.get("dictionaries").filter(|_| !whole) {
        None => None,
        Some(Value::Array(dictionaries)) => Some(dictionaries.as_slice()),
        Some(_) => return Err("the custom dictionary write carried an invalid dictionary list".into()),
    };
    if !whole && message.get("groups").is_some_and(|groups| !groups.is_array()) {
        return Err("the custom dictionary write carried invalid groups".into());
    }
    let current = stored_state(storage);
    let legacy = current.is_null() && storage.read(LEGACY_DICTIONARIES_KEY).is_some_and(|rows| rows.is_array());
    let stored_options = storage.read(OPTIONS_KEY);
    assert_state(&current)?;
    if whole {
        assert_own_state_write(sender)?;
    }
    if profile_worker::language_custom_request(message, stored_options.as_ref()) {
        return profile_worker::language_custom_cas(storage, message);
    }
    let document = custom::normalise_document(storage.read(custom::CUSTOM_DICTIONARY_SOURCE_KEY).as_ref())?;
    if !strict_eq(message.get("baseDocumentRevision"), document.get("revision")) {
        return Ok(object(json!({
            "ok": false, "stale": true, "error": "the custom dictionary source changed while it was being saved",
            "document": document, "state": current,
        })));
    }
    if !strict_eq(message.get("baseRevision"), Some(&state_revision(&current))) {
        return Ok(object(json!({
            "ok": false, "conflict": true, "error": "the dictionary state changed while the custom dictionary was being saved",
            "document": document, "state": current,
        })));
    }
    let (entries, _) = custom::parse(text, true);
    let revision = custom::semantic_revision(&entries, true)?;
    if revision != semantic {
        return Err("the custom dictionary semantic revision does not match its source".into());
    }
    let checked = match dictionaries {
        Some(dictionaries) => dictionaries,
        None => listed(current.get("dictionaries"), "dictionaries.filter")?,
    };
    custom::assert_source_state(checked, &revision, entries.len(), &custom::japanese())?;
    let changed = document["text"].as_str() != Some(text) || document["semanticRevision"].as_str() != Some(semantic);
    let next_document = if changed {
        json!({
            "schemaVersion": 1, "revision": js::number(js::to_number(document.get("revision")) + 1.0),
            "semanticRevision": semantic, "text": text,
        })
    } else {
        document
    };
    let mut state = current.clone();
    let mut values = Map::new();
    if changed {
        values.insert(custom::CUSTOM_DICTIONARY_SOURCE_KEY.to_owned(), next_document.clone());
    }
    if let Some(dictionaries) = dictionaries {
        custom::assert_commit(dictionaries, &custom::japanese())?;
        let groups = message.get("groups").or_else(|| current.get("groups"));
        let next_groups = prune_groups(groups, dictionaries)?;
        let list = Value::Array(dictionaries.to_vec());
        let dictionary_changed = current.is_null()
            || !current.get("dictionaries").is_some_and(|before| js::same_json(before, &list))
            || !current.get("groups").is_some_and(|before| js::same_json(before, &next_groups));
        if dictionary_changed {
            let (next, commit) = dictionary_commit(&current, stored_options.as_ref(), dictionaries, Some(&next_groups))?;
            state = next;
            values.extend(commit);
            if legacy {
                values.insert(LEGACY_DICTIONARIES_KEY.to_owned(), Value::Null);
            }
        }
    }
    if !values.is_empty() {
        storage.write(values)?;
    }
    Ok(object(owned_json!({ "document": next_document, "state": state })))
}

/// `responseFits` for an options message: its JSON within 1 MiB of UTF-8.
fn options_frame_fits(value: &Value) -> bool {
    let json = value.to_string();
    let units = js::length(&json);
    units <= MAX_OPTIONS_FRAME_BYTES && (units * 3 <= MAX_OPTIONS_FRAME_BYTES || json.len() <= MAX_OPTIONS_FRAME_BYTES)
}

/// `validResponseRequestId`.
fn valid_request_id(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null | Value::String(_) | Value::Number(_)))
}

/// `boundResponseFailure` of an options write's failure.
fn options_failure(message: &Value, error: &str) -> Value {
    options_coded_failure(message, &Coded::from(error))
}

fn options_coded_failure(message: &Value, failed: &Coded) -> Value {
    let mut reply = crate::host::local::failure_with_code(message, &failed.error, failed.code);
    if !valid_request_id(message.get("requestId")) {
        reply["requestId"] = Value::Null;
    }
    if !options_frame_fits(&reply) {
        let unchanged = reply["error"].as_str() == Some(OPTIONS_LIMIT);
        reply["error"] = json!(OPTIONS_LIMIT);
        if unchanged || !options_frame_fits(&reply) {
            reply["requestId"] = Value::Null;
        }
    }
    reply
}

fn options_write(storage: &impl Storage, message: &Value, capabilities: &[String]) -> Value {
    if !valid_request_id(message.get("requestId")) {
        return options_failure(message, "the options write request carried an invalid request ID");
    }
    if !options_frame_fits(message) {
        return options_failure(message, OPTIONS_LIMIT);
    }
    let result = compatible_options_message(storage, message, capabilities).and_then(|message| {
        let result = options_write_storage(storage, &message)?;
        let reply = reply(&message, result);
        if !options_frame_fits(&reply) {
            return Err(OPTIONS_LIMIT.into());
        }
        Ok(reply)
    });
    result.unwrap_or_else(|failed| options_coded_failure(message, &failed))
}

/// `WORKER_HANDLERS.hd_options_write` with `optionsWriteResult`.
fn options_write_storage(storage: &impl Storage, message: &Value) -> profile_worker::Result<Map<String, Value>> {
    let patch = options::validate_patch(message.get("options"))?;
    let state = stored_state(storage);
    let stored = storage.read(OPTIONS_KEY);
    if js::whole(message.get("baseRevision")).is_none() {
        return Err("the options write request carried no valid base revision".into());
    }
    assert_state(&state)?;
    let revision = options_revision(stored.as_ref());
    if profiles::has_profiles(stored.as_ref()) {
        let result = profile_worker::options_write(message, &patch, &state, stored.as_ref())?;
        if result.get("ok") != Some(&Value::Bool(false))
            && result.get("options").and_then(|options| options.get("revision")) != Some(&js::number(revision))
        {
            storage.write(object(json!({ OPTIONS_KEY: result["options"] })))?;
        }
        return Ok(result);
    }
    // The single-profile shape: its only profile is Japanese, whose pack
    // declares no settings of its own.
    if message.get("profileId").is_some() {
        profile_worker::request_profile_id(message, stored.as_ref())?;
    }
    if let Some(pack_settings) = message.get("packSettings") {
        profiles::apply_pack_settings_patch(stored.as_ref(), Some(pack_settings), JAPANESE)?;
    }
    let mut current = options::project_stored(stored.as_ref());
    current.insert("revision".to_owned(), js::number(revision));
    if !strict_eq(message.get("baseRevision"), Some(&js::number(revision))) {
        return Ok(object(json!({
            "ok": false, "conflict": true,
            "error": "Settings changed in another page. Review your changes before saving again.", "options": current,
        })));
    }
    let mut patched = current;
    patched.extend(patch);
    let mut next = if state.is_null() {
        patched
    } else {
        let dictionaries = listed(state.get("dictionaries"), "dictionaries.find")?;
        let groups = state.get("groups").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
        object(options::normalise_dictionary_selections(&Value::Object(patched), dictionaries, groups)?)
    };
    let mut base = spread(stored.as_ref());
    base.insert("revision".to_owned(), js::number(revision));
    if !js::same_json(&Value::Object(next.clone()), &Value::Object(base)) {
        next.insert("revision".to_owned(), js::number(revision + 1.0));
    }
    let written = next.get("revision").and_then(Value::as_f64).is_some_and(|next| next > revision);
    if written {
        storage.write(object(json!({ OPTIONS_KEY: next.clone() })))?;
    }
    Ok(object(owned_json!({ "options": next })))
}

/// `compatibleLinkedWorkerMessage`: an options write from a client that predates
/// Templates and Custom Buttons edits them as its legacy fields.
fn compatible_options_message(storage: &impl Storage, message: &Value, capabilities: &[String]) -> profile_worker::Result<Value> {
    if capabilities.iter().any(|capability| capability == LINKED_ANKI_CAPABILITY) {
        return Ok(message.clone());
    }
    let Some(patch) = message.get("options").and_then(Value::as_object) else { return Ok(message.clone()) };
    let rich_anki = patch.get("anki").and_then(Value::as_object).is_some_and(|anki| anki.contains_key("templates"));
    if patch.contains_key("customButtons") || rich_anki {
        return Err(LINKED_SETTINGS_UPDATE_REQUIRED.into());
    }
    if !patch.contains_key("customLinks") && !patch.contains_key("anki") {
        return Ok(message.clone());
    }
    let stored = options::normalise(storage.read(OPTIONS_KEY).as_ref());
    let mut compatible = patch.clone();
    if let Some(links) = compatible.remove("customLinks") {
        let buttons = merge_legacy_links(stored["customButtons"].as_array().map_or(&[][..], Vec::as_slice), &links)?;
        compatible.insert("customButtons".to_owned(), Value::Array(buttons));
    }
    if let Some(anki) = patch.get("anki") {
        let legacy = options::validate_patch(Some(&json!({ "anki": anki })))?;
        compatible.insert("anki".to_owned(), merge_legacy_anki(&stored["anki"], &legacy["anki"]));
    }
    let mut message = message.clone();
    message["options"] = Value::Object(compatible);
    Ok(message)
}

/// `mergeLegacyCustomLinks`: legacy links edited in place keep their buttons' identities.
fn merge_legacy_links(current: &[Value], links: &Value) -> Result<Vec<Value>> {
    let incoming = options::validate_patch(Some(&json!({ "customLinks": links })))?;
    let incoming = incoming["customButtons"].as_array().cloned().unwrap_or_default();
    let current_links: Vec<&Value> = current.iter().filter(|button| button["type"] == "link").collect();
    let mut assigned: Vec<Option<usize>> = vec![None; incoming.len()];
    let mut available: Vec<bool> = vec![true; current_links.len()];
    for (index, button) in incoming.iter().enumerate() {
        let found = current_links
            .iter()
            .enumerate()
            .position(|(at, candidate)| available[at] && candidate["label"] == button["label"] && candidate["url"] == button["url"]);
        if let Some(found) = found {
            assigned[index] = Some(found);
            available[found] = false;
        }
    }
    for index in 0..incoming.len() {
        if assigned[index].is_none() && available.get(index).copied().unwrap_or(false) {
            assigned[index] = Some(index);
            available[index] = false;
        }
    }
    let mut used: std::collections::HashSet<String> =
        current.iter().filter(|button| button["type"] != "link").filter_map(|button| button["id"].as_str().map(str::to_owned)).collect();
    used.extend(assigned.iter().flatten().filter_map(|at| current_links[*at]["id"].as_str().map(str::to_owned)));
    let mut generated = 1;
    let next_links: Vec<Value> = incoming
        .into_iter()
        .zip(&assigned)
        .map(|(mut button, assigned)| {
            let id = if let Some(at) = assigned {
                current_links[*at]["id"].clone()
            } else {
                let mut id = format!("legacy-link-{generated}");
                generated += 1;
                while used.contains(&id) {
                    id = format!("legacy-link-{generated}");
                    generated += 1;
                }
                used.insert(id.clone());
                json!(id)
            };
            button["id"] = id;
            button
        })
        .collect();
    let mut links = next_links.into_iter();
    let mut merged = Vec::new();
    for button in current {
        if button["type"] == "link" {
            merged.extend(links.next());
        } else {
            merged.push(button.clone());
        }
    }
    merged.extend(links);
    Ok(merged)
}

/// `mergeLegacyAnki`: the legacy fields edit the first Template.
fn merge_legacy_anki(current: &Value, legacy: &Value) -> Value {
    let templates = current["templates"].as_array().map_or(&[][..], Vec::as_slice);
    let mut first = Map::new();
    first.insert("id".to_owned(), templates.first().map_or(Value::Null, |template| template["id"].clone()));
    first.insert("name".to_owned(), templates.first().map_or(Value::Null, |template| template["name"].clone()));
    for key in options::ANKI_TEMPLATE_CONFIG_KEYS {
        first.insert(key.to_owned(), legacy.get(key).cloned().unwrap_or(Value::Null));
    }
    let mut anki = Map::new();
    anki.insert("url".to_owned(), legacy.get("url").cloned().unwrap_or(Value::Null));
    anki.insert("apiKey".to_owned(), legacy.get("apiKey").cloned().unwrap_or(Value::Null));
    for key in options::ANKI_TEMPLATE_CONFIG_KEYS {
        anki.insert(key.to_owned(), first[key].clone());
    }
    let mut list = vec![Value::Object(first)];
    list.extend(templates.iter().skip(1).cloned());
    anki.insert("templates".to_owned(), Value::Array(list));
    Value::Object(anki)
}

/// `normaliseLookupTerm`: trimmed and NFC.
fn lookup_term(term: Option<&Value>, reading: Option<&Value>) -> Result<(String, String)> {
    let empty = Value::String(String::new());
    let reading = reading.unwrap_or(&empty);
    let (Some(term), Some(reading)) = (term.and_then(Value::as_str), reading.as_str()) else {
        return Err("Lookup statistics require a nonempty term and a reading string.".to_owned());
    };
    if js::trim(term).is_empty() {
        return Err("Lookup statistics require a nonempty term and a reading string.".to_owned());
    }
    let nfc = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
    Ok((nfc.normalize(js::trim(term)).into_owned(), nfc.normalize(js::trim(reading)).into_owned()))
}

/// `assertLookupStatsDescriptor`.
pub(crate) fn assert_descriptor(descriptor: &Value) -> Result<()> {
    let generation = descriptor.get("generation");
    let valid = js::truthy(Some(descriptor))
        && js::safe_integer(descriptor.get("revision")).is_some_and(|revision| revision >= 0.0)
        && (generation == Some(&Value::Null) || generation.and_then(Value::as_str).is_some_and(|generation| !generation.is_empty()))
        && descriptor.get("languages").is_none_or(valid_language_generations);
    if valid { Ok(()) } else { Err("Invalid lookup statistics descriptor.".to_owned()) }
}

/// `validLanguageGenerations`: every counted language but Japanese, by its code,
/// to the generation that keys its rows.
fn valid_language_generations(languages: &Value) -> bool {
    languages.as_object().is_some_and(|languages| {
        languages.iter().all(|(code, generation)| {
            (2..=3).contains(&code.len())
                && code.bytes().all(|b| b.is_ascii_lowercase())
                && code != "ja"
                && generation.as_str().is_some_and(|generation| !generation.is_empty())
        })
    })
}

/// `lookupStatsLanguageView`: the descriptor whose generation keys `language`'s rows.
fn lookup_stats_view(descriptor: &Value, language: &str) -> Value {
    if language == "ja" {
        return descriptor.clone();
    }
    let generation = descriptor.get("languages").and_then(|languages| languages.get(language)).cloned().unwrap_or(Value::Null);
    json!({ "generation": generation, "revision": descriptor["revision"] })
}

/// `withLookupStatsGeneration`: one revision up, with `language`'s generation set.
pub(crate) fn with_lookup_stats_generation(descriptor: &Value, language: &str, generation: &str) -> Result<Value> {
    let mut next = descriptor.clone();
    next["revision"] = js::number(js::to_number(descriptor.get("revision")) + 1.0);
    if language == "ja" {
        next["generation"] = json!(generation);
    } else {
        let mut languages = descriptor.get("languages").and_then(Value::as_object).cloned().unwrap_or_default();
        languages.insert(language.to_owned(), json!(generation));
        next["languages"] = Value::Object(languages);
    }
    assert_descriptor(&next)?;
    Ok(next)
}

/// `assertLookupStatsRow`.
fn assert_row(row: &Value) -> Result<()> {
    let number = |key: &str| row.get(key).and_then(Value::as_f64);
    let term = row.get("term").and_then(Value::as_str);
    let valid = js::truthy(Some(row))
        && term.is_some_and(|term| !js::trim(term).is_empty())
        && row.get("reading").is_some_and(Value::is_string)
        && js::safe_integer(row.get("lookupCount")).is_some_and(|count| count >= 0.0)
        && number("firstLookedUpAt").zip(number("lastLookedUpAt")).is_some_and(|(first, last)| first <= last);
    if !valid {
        return Err("Invalid lookup statistics row.".to_owned());
    }
    let (term, reading) = lookup_term(row.get("term"), row.get("reading"))?;
    if row["term"].as_str() != Some(term.as_str()) || row["reading"].as_str() != Some(reading.as_str()) {
        return Err("The lookup statistics row is not canonical.".to_owned());
    }
    Ok(())
}

/// `lookupStatsKey`.
fn lookup_stats_key(descriptor: &Value, term: &str, reading: &str) -> String {
    format!("{}{}:{}", store::LOOKUP_STATS_ROW_PREFIX, descriptor.get("generation").unwrap_or(&Value::Null), json!([term, reading]))
}

/// `removeObsoleteLookupStatsRows` (`unreachableLookupStatsKeys`): the rows
/// outside the committed descriptor's live generations, which nothing reaches.
pub(crate) fn remove_obsolete_lookup_rows(storage: &impl Storage) -> Result<()> {
    let descriptor = storage.read(LOOKUP_STATS_KEY).unwrap_or_else(|| json!({ "generation": null, "revision": 0 }));
    assert_descriptor(&descriptor)?;
    let prefix = |generation: &Value| format!("{}{}:", store::LOOKUP_STATS_ROW_PREFIX, generation);
    let mut live = vec![prefix(descriptor.get("generation").unwrap_or(&Value::Null))];
    live.extend(descriptor.get("languages").and_then(Value::as_object).into_iter().flatten().map(|(_, generation)| prefix(generation)));
    let unused: Map<String, Value> = storage
        .keys_with(store::LOOKUP_STATS_ROW_PREFIX)
        .into_iter()
        .filter(|key| !live.iter().any(|prefix| key.starts_with(prefix.as_str())))
        .map(|key| (key, Value::Null))
        .collect();
    if unused.is_empty() { Ok(()) } else { storage.write(unused) }
}

/// `crypto.randomUUID()`.
pub(crate) fn random_uuid() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = super::hex(&bytes);
    Ok(format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))
}

/// `lookupStatisticsStorage`, and `languageLookupStatistics` once counts are kept
/// per language: `language`'s rows, under its own generation, every other
/// language's generation kept.
fn lookup_stats(storage: &impl Storage, message: &Value, record: bool) -> profile_worker::Result<Map<String, Value>> {
    let (term, reading) = lookup_term(message.get("term"), message.get("reading"))?;
    let mut descriptor = storage.read(LOOKUP_STATS_KEY).unwrap_or_else(|| json!({ "generation": null, "revision": 0 }));
    assert_descriptor(&descriptor)?;
    let stored_options = storage.read(OPTIONS_KEY);
    let per_language = profiles::has_profiles(stored_options.as_ref()) || descriptor.get("languages").is_some();
    let language = if per_language {
        // `languageLookupStatistics`: the counts of the language of the profile
        // the request is for.
        let has_japanese = profiles::profiles(stored_options.as_ref()).iter().any(|profile| profile["id"] == JAPANESE);
        let profile_id = if message.get("profileId").is_none() && !has_japanese {
            None
        } else {
            Some(profile_worker::request_profile_id(message, stored_options.as_ref())?)
        };
        let language = match &profile_id {
            None => JAPANESE.to_owned(),
            Some(id) => profile_language(stored_options.as_ref(), id),
        };
        let projected = profiles::project_options(stored_options.as_ref(), profile_id.as_deref().unwrap_or(JAPANESE), true);
        if projected.get("showLookupCounts") == Some(&Value::Bool(false)) {
            return Ok(object(json!({ "descriptor": descriptor, "statistics": null })));
        }
        language
    } else {
        if message.get("profileId").is_some() {
            profile_worker::request_profile_id(message, stored_options.as_ref())?;
        }
        if stored_options.as_ref().is_some_and(|options| options.get("showLookupCounts") == Some(&Value::Bool(false))) {
            return Ok(object(json!({ "descriptor": descriptor, "statistics": null })));
        }
        JAPANESE.to_owned()
    };
    let language = language.as_str();
    let view = lookup_stats_view(&descriptor, language);
    let key = lookup_stats_key(&view, &term, &reading);
    let mut row = if view["generation"].is_null() { None } else { storage.read(&key) };
    if record {
        let now = js::now();
        if let Some(previous) = &row {
            assert_row(previous)?;
            if previous["term"].as_str() != Some(term.as_str()) || previous["reading"].as_str() != Some(reading.as_str()) {
                return Err("The lookup statistics row does not match its key.".into());
            }
        }
        let previous = |key: &str| row.as_ref().and_then(|row| row[key].as_f64());
        let next = json!({
            "term": term, "reading": reading, "lookupCount": js::number(previous("lookupCount").unwrap_or(0.0) + 1.0),
            "firstLookedUpAt": js::number(previous("firstLookedUpAt").unwrap_or(now).min(now)),
            "lastLookedUpAt": js::number(previous("lastLookedUpAt").unwrap_or(now).max(now)),
        });
        assert_row(&next)?;
        let revision = js::number(js::to_number(descriptor.get("revision")) + 1.0);
        descriptor = if !per_language {
            let generation = if view["generation"].is_null() { json!(random_uuid()?) } else { view["generation"].clone() };
            json!({ "generation": generation, "revision": revision })
        } else if view["generation"].is_null() {
            // The language's first row creates its generation in the same write.
            with_lookup_stats_generation(&descriptor, language, &random_uuid()?)?
        } else {
            let mut next = descriptor.clone();
            next["revision"] = revision;
            next
        };
        assert_descriptor(&descriptor)?;
        let mut values = Map::new();
        values.insert(LOOKUP_STATS_KEY.to_owned(), descriptor.clone());
        values.insert(lookup_stats_key(&lookup_stats_view(&descriptor, language), &term, &reading), next.clone());
        storage.write(values)?;
        row = Some(next);
    } else if let Some(row) = &row {
        assert_row(row)?;
        if lookup_stats_key(&view, row["term"].as_str().unwrap_or_default(), row["reading"].as_str().unwrap_or_default()) != key {
            return Err("The lookup statistics row does not match its key.".into());
        }
    }
    let statistics = row.unwrap_or_else(|| json!({ "term": term, "reading": reading, "lookupCount": 0 }));
    Ok(object(json!({ "descriptor": descriptor, "statistics": statistics })))
}

/// `normaliseWordStatusOverrides`.
pub(crate) fn normalise_overrides(value: Option<&Value>) -> Map<String, Value> {
    let revision = js::safe_integer(value.and_then(|value| value.get("revision"))).filter(|revision| *revision >= 0.0).unwrap_or(0.0);
    let mut record = Map::new();
    record.insert("revision".to_owned(), js::number(revision));
    let mut seen = std::collections::HashSet::new();
    for status in OVERRIDE_STATUSES {
        let words: Vec<Value> = value
            .and_then(|value| value.get(status))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|word| word.as_str().is_some_and(|word| !word.is_empty() && seen.insert(word.to_owned())))
            .cloned()
            .collect();
        record.insert(status.to_owned(), Value::Array(words));
    }
    record
}

/// `overrideLanguages`: every other language's marks, by language.
fn override_languages(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value?.get("languages")?.as_object()
}

/// `wordStatusOverridesFor`: one language's marks as a record of their own, at the
/// record's revision.
pub(crate) fn overrides_for(value: Option<&Value>, language: &str) -> Map<String, Value> {
    if language == "ja" {
        return normalise_overrides(value);
    }
    let mut entry =
        override_languages(value).and_then(|languages| languages.get(language)).and_then(Value::as_object).cloned().unwrap_or_default();
    entry.insert("revision".to_owned(), value.and_then(|value| value.get("revision")).cloned().unwrap_or(Value::Null));
    normalise_overrides(Some(&Value::Object(entry)))
}

/// `withLanguageWordStatusOverrides`: the record with `language`'s marks replaced by
/// `record`'s and every other language's kept, at `record`'s revision.
pub(crate) fn with_language_overrides(value: Option<&Value>, language: &str, record: Map<String, Value>) -> Value {
    let marks = |record: &Map<String, Value>| json!({ "known": record["known"], "ignored": record["ignored"] });
    let mut languages: Map<String, Value> = override_languages(value)
        .into_iter()
        .flatten()
        .map(|(code, entry)| (code.clone(), marks(&normalise_overrides(Some(entry)))))
        .collect();
    let mut top = normalise_overrides(value);
    if language == "ja" {
        top = record;
    } else {
        languages.insert(language.to_owned(), marks(&record));
        top.insert("revision".to_owned(), record["revision"].clone());
    }
    let empty = |list: &Value| list.as_array().is_none_or(Vec::is_empty);
    languages.retain(|_, entry| !(empty(&entry["known"]) && empty(&entry["ignored"])));
    if !languages.is_empty() {
        top.insert("languages".to_owned(), Value::Object(languages));
    }
    Value::Object(top)
}

/// `hd_word_status_override` with `withLanguageWordStatusOverride`, for `language`'s
/// marks: Japanese on a record without `languages` is `withWordStatusOverride`.
/// The language of the profile `id`, which exists.
fn profile_language(stored_options: Option<&Value>, id: &str) -> String {
    profile_worker::profile_languages(stored_options)
        .into_iter()
        .find(|(profile, _)| profile == id)
        .map_or_else(|| JAPANESE.to_owned(), |(_, language)| language)
}

/// The language of the profile a request names, Japanese when it names none.
fn request_language(storage: &impl Storage, message: &Value) -> profile_worker::Result<String> {
    if message.get("profileId").is_none() {
        return Ok(JAPANESE.to_owned());
    }
    let stored_options = storage.read(OPTIONS_KEY);
    let id = profile_worker::request_profile_id(message, stored_options.as_ref())?;
    Ok(profile_language(stored_options.as_ref(), &id))
}

/// `hd_known_words_add` with `withKnownWords`: Settings' pasted list, one word per
/// line, joins the language's known words in one revision and leaves its ignored
/// list; `added` counts the words that were not known yet.
fn known_words_add(storage: &impl Storage, message: &Value) -> profile_worker::Result<Map<String, Value>> {
    let language = request_language(storage, message)?;
    let stored = storage.read(WORD_STATUS_OVERRIDES_KEY);
    let Some(text) = message.get("text").and_then(Value::as_str) else {
        return Err("Known words must be text, one word per line.".into());
    };
    let current = overrides_for(stored.as_ref(), &language);
    let words = |status: &str| current[status].as_array().cloned().unwrap_or_default();
    let mut known = words("known");
    let before = known.len();
    // A set, as upstream's, so a long pasted list is merged once.
    let mut seen: std::collections::HashSet<String> = known.iter().filter_map(Value::as_str).map(str::to_owned).collect();
    // `text.split(/\r?\n/u)`, each line trimmed.
    for line in text.split('\n') {
        let word = js::trim(line.strip_suffix('\r').unwrap_or(line));
        if !word.is_empty() && seen.insert(word.to_owned()) {
            known.push(json!(word));
        }
    }
    let added = known.len() - before;
    if added == 0 {
        return Ok(object(json!({ "revision": normalise_overrides(stored.as_ref())["revision"], "added": 0 })));
    }
    let ignored: Vec<Value> = words("ignored").into_iter().filter(|word| !word.as_str().is_some_and(|word| seen.contains(word))).collect();
    let mut next = Map::new();
    next.insert("revision".to_owned(), js::number(js::to_number(current.get("revision")) + 1.0));
    next.insert("known".to_owned(), Value::Array(known));
    next.insert("ignored".to_owned(), Value::Array(ignored));
    let record = with_language_overrides(stored.as_ref(), &language, next);
    storage.write(object(json!({ WORD_STATUS_OVERRIDES_KEY: record })))?;
    Ok(object(json!({ "revision": normalise_overrides(Some(&record))["revision"], "added": added })))
}

/// `hd_word_status_override`: a request names the profile whose language the
/// headword belongs to; the other languages' marks are kept.
fn word_status_override(storage: &impl Storage, message: &Value) -> profile_worker::Result<Map<String, Value>> {
    let language = request_language(storage, message)?;
    let language = language.as_str();
    let stored = storage.read(WORD_STATUS_OVERRIDES_KEY);
    let current = overrides_for(stored.as_ref(), language);
    let Some(headword) = message.get("headword").and_then(Value::as_str).filter(|headword| !headword.is_empty()) else {
        return Err("A word status override needs a headword.".into());
    };
    let status = match message.get("status") {
        Some(Value::Null) => None,
        Some(Value::String(status)) if OVERRIDE_STATUSES.contains(&status.as_str()) => Some(status.as_str()),
        _ => return Err("A word status override is known, ignored or null.".into()),
    };
    let listed = |status: &str| current[status].as_array().is_some_and(|words| words.iter().any(|word| word == headword));
    let existing = OVERRIDE_STATUSES.iter().copied().find(|status| listed(status));
    if existing == status {
        return Ok(object(json!({ "revision": normalise_overrides(stored.as_ref())["revision"] })));
    }
    let revision = js::number(js::to_number(current.get("revision")) + 1.0);
    let mut next = Map::new();
    next.insert("revision".to_owned(), revision.clone());
    for candidate in OVERRIDE_STATUSES {
        let mut words: Vec<Value> = current[candidate].as_array().into_iter().flatten().filter(|word| *word != headword).cloned().collect();
        if Some(candidate) == status {
            words.push(json!(headword));
        }
        next.insert(candidate.to_owned(), Value::Array(words));
    }
    let next = if language == "ja" && override_languages(stored.as_ref()).is_none() {
        Value::Object(next)
    } else {
        with_language_overrides(stored.as_ref(), language, next)
    };
    storage.write(object(json!({ WORD_STATUS_OVERRIDES_KEY: next })))?;
    Ok(object(json!({ "revision": revision })))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;

    /// Upstream's dictionary-update-schedule.test.mjs, first case: what a
    /// per-dictionary schedule may be.
    #[test]
    fn per_dictionary_schedules_are_inherit_or_a_policy() {
        for value in [
            None,
            Some(json!(null)),
            Some(json!("off")),
            Some(json!("hourly")),
            Some(json!("daily")),
            Some(json!("weekly")),
            Some(json!("monthly")),
        ] {
            let dictionary = value.map_or_else(|| json!({}), |value| json!({ "updateScheduleOverride": value }));
            assert_eq!(assert_update_schedules(&[dictionary]), Ok(()));
        }
        for value in [json!(false), json!("inherit"), json!("sometimes")] {
            let refused = assert_update_schedules(&[json!({ "updateScheduleOverride": value })]).unwrap_err();
            assert!(refused.contains("schedule"), "{refused}");
        }
    }

    #[derive(Default)]
    pub(crate) struct Memory(pub(crate) RefCell<BTreeMap<String, Value>>);

    impl Storage for Memory {
        fn read(&self, key: &str) -> Option<Value> {
            self.0.borrow().get(key).cloned()
        }

        fn keys_with(&self, prefix: &str) -> Vec<String> {
            self.0.borrow().keys().filter(|key| key.starts_with(prefix)).cloned().collect()
        }

        fn write(&self, values: Map<String, Value>) -> std::result::Result<(), String> {
            let mut stored = self.0.borrow_mut();
            for (key, value) in values {
                if value.is_null() {
                    stored.remove(&key);
                } else {
                    stored.insert(key, value);
                }
            }
            Ok(())
        }
    }

    /// Options with a Japanese profile and one for each of `languages`.
    fn with_profiles(storage: &Memory, languages: &[&str]) {
        let mut list = vec![json!({ "id": "ja", "language": "ja", "settings": {} })];
        list.extend(languages.iter().map(|code| json!({ "id": code, "language": code, "settings": {} })));
        storage.write(object(json!({ OPTIONS_KEY: { "revision": 1, "profiles": list } }))).unwrap();
    }

    fn request(storage: &Memory, fields: &Value) -> Value {
        let mut message = json!({ "target": "hoshidicts-worker", "requestId": 7 });
        message.as_object_mut().into_iter().for_each(|message| message.extend(object(fields.clone())));
        handle(storage, &message, &[LINKED_ANKI_CAPABILITY.to_owned()])
    }

    #[test]
    fn a_state_commit_follows_renamed_and_removed_selections() {
        let storage = Memory::default();
        let library = json!([
            { "id": "a", "title": "Words", "termCount": 3 },
            { "id": "b", "title": "Freq", "frequencyCount": 2 },
        ]);
        storage.0.borrow_mut().insert(
            "dictionaryState".into(),
            json!({ "schemaVersion": 1, "revision": 1, "dictionaries": library, "groups": [{ "id": "g", "name": "G", "dictionaryIds": ["a", "b"] }] }),
        );
        storage
            .0
            .borrow_mut()
            .insert("options".into(), json!({ "revision": 4, "frequencyDictionary": "Freq", "kanjiClickDictionary": "Words" }));
        let stale = request(&storage, &json!({ "type": "hd_state_cas", "baseRevision": 0, "dictionaries": [] }));
        assert_eq!((stale["ok"].clone(), stale["conflict"].clone()), (json!(false), json!(true)));
        let renamed = json!([{ "id": "b", "title": "Freq 2", "frequencyCount": 2 }]);
        let reply = request(&storage, &json!({ "type": "hd_state_cas", "baseRevision": 1, "dictionaries": renamed }));
        assert_eq!(reply["type"], "hd_state_cas_result");
        assert_eq!(reply["requestId"], 7);
        assert_eq!(reply["state"]["revision"], 2);
        assert_eq!(reply["state"]["groups"], json!([{ "id": "g", "name": "G", "dictionaryIds": ["b"] }]));
        let options = storage.read("options").unwrap_or_default();
        assert_eq!((options["revision"].clone(), options["frequencyDictionary"].clone()), (json!(5), json!("Freq 2")));
        assert_eq!(options["kanjiClickDictionary"], "");
    }

    // A host with language profiles keeps other languages' marks under
    // `languages` (word-status-overrides.js): a Japanese mark keeps them, a
    // language's last mark cleared drops its entry, and a malformed record is
    // normalised as the extension normalises it.
    #[test]
    fn word_status_marks_of_other_languages_are_kept() {
        let storage = Memory::default();
        storage
            .write(object(json!({ WORD_STATUS_OVERRIDES_KEY: {
            "revision": 4, "known": ["猫", 7, "猫", ""], "ignored": ["猫", "犬"],
            "languages": { "en": { "known": ["cat"], "ignored": ["cat", "dog"] } },
        } })))
            .unwrap();
        let japanese = request(&storage, &json!({ "type": "hd_word_status_override", "headword": "鳥", "status": "known" }));
        assert_eq!(japanese["revision"], 5);
        assert_eq!(
            storage.read(WORD_STATUS_OVERRIDES_KEY).unwrap(),
            json!({ "revision": 5, "known": ["猫", "鳥"], "ignored": ["犬"], "languages": { "en": { "known": ["cat"], "ignored": ["dog"] } } })
        );
        assert_eq!(overrides_for(storage.read(WORD_STATUS_OVERRIDES_KEY).as_ref(), "en")["known"], json!(["cat"]));
        with_profiles(&storage, &["en"]);
        let english = word_status_override(&storage, &json!({ "headword": "cat", "status": null, "profileId": "en" })).unwrap();
        assert_eq!(english["revision"], 6);
        let english = word_status_override(&storage, &json!({ "headword": "dog", "status": null, "profileId": "en" })).unwrap();
        assert_eq!(english["revision"], 7);
        assert_eq!(storage.read(WORD_STATUS_OVERRIDES_KEY).unwrap(), json!({ "revision": 7, "known": ["猫", "鳥"], "ignored": ["犬"] }));
        word_status_override(&storage, &json!({ "headword": "bird", "status": "ignored", "profileId": "en" })).unwrap();
        assert_eq!(storage.read(WORD_STATUS_OVERRIDES_KEY).unwrap()["languages"], json!({ "en": { "known": [], "ignored": ["bird"] } }));
    }

    /// Upstream's word-status-overrides.test.mjs "pasted known words append once,
    /// move ignored words and keep other languages", through `hd_known_words_add`.
    #[test]
    fn pasted_known_words_append_once() {
        let storage = Memory::default();
        let stored = json!({ "revision": 4, "known": ["猫"], "ignored": ["犬", "さん"],
            "languages": { "en": { "known": ["cat"], "ignored": ["dog"] } } });
        storage.write(object(json!({ WORD_STATUS_OVERRIDES_KEY: stored }))).unwrap();
        let reply = request(&storage, &json!({ "type": "hd_known_words_add", "text": " 猫 \r\n犬\r\n\n学校\n学校\n " }));
        assert_eq!((reply["ok"].clone(), reply["revision"].clone(), reply["added"].clone()), (json!(true), json!(5), json!(2)));
        let record = json!({ "revision": 5, "known": ["猫", "犬", "学校"], "ignored": ["さん"],
            "languages": { "en": { "known": ["cat"], "ignored": ["dog"] } } });
        assert_eq!(storage.read(WORD_STATUS_OVERRIDES_KEY).unwrap(), record);
        for text in ["", " \r\n ", "猫\n犬\n学校"] {
            let reply = request(&storage, &json!({ "type": "hd_known_words_add", "text": text }));
            assert_eq!((reply["revision"].clone(), reply["added"].clone()), (json!(5), json!(0)), "{text:?}");
            assert_eq!(storage.read(WORD_STATUS_OVERRIDES_KEY).unwrap(), record);
        }
        with_profiles(&storage, &["en"]);
        let english = request(&storage, &json!({ "type": "hd_known_words_add", "text": "dog\nice cream", "profileId": "en" }));
        assert_eq!(english["added"], 2);
        assert_eq!(
            storage.read(WORD_STATUS_OVERRIDES_KEY).unwrap()["languages"],
            json!({ "en": { "known": ["cat", "dog", "ice cream"], "ignored": [] } })
        );
        let refused = request(&storage, &json!({ "type": "hd_known_words_add", "text": 3 }));
        assert_eq!(refused["error"], "Known words must be text, one word per line.");
        let missing = request(&storage, &json!({ "type": "hd_known_words_add", "text": "x", "profileId": "fr" }));
        assert_eq!(missing["ok"], false);
    }

    // Lookup counts per language (lookup-stats.js): Japanese's generation at
    // the top, every other language's under `languages`, all kept by a write.
    #[test]
    fn lookup_counts_of_other_languages_are_kept() {
        let storage = Memory::default();
        storage.write(object(json!({ LOOKUP_STATS_KEY: { "generation": "g-ja", "revision": 3, "languages": { "en": "g-en" } } }))).unwrap();
        let japanese = request(&storage, &json!({ "type": "hd_lookup_stats_record", "term": "猫" }));
        assert_eq!(japanese["descriptor"], json!({ "generation": "g-ja", "revision": 4, "languages": { "en": "g-en" } }));
        assert!(storage.read(&lookup_stats_key(&json!({ "generation": "g-ja" }), "猫", "")).is_some());
        with_profiles(&storage, &["en", "fr"]);
        let english = lookup_stats(&storage, &json!({ "term": "cat", "profileId": "en" }), true).unwrap();
        assert_eq!(english["descriptor"]["revision"], 5);
        assert_eq!(english["statistics"]["lookupCount"], 1);
        assert!(storage.read(&lookup_stats_key(&json!({ "generation": "g-en" }), "cat", "")).is_some());
        let french = lookup_stats(&storage, &json!({ "term": "chat", "profileId": "fr" }), true).unwrap();
        let generation = french["descriptor"]["languages"]["fr"].as_str().unwrap().to_owned();
        assert_eq!(french["descriptor"]["languages"]["en"], "g-en");
        assert!(storage.read(&lookup_stats_key(&json!({ "generation": generation }), "chat", "")).is_some());
        for languages in [json!({ "ja": "g" }), json!({ "EN": "g" }), json!({ "en": "" }), json!([])] {
            storage.write(object(json!({ LOOKUP_STATS_KEY: { "generation": null, "revision": 0, "languages": languages } }))).unwrap();
            let refused = request(&storage, &json!({ "type": "hd_lookup_stats_read", "term": "猫" }));
            assert_eq!(refused["error"], "Invalid lookup statistics descriptor.", "{languages}");
        }
    }

    #[test]
    fn word_status_and_lookup_counts_change_one_entry_at_a_time() {
        let storage = Memory::default();
        let known = request(&storage, &json!({ "type": "hd_word_status_override", "headword": "猫", "status": "known" }));
        assert_eq!(known["revision"], 1);
        let again = request(&storage, &json!({ "type": "hd_word_status_override", "headword": "猫", "status": "known" }));
        assert_eq!(again["revision"], 1);
        let bad = request(&storage, &json!({ "type": "hd_word_status_override", "headword": "猫", "status": "learning" }));
        assert_eq!(bad["error"], "A word status override is known, ignored or null.");

        let first = request(&storage, &json!({ "type": "hd_lookup_stats_record", "term": " 猫 " }));
        let second = request(&storage, &json!({ "type": "hd_lookup_stats_record", "term": "猫", "reading": "" }));
        assert_eq!(first["descriptor"]["generation"], second["descriptor"]["generation"]);
        assert_eq!(second["descriptor"]["revision"], 2);
        assert_eq!(second["statistics"]["lookupCount"], 2);
        let key = lookup_stats_key(&second["descriptor"], "猫", "");
        assert!(key.starts_with("lookupStats:\""), "{key}");
        assert_eq!(storage.read(&key).map(|row| row["lookupCount"].clone()), Some(json!(2)));
    }

    #[test]
    fn a_legacy_client_cannot_edit_templates_but_its_links_keep_their_buttons() {
        let storage = Memory::default();
        storage.0.borrow_mut().insert(
            "options".into(),
            json!({ "revision": 0, "customButtons": [{ "id": "keep", "type": "link", "label": "J", "url": "https://j/{term}" }] }),
        );
        let legacy = |options: Value| {
            let message = json!({ "target": "hoshidicts-worker", "type": "hd_options_write", "baseRevision": 0, "options": options });
            handle(&storage, &message, &[])
        };
        assert_eq!(legacy(json!({ "customButtons": [] }))["error"], LINKED_SETTINGS_UPDATE_REQUIRED);
        let reply =
            legacy(json!({ "customLinks": [{ "label": "W", "url": "https://w/{term}" }, { "label": "J", "url": "https://j/{term}" }] }));
        let ids: Vec<&Value> = reply["options"]["customButtons"].as_array().into_iter().flatten().map(|button| &button["id"]).collect();
        assert_eq!(ids, [&json!("legacy-link-1"), &json!("keep")]);
        assert_eq!(reply["options"]["revision"], 1);
    }
}
