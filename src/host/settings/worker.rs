// SPDX-License-Identifier: GPL-3.0-or-later
//! The service worker's storage requests a linked browser forwards to its host
//! (`WORKER_HANDLERS` in the extension's `background-requests.js`, with the commit
//! rules of `background-core.js`): dictionary state and custom-source CAS, options
//! writes, lookup counts and word-status overrides.
//!
//! The caller runs one request at a time (`serialiseStorage`).

use serde_json::{Map, Value, json};

use super::{custom, options};
use crate::host::js::{self, strict_eq};
use crate::host::store;

/// The worker requests a linked browser forwards (`FORWARDED_REQUESTS`).
pub(crate) const REQUESTS: [&str; 8] = [
    "hd_state_read",
    "hd_state_cas",
    "hd_custom_read",
    "hd_custom_cas",
    "hd_options_write",
    "hd_lookup_stats_read",
    "hd_lookup_stats_record",
    "hd_word_status_override",
];
/// The capability of a linked client that edits Templates and Custom Buttons itself.
pub(crate) const LINKED_ANKI_CAPABILITY: &str = "linked-anki-v2";

const OPTIONS_KEY: &str = "options";
const LEGACY_DICTIONARIES_KEY: &str = "dictionaries";
const LOOKUP_STATS_KEY: &str = "lookupStats";
const WORD_STATUS_OVERRIDES_KEY: &str = "wordStatusOverrides";
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
}

type Result<T> = std::result::Result<T, String>;

/// Answers one worker request with its reply (`workerReply` or `failureReply`).
pub(crate) fn handle(storage: &impl Storage, message: &Value, capabilities: &[String]) -> Value {
    let kind = message.get("type").and_then(Value::as_str).unwrap_or_default();
    let result = match kind {
        "hd_state_read" => Ok(state_read(storage)),
        "hd_state_cas" => state_cas(storage, message),
        "hd_custom_read" => custom_read(storage),
        "hd_custom_cas" => custom_cas(storage, message),
        "hd_options_write" => return options_write(storage, message, capabilities),
        "hd_lookup_stats_read" => lookup_stats(storage, message, false),
        "hd_lookup_stats_record" => lookup_stats(storage, message, true),
        "hd_word_status_override" => word_status_override(storage, message),
        other => Err(format!("unknown worker request type {}", Value::from(other))),
    };
    match result {
        Ok(result) => reply(message, result),
        Err(error) => failure(message, &error),
    }
}

/// `workerReply`.
fn reply(message: &Value, mut result: Map<String, Value>) -> Value {
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

fn failure(message: &Value, error: &str) -> Value {
    crate::host::local::failure(message, error)
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(fields) => fields,
        _ => Map::new(),
    }
}

/// `{ ...value }` for what storage holds: an object's fields, nothing otherwise.
fn spread(value: Option<&Value>) -> Map<String, Value> {
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
fn stored_state(storage: &impl Storage) -> Value {
    storage.read(store::DICTIONARY_STATE).unwrap_or(Value::Null)
}

/// `assertDictionaryState`.
fn assert_state(state: &Value) -> Result<()> {
    if !state.is_null() && !strict_eq(state.get("schemaVersion"), Some(&json!(1))) {
        return Err(format!("unsupported dictionary state schema {}", js::to_string(state.get("schemaVersion"))));
    }
    Ok(())
}

/// `current?.revision ?? 0`.
fn state_revision(state: &Value) -> Value {
    state.get("revision").filter(|revision| !revision.is_null()).cloned().unwrap_or(json!(0))
}

/// `optionsRevision`.
pub(crate) fn options_revision(options: Option<&Value>) -> f64 {
    js::whole(options.and_then(|options| options.get("revision"))).unwrap_or(0.0)
}

fn state_read(storage: &impl Storage) -> Map<String, Value> {
    let state = stored_state(storage);
    let legacy = if state.is_null() { storage.read(LEGACY_DICTIONARIES_KEY).filter(Value::is_array) } else { None };
    object(owned_json!({ "state": state, "legacyDictionaries": legacy }))
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
fn prune_groups(value: Option<&Value>, dictionaries: &[Value]) -> Result<Value> {
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
fn assert_update_schedules(dictionaries: &[Value]) -> Result<()> {
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
fn migrate_selections(mut options: Map<String, Value>, current: &Value, dictionaries: &[Value]) -> Result<Map<String, Value>> {
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

fn state_cas(storage: &impl Storage, message: &Value) -> Result<Map<String, Value>> {
    if js::whole(message.get("baseRevision")).is_none() {
        return Err("the dictionary state write request carried no valid base revision".to_owned());
    }
    let Some(dictionaries) = message.get("dictionaries").and_then(Value::as_array) else {
        return Err("the dictionary state write request carried no list".to_owned());
    };
    let groups = message.get("groups");
    if groups.is_some_and(|groups| !groups.is_array()) {
        return Err("the dictionary state write request carried invalid groups".to_owned());
    }
    let current = stored_state(storage);
    let legacy = current.is_null() && storage.read(LEGACY_DICTIONARIES_KEY).is_some_and(|rows| rows.is_array());
    assert_state(&current)?;
    if !strict_eq(message.get("baseRevision"), Some(&state_revision(&current))) {
        return Ok(object(json!({
            "ok": false, "conflict": true, "error": "the dictionary state changed while it was being written", "state": current,
        })));
    }
    let transition = listed(current.get("dictionaries"), "currentDictionaries.findIndex")
        .and_then(|before| custom::assert_ordinary_transition(before, dictionaries));
    if let Err(error) = transition {
        return Ok(object(json!({ "ok": false, "protected": true, "error": error, "state": current })));
    }
    let (state, mut values) = dictionary_commit(&current, storage.read(OPTIONS_KEY).as_ref(), dictionaries, groups)?;
    if legacy {
        values.insert(LEGACY_DICTIONARIES_KEY.to_owned(), Value::Null);
    }
    storage.write(values)?;
    Ok(object(owned_json!({ "state": state })))
}

fn custom_read(storage: &impl Storage) -> Result<Map<String, Value>> {
    let state = stored_state(storage);
    assert_state(&state)?;
    let document = custom::normalise_document(storage.read(custom::CUSTOM_DICTIONARY_SOURCE_KEY).as_ref())?;
    Ok(object(owned_json!({ "document": document, "state": state })))
}

fn custom_cas(storage: &impl Storage, message: &Value) -> Result<Map<String, Value>> {
    if js::whole(message.get("baseDocumentRevision")).is_none() {
        return Err("the custom dictionary write carried no valid document revision".to_owned());
    }
    if js::whole(message.get("baseRevision")).is_none() {
        return Err("the custom dictionary write carried no valid dictionary revision".to_owned());
    }
    let (Some(text), Some(semantic)) =
        (message.get("text").and_then(Value::as_str), message.get("semanticRevision").and_then(Value::as_str))
    else {
        return Err("the custom dictionary write carried no source document".to_owned());
    };
    let dictionaries = match message.get("dictionaries") {
        None => None,
        Some(Value::Array(dictionaries)) => Some(dictionaries.as_slice()),
        Some(_) => return Err("the custom dictionary write carried an invalid dictionary list".to_owned()),
    };
    if message.get("groups").is_some_and(|groups| !groups.is_array()) {
        return Err("the custom dictionary write carried invalid groups".to_owned());
    }
    let current = stored_state(storage);
    let legacy = current.is_null() && storage.read(LEGACY_DICTIONARIES_KEY).is_some_and(|rows| rows.is_array());
    let stored_options = storage.read(OPTIONS_KEY);
    assert_state(&current)?;
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
    let (entries, _) = custom::parse(text);
    let revision = custom::semantic_revision(&entries)?;
    if revision != semantic {
        return Err("the custom dictionary semantic revision does not match its source".to_owned());
    }
    let checked = match dictionaries {
        Some(dictionaries) => dictionaries,
        None => listed(current.get("dictionaries"), "dictionaries.filter")?,
    };
    custom::assert_source_state(checked, &revision, entries.len())?;
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
        custom::assert_commit(dictionaries)?;
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
    let mut reply = failure(message, error);
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
            return Err(OPTIONS_LIMIT.to_owned());
        }
        Ok(reply)
    });
    result.unwrap_or_else(|error| options_failure(message, &error))
}

/// `WORKER_HANDLERS.hd_options_write` with `optionsWriteResult`.
fn options_write_storage(storage: &impl Storage, message: &Value) -> Result<Map<String, Value>> {
    let patch = options::validate_patch(message.get("options"))?;
    let state = stored_state(storage);
    let stored = storage.read(OPTIONS_KEY);
    if js::whole(message.get("baseRevision")).is_none() {
        return Err("the options write request carried no valid base revision".to_owned());
    }
    assert_state(&state)?;
    let revision = options_revision(stored.as_ref());
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
fn compatible_options_message(storage: &impl Storage, message: &Value, capabilities: &[String]) -> Result<Value> {
    if capabilities.iter().any(|capability| capability == LINKED_ANKI_CAPABILITY) {
        return Ok(message.clone());
    }
    let Some(patch) = message.get("options").and_then(Value::as_object) else { return Ok(message.clone()) };
    let rich_anki = patch.get("anki").and_then(Value::as_object).is_some_and(|anki| anki.contains_key("templates"));
    if patch.contains_key("customButtons") || rich_anki {
        return Err(LINKED_SETTINGS_UPDATE_REQUIRED.to_owned());
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
fn assert_descriptor(descriptor: &Value) -> Result<()> {
    let generation = descriptor.get("generation");
    let valid = js::truthy(Some(descriptor))
        && js::safe_integer(descriptor.get("revision")).is_some_and(|revision| revision >= 0.0)
        && (generation == Some(&Value::Null) || generation.and_then(Value::as_str).is_some_and(|generation| !generation.is_empty()));
    if valid { Ok(()) } else { Err("Invalid lookup statistics descriptor.".to_owned()) }
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

/// `crypto.randomUUID()`.
pub(crate) fn random_uuid() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = super::hex(&bytes);
    Ok(format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))
}

/// `lookupStatisticsStorage`.
fn lookup_stats(storage: &impl Storage, message: &Value, record: bool) -> Result<Map<String, Value>> {
    let (term, reading) = lookup_term(message.get("term"), message.get("reading"))?;
    let mut descriptor = storage.read(LOOKUP_STATS_KEY).unwrap_or_else(|| json!({ "generation": null, "revision": 0 }));
    assert_descriptor(&descriptor)?;
    if storage.read(OPTIONS_KEY).is_some_and(|options| options.get("showLookupCounts") == Some(&Value::Bool(false))) {
        return Ok(object(json!({ "descriptor": descriptor, "statistics": null })));
    }
    let key = lookup_stats_key(&descriptor, &term, &reading);
    let mut row = if descriptor["generation"].is_null() { None } else { storage.read(&key) };
    if record {
        let now = js::now();
        if let Some(previous) = &row {
            assert_row(previous)?;
            if previous["term"].as_str() != Some(term.as_str()) || previous["reading"].as_str() != Some(reading.as_str()) {
                return Err("The lookup statistics row does not match its key.".to_owned());
            }
        }
        let previous = |key: &str| row.as_ref().and_then(|row| row[key].as_f64());
        let next = json!({
            "term": term, "reading": reading, "lookupCount": js::number(previous("lookupCount").unwrap_or(0.0) + 1.0),
            "firstLookedUpAt": js::number(previous("firstLookedUpAt").unwrap_or(now).min(now)),
            "lastLookedUpAt": js::number(previous("lastLookedUpAt").unwrap_or(now).max(now)),
        });
        assert_row(&next)?;
        let generation = match &descriptor["generation"] {
            Value::Null => json!(random_uuid()?),
            generation => generation.clone(),
        };
        descriptor = json!({ "generation": generation, "revision": js::number(js::to_number(descriptor.get("revision")) + 1.0) });
        assert_descriptor(&descriptor)?;
        let mut values = Map::new();
        values.insert(LOOKUP_STATS_KEY.to_owned(), descriptor.clone());
        values.insert(lookup_stats_key(&descriptor, &term, &reading), next.clone());
        storage.write(values)?;
        row = Some(next);
    } else if let Some(row) = &row {
        assert_row(row)?;
        if lookup_stats_key(&descriptor, row["term"].as_str().unwrap_or_default(), row["reading"].as_str().unwrap_or_default()) != key {
            return Err("The lookup statistics row does not match its key.".to_owned());
        }
    }
    let statistics = row.unwrap_or_else(|| json!({ "term": term, "reading": reading, "lookupCount": 0 }));
    Ok(object(json!({ "descriptor": descriptor, "statistics": statistics })))
}

/// `normaliseWordStatusOverrides`.
fn normalise_overrides(value: Option<&Value>) -> Map<String, Value> {
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

/// `hd_word_status_override` with `withWordStatusOverride`.
fn word_status_override(storage: &impl Storage, message: &Value) -> Result<Map<String, Value>> {
    let current = normalise_overrides(storage.read(WORD_STATUS_OVERRIDES_KEY).as_ref());
    let Some(headword) = message.get("headword").and_then(Value::as_str).filter(|headword| !headword.is_empty()) else {
        return Err("A word status override needs a headword.".to_owned());
    };
    let status = match message.get("status") {
        Some(Value::Null) => None,
        Some(Value::String(status)) if OVERRIDE_STATUSES.contains(&status.as_str()) => Some(status.as_str()),
        _ => return Err("A word status override is known, ignored or null.".to_owned()),
    };
    let listed = |status: &str| current[status].as_array().is_some_and(|words| words.iter().any(|word| word == headword));
    let existing = OVERRIDE_STATUSES.iter().copied().find(|status| listed(status));
    if existing == status {
        return Ok(object(json!({ "revision": current["revision"] })));
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
    storage.write(object(json!({ WORD_STATUS_OVERRIDES_KEY: next })))?;
    Ok(object(json!({ "revision": revision })))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Default)]
    struct Memory(RefCell<BTreeMap<String, Value>>);

    impl Storage for Memory {
        fn read(&self, key: &str) -> Option<Value> {
            self.0.borrow().get(key).cloned()
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
