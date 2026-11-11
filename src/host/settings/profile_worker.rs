// SPDX-License-Identifier: GPL-3.0-or-later
//! The settings worker on records with language profiles (docs/languages.md 5
//! upstream): `requestProfileId`, the commits of a schema-2 `dictionaryState`
//! (`profileDictionaryCommit`, `profileViewCommit`, `wholeStateCas`), the options
//! write of a record with profiles, the personal dictionary of each language
//! (`languageCustomCas`), and the engine's one compare-and-set of
//! `hd_profile_add` and `hd_profile_remove` (`profileCas`), as
//! `background-core.js` and `background-requests.js` have them at 0.5.0.

use serde_json::{Map, Value, json};

use super::custom::{self, Identity};
use super::options;
use super::profiles::{self, NO_JAPANESE_PROFILE, PROFILE_REMOVED, Records};
use super::worker::{self, Storage, object};
use crate::host::js::{self, strict_eq};
use crate::host::profile::{JAPANESE, language_code};
use crate::host::registry;
use crate::host::store;

/// An error, with the `code` the extension gives it when it gives one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Coded {
    pub(crate) error: String,
    pub(crate) code: Option<&'static str>,
}

impl From<String> for Coded {
    fn from(error: String) -> Self {
        Self { error, code: None }
    }
}

impl From<&str> for Coded {
    fn from(error: &str) -> Self {
        Self { error: error.to_owned(), code: None }
    }
}

pub(crate) type Result<T> = std::result::Result<T, Coded>;

/// `profileError`.
fn coded(error: impl Into<String>, code: &'static str) -> Coded {
    Coded { error: error.into(), code: Some(code) }
}

fn array(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// `requestProfileId`: the profile a request names, or, from code that predates
/// profiles, the Japanese one. A profile that does not exist is refused, and so
/// is a request naming none when there is no Japanese profile.
pub(crate) fn request_profile_id(message: &Value, options: Option<&Value>) -> Result<String> {
    let named = message.get("profileId");
    if named.is_some_and(|named| named.as_str().is_none_or(str::is_empty)) {
        return Err("the request named an invalid profile".into());
    }
    let id = named.and_then(Value::as_str).unwrap_or(JAPANESE);
    if profiles::profiles(options).iter().any(|profile| profile["id"] == id) {
        return Ok(id.to_owned());
    }
    Err(if named.is_none() { coded(NO_JAPANESE_PROFILE, "no-japanese-profile") } else { coded(PROFILE_REMOVED, "profile-missing") })
}

/// `profileLanguages`: profile id → language, in the profiles' order.
pub(crate) fn profile_languages(options: Option<&Value>) -> Vec<(String, String)> {
    profiles::profiles(options)
        .iter()
        .filter_map(|profile| Some((profile["id"].as_str()?.to_owned(), profile["language"].as_str()?.to_owned())))
        .collect()
}

fn language_of<'a>(languages: &'a [(String, String)], id: &str) -> Option<&'a str> {
    languages.iter().find(|(profile, _)| profile == id).map(|(_, language)| language.as_str())
}

/// `[...new Set(profileLanguages(options).values())]`.
pub(crate) fn distinct_languages(options: Option<&Value>) -> Vec<String> {
    let mut languages: Vec<String> = Vec::new();
    for (_, language) in profile_languages(options) {
        if !languages.contains(&language) {
            languages.push(language);
        }
    }
    languages
}

/// `profileStateOf`: a schema-2 state body as a stored state.
fn state_of(body: &Value, revision: Value) -> Value {
    let mut state = Map::new();
    state.insert("schemaVersion".to_owned(), Value::from(profiles::PROFILES_SCHEMA));
    state.insert("revision".to_owned(), revision);
    for key in ["dictionaries", "profiles"] {
        if let Some(value) = body.get(key) {
            state.insert(key.to_owned(), value.clone());
        }
    }
    Value::Object(state)
}

/// `assertPersonalPlacement`: a personal package is pinned first and enabled in
/// every profile of its language, and appears in no other profile.
fn assert_personal_placement(state: &Value, languages: &[(String, String)]) -> std::result::Result<(), String> {
    for row in array(state.get("dictionaries")) {
        let Some(language) = custom::personal_language(row) else { continue };
        for (id, profile) in state.get("profiles").and_then(Value::as_object).into_iter().flatten() {
            let list = array(profile.get("dictionaries"));
            let index = list.iter().position(|entry| strict_eq(entry.get("id"), row.get("id")));
            if language_of(languages, id) == Some(language.as_str()) {
                if index != Some(0) || list[0].get("enabled") != Some(&Value::Bool(true)) {
                    return Err("the managed custom dictionary must stay enabled and first".to_owned());
                }
            } else if index.is_some() {
                return Err("a personal dictionary belongs only to the profiles of its language".to_owned());
            }
        }
    }
    Ok(())
}

/// The personal packages of a list, by id, the last of a repeated id kept.
fn personal_rows(rows: &[Value]) -> Vec<(String, &Value)> {
    let mut found: Vec<(String, &Value)> = Vec::new();
    for row in rows.iter().filter(|row| custom::personal_language(row).is_some()) {
        let id = row["id"].as_str().unwrap_or_default().to_owned();
        match found.iter_mut().find(|(known, _)| *known == id) {
            Some(entry) => entry.1 = row,
            None => found.push((id, row)),
        }
    }
    found
}

/// `assertOrdinaryPersonalTransition`: an ordinary write of a schema-2 state
/// moves nothing but the personal packages' presentation.
fn assert_ordinary_personal_transition(current: &Value, next: &Value, languages: &[(String, String)]) -> std::result::Result<(), String> {
    let before = personal_rows(array(current.get("dictionaries")));
    let after = personal_rows(array(next.get("dictionaries")));
    let find = |rows: &[(String, &'_ Value)], id: &str| rows.iter().find(|(known, _)| known == id).map(|(_, row)| (*row).clone());
    let mut ids: Vec<&str> = before.iter().map(|(id, _)| id.as_str()).collect();
    ids.extend(after.iter().map(|(id, _)| id.as_str()).filter(|id| !before.iter().any(|(known, _)| known == id)));
    for id in ids {
        let (Some(was), Some(is)) = (find(&before, id), find(&after, id)) else {
            return Err("the managed custom dictionary can only be changed by its source editor".to_owned());
        };
        if !custom::same_engine_state(&was, &is) {
            return Err("the managed custom dictionary must stay enabled and first".to_owned());
        }
    }
    assert_personal_placement(next, languages)
}

/// `assertProfileDictionaryState`: the shape of the whole schema-2 state an
/// engine commit carries.
fn assert_profile_dictionary_state(next: Option<&Value>, options: Option<&Value>) -> std::result::Result<(), String> {
    let Some(rows) = next.and_then(|next| next.get("dictionaries")).and_then(Value::as_array) else {
        return Err("the dictionary state write carried no package list".to_owned());
    };
    let mut ids: Vec<&str> = Vec::new();
    for row in rows {
        match row.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() && !ids.contains(&id) => ids.push(id),
            _ => return Err("the dictionary state write carried an invalid package list".to_owned()),
        }
    }
    let mut expected: Vec<String> = profile_languages(options).into_iter().map(|(id, _)| id).collect();
    expected.sort();
    let profiles = next.and_then(|next| next.get("profiles")).and_then(Value::as_object);
    let matches = profiles.is_some_and(|profiles| {
        let mut keys: Vec<&String> = profiles.keys().collect();
        keys.sort();
        keys.len() == expected.len() && keys.iter().zip(&expected).all(|(key, id)| *key == id)
    });
    let Some(profiles) = profiles.filter(|_| matches) else {
        return Err("the dictionary state write does not match the installation's profiles".to_owned());
    };
    for profile in profiles.values() {
        let invalid = || "the dictionary state write carried an invalid profile list".to_owned();
        let (Some(entries), true) =
            (profile.get("dictionaries").and_then(Value::as_array), profile.get("groups").is_some_and(Value::is_array))
        else {
            return Err(invalid());
        };
        let mut listed: Vec<&str> = Vec::new();
        for entry in entries {
            let id = entry.get("id").and_then(Value::as_str).filter(|id| ids.contains(id));
            match id {
                Some(id) if entry.get("enabled").is_some_and(Value::is_boolean) && !listed.contains(&id) => listed.push(id),
                _ => return Err(invalid()),
            }
        }
    }
    Ok(())
}

/// `profileDictionaryCommit`: each profile's groups pruned against its own
/// packages, each profile's dictionary selections migrated and pruned against
/// its own packages and groups.
pub(crate) fn profile_dictionary_commit(
    current: &Value,
    stored_options: Option<&Value>,
    next: &Value,
) -> std::result::Result<(Value, Map<String, Value>), String> {
    let rows = array(next.get("dictionaries"));
    worker::assert_update_schedules(rows)?;
    let revision = js::number(js::to_number(Some(&worker::state_revision(current))) + 1.0);
    let mut state = json!({ "schemaVersion": profiles::PROFILES_SCHEMA, "revision": revision, "dictionaries": rows, "profiles": {} });
    let mut committed = Map::new();
    for (id, profile) in next.get("profiles").and_then(Value::as_object).into_iter().flatten() {
        let mut single = state.clone();
        single["profiles"] = json!({ id: { "dictionaries": profile.get("dictionaries"), "groups": [] } });
        let members = profiles::project_dictionary_state(Some(&single), id).unwrap_or(Value::Null);
        let groups = worker::prune_groups(profile.get("groups"), array(members.get("dictionaries")))?;
        let mut entry = Map::new();
        if let Some(dictionaries) = profile.get("dictionaries") {
            entry.insert("dictionaries".to_owned(), dictionaries.clone());
        }
        entry.insert("groups".to_owned(), groups);
        committed.insert(id.clone(), Value::Object(entry));
    }
    state["profiles"] = Value::Object(committed);
    let mut values = Map::new();
    values.insert(store::DICTIONARY_STATE.to_owned(), state.clone());
    if profiles::has_profiles(stored_options) {
        let revision = worker::options_revision(stored_options);
        let mut record = profiles::apply_options_patch(stored_options, &Map::new(), JAPANESE)?;
        let mut list = Vec::new();
        for profile in array(record.get("profiles")) {
            let id = profile["id"].as_str().unwrap_or_default();
            let before = profiles::project_dictionary_state(Some(current), id).unwrap_or(Value::Null);
            let after = profiles::project_dictionary_state(Some(&state), id).unwrap_or_else(|| json!({ "dictionaries": [], "groups": [] }));
            let (dictionaries, groups) = (array(after.get("dictionaries")), array(after.get("groups")));
            let migrated = worker::migrate_selections(object(profile["settings"].clone()), &before, dictionaries)?;
            let settings = options::normalise_dictionary_selections(&Value::Object(migrated), dictionaries, groups)?;
            let mut profile = profile.clone();
            profile["settings"] = settings;
            list.push(profile);
        }
        record.insert("profiles".to_owned(), Value::Array(list));
        let mut base = worker::spread(stored_options);
        base.insert("revision".to_owned(), js::number(revision));
        if !js::same_json(&Value::Object(record.clone()), &Value::Object(base)) {
            record.insert("revision".to_owned(), js::number(revision + 1.0));
            values.insert(worker::OPTIONS_KEY.to_owned(), Value::Object(record));
        }
    }
    Ok((state, values))
}

/// What a view commit refused: a profile refusal goes back as a failure, any
/// other error as a protected reply.
pub(crate) enum ViewRefusal {
    Profile(Coded),
    Protected(String),
}

/// `profileViewCommit`: a schema-1 view of one profile merged into the whole
/// state. A view that names no profile comes from code that predates profiles,
/// lists every package and means the Japanese profile.
pub(crate) fn profile_view_commit(
    message: &Value,
    current: &Value,
    stored_options: Option<&Value>,
) -> std::result::Result<(Value, Map<String, Value>), ViewRefusal> {
    let legacy = message.get("profileId").is_none();
    let profile_id = request_profile_id(message, stored_options).map_err(|refused| match refused.code {
        Some(_) => ViewRefusal::Profile(refused),
        None => ViewRefusal::Protected(refused.error),
    })?;
    let groups = message.get("groups").filter(|groups| !groups.is_null()).cloned().unwrap_or_else(|| {
        profiles::project_dictionary_state(Some(current), &profile_id)
            .and_then(|view| view.get("groups").cloned())
            .filter(|groups| !groups.is_null())
            .unwrap_or_else(|| json!([]))
    });
    let view = json!({ "dictionaries": message.get("dictionaries"), "groups": groups });
    let committed = profiles::merge_dictionary_view(current, &profile_id, &view, legacy).and_then(|next| {
        assert_ordinary_personal_transition(current, &next, &profile_languages(stored_options))?;
        profile_dictionary_commit(current, stored_options, &next)
    });
    committed.map_err(ViewRefusal::Protected)
}

/// `wholeStateCas`: the engine's commit of a whole schema-2 record (import,
/// reimport, managed update, removal, reconciliation).
pub(crate) fn whole_state_cas(storage: &impl Storage, message: &Value) -> Result<Map<String, Value>> {
    let current = worker::stored_state(storage);
    let stored_options = storage.read(worker::OPTIONS_KEY);
    worker::assert_state(&current)?;
    if !strict_eq(message.get("baseRevision"), Some(&worker::state_revision(&current))) {
        return Ok(object(json!({
            "ok": false, "conflict": true, "error": "the dictionary state changed while it was being written", "state": current,
        })));
    }
    if !profiles::has_profiles(stored_options.as_ref()) || !profiles::has_dictionary_profiles(Some(&current)) {
        return Err("the dictionary state write carried profiles this installation does not have".into());
    }
    let body = message.get("state");
    assert_profile_dictionary_state(body, stored_options.as_ref())?;
    let body = body.cloned().unwrap_or(Value::Null);
    let languages = profile_languages(stored_options.as_ref());
    if let Err(error) = assert_ordinary_personal_transition(&current, &state_of(&body, current["revision"].clone()), &languages) {
        return Ok(object(json!({ "ok": false, "protected": true, "error": error, "state": current })));
    }
    let (state, values) = profile_dictionary_commit(&current, stored_options.as_ref(), &body)?;
    storage.write(values)?;
    Ok(object(json!({ "state": state })))
}

/// `profileOptionsWriteResult`: global keys to the top level, profile keys and
/// pack settings to the profile the request names, which is then pruned
/// against its own packages and groups.
pub(crate) fn options_write(
    message: &Value,
    patch: &Map<String, Value>,
    state: &Value,
    stored: Option<&Value>,
) -> Result<Map<String, Value>> {
    let revision = worker::options_revision(stored);
    if !strict_eq(message.get("baseRevision"), Some(&js::number(revision))) {
        let current = profiles::apply_options_patch(stored, &Map::new(), JAPANESE)?;
        return Ok(object(json!({
            "ok": false, "conflict": true,
            "error": "Settings changed in another page. Review your changes before saving again.", "options": current,
        })));
    }
    let global_only = patch.keys().all(|key| profiles::GLOBAL_OPTION_KEYS.contains(&key.as_str())) && message.get("packSettings").is_none();
    let profile_id = if global_only { None } else { Some(request_profile_id(message, stored)?) };
    let target = profile_id.as_deref().unwrap_or(JAPANESE);
    let mut next = Value::Object(profiles::apply_options_patch(stored, patch, target)?);
    if let Some(pack_settings) = message.get("packSettings") {
        next = profiles::apply_pack_settings_patch(Some(&next), Some(pack_settings), target)?;
    }
    if let Some(id) = profile_id.as_deref()
        && !state.is_null()
        && let Some(view) = profiles::project_dictionary_state(Some(state), id)
    {
        let (dictionaries, groups) = (array(view.get("dictionaries")), array(view.get("groups")));
        let mut list = Vec::new();
        for profile in array(next.get("profiles")) {
            let mut profile = profile.clone();
            if profile["id"] == id {
                profile["settings"] = options::normalise_dictionary_selections(&profile["settings"], dictionaries, groups)?;
            }
            list.push(profile);
        }
        next["profiles"] = Value::Array(list);
    }
    let mut base = worker::spread(stored);
    base.insert("revision".to_owned(), js::number(revision));
    if !js::same_json(&next, &Value::Object(base)) {
        next["revision"] = js::number(js::to_number(next.get("revision")) + 1.0);
    }
    Ok(object(json!({ "options": next })))
}

/// `customRequestLanguage`: the engine names the language, a page names its
/// profile, and a request that names neither means Japanese.
pub(crate) fn custom_request_language(message: &Value, stored_options: Option<&Value>) -> Result<String> {
    if let Some(language) = message.get("language") {
        return match language.as_str().filter(|code| language_code(code)) {
            Some(code) => Ok(code.to_owned()),
            None => Err("the custom dictionary request named an invalid language".into()),
        };
    }
    if message.get("profileId").is_some() {
        let id = request_profile_id(message, stored_options)?;
        return Ok(language_of(&profile_languages(stored_options), &id).unwrap_or(JAPANESE).to_owned());
    }
    Ok(JAPANESE.to_owned())
}

/// `languageCustomRequest`: records with profiles, another language, or the
/// engine's whole record.
pub(crate) fn language_custom_request(message: &Value, stored_options: Option<&Value>) -> bool {
    profiles::has_profiles(stored_options)
        || message.get("state").is_some()
        || ["language", "profileId"].iter().any(|field| message.get(*field).is_some_and(|value| value != JAPANESE))
}

/// The language a personal-dictionary request is for, its profiles' languages
/// and its personal package.
struct Target {
    language: String,
    languages: Vec<(String, String)>,
    identity: Identity,
}

/// `customProfileLanguage`.
fn custom_profile_language(message: &Value, stored_options: Option<&Value>) -> Result<Target> {
    if !profiles::has_profiles(stored_options) {
        return Err(crate::host::profile::NO_LANGUAGE_PROFILE.into());
    }
    let language = custom_request_language(message, stored_options)?;
    let languages = profile_languages(stored_options);
    if !languages.iter().any(|(_, held)| *held == language) {
        return Err(if language == JAPANESE {
            coded(NO_JAPANESE_PROFILE, "no-japanese-profile")
        } else {
            coded(PROFILE_REMOVED, "profile-missing")
        });
    }
    let identity = custom::identity(&language).ok_or("This Hachidori has no personal dictionary for this language.")?;
    Ok(Target { language, languages, identity })
}

/// `assertOtherPersonalPackagesKept`: only this language's personal package may change.
fn assert_other_personal_packages_kept(current: &Value, next: &Value, language: &str) -> std::result::Result<(), String> {
    let (before, after) = (array(current.get("dictionaries")), array(next.get("dictionaries")));
    for row in before.iter().chain(after) {
        let Some(owner) = custom::personal_language(row) else { continue };
        if owner == language {
            continue;
        }
        let find = |rows: &[Value]| rows.iter().find(|entry| strict_eq(entry.get("id"), row.get("id"))).cloned();
        match (find(before), find(after)) {
            (Some(was), Some(is)) if js::same_json(&was, &is) => {}
            _ => return Err("the managed custom dictionary can only be changed by its source editor".to_owned()),
        }
    }
    Ok(())
}

/// `Cannot read properties of null (reading 'key')`, as a property read of a
/// missing record throws it.
fn null_read(value: &Value, key: &str) -> String {
    js::null_property(Some(value).filter(|value| !value.is_null()), key)
}

/// `assertPersonalPackageState`: the language's personal package matches its
/// source in every profile of the language.
fn assert_personal_package_state(next: &Value, entry_count: usize, revision: &str, target: &Target) -> std::result::Result<(), String> {
    if next.is_null() {
        return Err(null_read(next, "dictionaries"));
    }
    let rows = array(next.get("dictionaries")).iter().filter(|row| row["id"] == target.identity.id.as_str()).count();
    if if entry_count == 0 { rows != 0 } else { rows != 1 } {
        return Err("the custom dictionary package does not match its source semantics".to_owned());
    }
    for (profile, language) in &target.languages {
        if *language != target.language {
            continue;
        }
        let view = profiles::project_dictionary_state(Some(next), profile)
            .ok_or_else(|| "Cannot read properties of undefined (reading 'dictionaries')".to_owned())?;
        custom::assert_source_state(array(view.get("dictionaries")), revision, entry_count, &target.identity)?;
    }
    Ok(())
}

/// `languageCustomCas`: the language's document, its personal package checked
/// in every profile of the language, the other languages' documents and
/// packages kept.
pub(crate) fn language_custom_cas(storage: &impl Storage, message: &Value) -> Result<Map<String, Value>> {
    let current = worker::stored_state(storage);
    let stored_options = storage.read(worker::OPTIONS_KEY);
    let stored_document = storage.read(custom::CUSTOM_DICTIONARY_SOURCE_KEY);
    let target = custom_profile_language(message, stored_options.as_ref())?;
    let document = custom::document_for(stored_document.as_ref(), &target.language)?;
    if !strict_eq(message.get("baseDocumentRevision"), document.get("revision")) {
        return Ok(object(json!({
            "ok": false, "stale": true, "error": "the custom dictionary source changed while it was being saved",
            "document": document, "state": current,
        })));
    }
    if !strict_eq(message.get("baseRevision"), Some(&worker::state_revision(&current))) {
        return Ok(object(json!({
            "ok": false, "conflict": true, "error": "the dictionary state changed while the custom dictionary was being saved",
            "document": document, "state": current,
        })));
    }
    let text = message["text"].as_str().unwrap_or_default();
    let semantic = message["semanticRevision"].as_str().unwrap_or_default();
    let (entries, _) = custom::parse(text, target.identity.reading_required);
    let revision = custom::semantic_revision(&entries, target.identity.reading_required)?;
    if revision != semantic {
        return Err("the custom dictionary semantic revision does not match its source".into());
    }
    let body = message.get("state");
    let next = match body {
        None => current.clone(),
        Some(_) if current.is_null() => return Err(null_read(&current, "revision").into()),
        Some(body) => state_of(body, current["revision"].clone()),
    };
    if body.is_some() {
        assert_profile_dictionary_state(body, stored_options.as_ref())?;
        assert_other_personal_packages_kept(&current, &next, &target.language)?;
        assert_personal_placement(&next, &target.languages)?;
    }
    assert_personal_package_state(&next, entries.len(), &revision, &target)?;
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
        let record = custom::with_document(stored_document.as_ref(), &target.language, &next_document)?;
        values.insert(custom::CUSTOM_DICTIONARY_SOURCE_KEY.to_owned(), record);
    }
    if let Some(body) = body {
        let mut held = Map::new();
        for key in ["dictionaries", "profiles"] {
            if let Some(value) = current.get(key) {
                held.insert(key.to_owned(), value.clone());
            }
        }
        if !js::same_json(&Value::Object(held), body) {
            let (committed, commit) = profile_dictionary_commit(&current, stored_options.as_ref(), body)?;
            state = committed;
            values.extend(commit);
        }
    }
    if !values.is_empty() {
        storage.write(values)?;
    }
    Ok(object(json!({ "document": next_document, "state": state })))
}

// ---------------------------------------------------------------------------
// hd_profile_cas

/// The language a profile request is for: a removal's profile names it.
fn profile_cas_language(message: &Value, stored_options: Option<&Value>) -> Result<String> {
    let languages = profile_languages(stored_options);
    let named = message.get("profileId").and_then(Value::as_str).and_then(|id| language_of(&languages, id));
    let language = match named {
        Some(named) if message.get("action").and_then(Value::as_str) == Some("remove") => Some(named),
        _ => message.get("language").and_then(Value::as_str),
    };
    language.filter(|code| language_code(code)).map(str::to_owned).ok_or_else(|| "the language request named no language".into())
}

/// `addedProfileOptions`: a profile added after the ones there are, copying the
/// settings of `copyFrom`.
fn added_profile_options(message: &Value, stored_options: Option<&Value>, language: &str) -> Result<Value> {
    let Some(descriptor) = registry::get(language) else {
        return Err(coded(format!("This Hachidori has no pack for {language}."), "language-unknown"));
    };
    let mut record = profiles::options_with_profiles(stored_options);
    let list = array(record.get("profiles"));
    if list.iter().any(|profile| profile["language"] == language) {
        let name = descriptor["name"].as_str().unwrap_or(language);
        return Err(coded(format!("{name} is already one of your languages."), "language-present"));
    }
    let source_id = message.get("copyFrom").filter(|id| !id.is_null()).cloned().unwrap_or_else(|| Value::from(JAPANESE));
    let Some(source) = list.iter().find(|profile| profile["id"] == source_id) else {
        return Err(coded("The language to copy settings from was removed.", "profile-missing"));
    };
    let new_id = || worker::random_uuid().unwrap_or_else(|_| format!("{:x}", js::now().to_bits()));
    let settings = profiles::new_profile_settings(source.get("settings"), Some(descriptor), new_id);
    let added = json!({
        "id": language, "language": language, "settings": settings, "packSettings": profiles::pack_setting_defaults(Some(descriptor)),
    });
    let mut list = list.to_vec();
    list.push(added);
    record["profiles"] = Value::Array(list);
    Ok(record)
}

/// `removedProfileOptions`: a profile removed; with no profile named, the
/// options an erase of a held language without a profile leaves.
fn removed_profile_options(message: &Value, stored_options: Option<&Value>, language: &str) -> Result<Value> {
    if !profiles::has_profiles(stored_options) {
        return Err("The last language cannot be removed.".into());
    }
    let mut record = profiles::apply_options_patch(stored_options, &Map::new(), JAPANESE)?;
    let list = array(record.get("profiles")).to_vec();
    let Some(profile_id) = message.get("profileId") else {
        if list.iter().any(|profile| profile["language"] == language) {
            return Err("Remove the language before erasing it.".into());
        }
        return Ok(Value::Object(record));
    };
    if !list.iter().any(|profile| profile.get("id") == Some(profile_id)) {
        return Err(coded(PROFILE_REMOVED, "profile-missing"));
    }
    if list.len() == 1 {
        return Err("The last language cannot be removed.".into());
    }
    let kept: Vec<Value> = list.into_iter().filter(|profile| profile.get("id") != Some(profile_id)).collect();
    record.insert("profiles".to_owned(), Value::Array(kept));
    Ok(Value::Object(record))
}

/// A BCP 47-like tag as the engine reads `targetLanguage` from an index.
fn target_language_tag(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let primary = parts.next().unwrap_or_default();
    (2..=8).contains(&primary.len())
        && primary.bytes().all(|b| b.is_ascii_alphabetic())
        && parts.all(|part| (1..=8).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// `assertPackagesKept` (with `convertingComparable`): a language request
/// changes no package; an erase removes the language's personal package and
/// nothing else. The request that first gives the records profiles may add each
/// package's `targetLanguage`.
fn assert_packages_kept(current: &Value, state: &Value, erased: Option<&str>) -> std::result::Result<(), String> {
    let converting = !profiles::has_dictionary_profiles(Some(current));
    let comparable = |row: &Value| -> Value {
        let mut fields = match row.as_object() {
            Some(fields) if converting && fields.contains_key("targetLanguage") => fields.clone(),
            _ => return row.clone(),
        };
        if fields.get("targetLanguage").and_then(Value::as_str).is_some_and(target_language_tag) {
            fields.remove("targetLanguage");
            Value::Object(fields)
        } else {
            row.clone()
        }
    };
    let with_profiles = profiles::dictionary_state_with_profiles(Some(current).filter(|current| !current.is_null()));
    let mut before: Vec<(Value, Value)> = Vec::new();
    for row in array(with_profiles.get("dictionaries")) {
        let id = row.get("id").cloned().unwrap_or(Value::Null);
        let value = comparable(row);
        match before.iter_mut().find(|(known, _)| *known == id) {
            Some(entry) => entry.1 = value,
            None => before.push((id, value)),
        }
    }
    let rows = array(state.get("dictionaries"));
    for row in rows {
        let was = before.iter().find(|(id, _)| Some(id) == row.get("id")).map(|(_, value)| value);
        if !was.is_some_and(|was| js::same_json(was, &comparable(row))) {
            return Err("adding or removing a language changes no dictionary".to_owned());
        }
    }
    for (id, _) in &before {
        if !rows.iter().any(|row| row.get("id") == Some(id)) && id.as_str() != erased {
            return Err("adding or removing a language removes no dictionary".to_owned());
        }
    }
    Ok(())
}

/// `withoutLookupStatsLanguage`: the descriptor without `language`'s counts; for
/// Japanese, a fresh generation.
fn without_lookup_stats_language(descriptor: &Value, language: &str) -> std::result::Result<Value, String> {
    if language == JAPANESE {
        return worker::with_lookup_stats_generation(descriptor, JAPANESE, &worker::random_uuid()?);
    }
    let mut languages = descriptor.get("languages").and_then(Value::as_object).cloned().unwrap_or_default();
    languages.remove(language);
    let mut next = Map::new();
    next.insert("generation".to_owned(), descriptor.get("generation").cloned().unwrap_or(Value::Null));
    next.insert("revision".to_owned(), js::number(js::to_number(descriptor.get("revision")) + 1.0));
    if !languages.is_empty() {
        next.insert("languages".to_owned(), Value::Object(languages));
    }
    let next = Value::Object(next);
    worker::assert_descriptor(&next)?;
    Ok(next)
}

/// The records an erase rewrites.
struct Erased {
    document: Option<Value>,
    lookup_stats: Value,
    marks: Option<Value>,
}

/// `eraseLanguageRecords`: the language's personal document emptied, its counts
/// and marks dropped; a document that changed after the confirmation is kept
/// (the stale reply).
fn erase_language_records(
    records: &mut Erased,
    message: &Value,
    language: &str,
    state: &Value,
    personal: Option<&Identity>,
    current: &Value,
) -> std::result::Result<Option<Map<String, Value>>, String> {
    let document = custom::document_for(records.document.as_ref(), language)?;
    if !strict_eq(message.get("baseDocumentRevision"), document.get("revision")) {
        return Ok(Some(object(json!({
            "ok": false, "stale": true,
            "error": "The personal dictionary changed after you confirmed, so it was kept. Review it and try again.",
            "document": document, "state": current,
        }))));
    }
    if let Some(personal) = personal
        && array(state.get("dictionaries")).iter().any(|row| row["id"] == personal.id.as_str())
    {
        return Err("an erase removes the personal dictionary".to_owned());
    }
    let emptied = json!({
        "schemaVersion": 1, "revision": js::number(js::to_number(document.get("revision")) + 1.0),
        "semanticRevision": custom::EMPTY_SEMANTIC_REVISION, "text": "",
    });
    records.document = Some(custom::with_document(records.document.as_ref(), language, &emptied)?);
    worker::assert_descriptor(&records.lookup_stats)?;
    records.lookup_stats = without_lookup_stats_language(&records.lookup_stats, language)?;
    let revision = js::number(js::to_number(worker::normalise_overrides(records.marks.as_ref()).get("revision")) + 1.0);
    let cleared = object(json!({ "revision": revision, "known": [], "ignored": [] }));
    records.marks = Some(worker::with_language_overrides(records.marks.as_ref(), language, cleared));
    Ok(None)
}

/// `selectionAfterRemoval`: removing the active profile moves lookups to
/// Japanese, else to the first profile left, in the same write.
fn selection_after_removal(selection: Option<&Value>, profile_id: Option<&Value>, options: &Value) -> Option<Value> {
    let current = profiles::normalise_language_selection(selection);
    let (Some(_), Some(profile_id)) = (selection, profile_id) else { return None };
    if !strict_eq(current.get("activeProfileId"), Some(profile_id)) {
        return None;
    }
    let left = profiles::profiles(Some(options));
    let next =
        left.iter().find(|profile| profile["id"] == JAPANESE).or_else(|| left.first()).map_or(Value::Null, |profile| profile["id"].clone());
    let mut selection = current;
    selection["revision"] = js::number(js::to_number(selection.get("revision")) + 1.0);
    selection["activeProfileId"] = next;
    Some(selection)
}

/// `hd_profile_cas` (`profileCas`): options and `dictionaryState` in one
/// compare-and-set against the dictionary state the engine read (and, for an
/// erase, the language's personal document). When the result is the
/// single-profile shape, the records convert back and `languageSelection` goes.
pub(crate) fn profile_cas(storage: &impl Storage, message: &Value) -> Result<Map<String, Value>> {
    let current = worker::stored_state(storage);
    let stored_options = storage.read(worker::OPTIONS_KEY);
    let selection = storage.read(profiles::LANGUAGE_SELECTION_KEY);
    worker::assert_state(&current)?;
    if js::whole(message.get("baseRevision")).is_none() {
        return Err("the language request carried no valid base revision".into());
    }
    if !strict_eq(message.get("baseRevision"), Some(&worker::state_revision(&current))) {
        return Ok(object(json!({
            "ok": false, "conflict": true, "error": "the dictionary state changed while the language was being changed", "state": current,
        })));
    }
    let language = profile_cas_language(message, stored_options.as_ref())?;
    let options = match message.get("action").and_then(Value::as_str) {
        Some("add") => added_profile_options(message, stored_options.as_ref(), &language)?,
        Some("remove") => removed_profile_options(message, stored_options.as_ref(), &language)?,
        _ => return Err("unknown profile request".into()),
    };
    let languages = profile_languages(Some(&options));
    assert_profile_dictionary_state(message.get("state"), Some(&options))?;
    let revision = js::number(js::to_number(Some(&worker::state_revision(&current))) + 1.0);
    let state = state_of(message.get("state").unwrap_or(&Value::Null), revision);
    assert_personal_placement(&state, &languages)?;
    let personal = custom::identity(&language);
    let erase = message.get("erase") == Some(&Value::Bool(true));
    assert_packages_kept(&current, &state, if erase { personal.as_ref().map(|identity| identity.id.as_str()) } else { None })?;
    let mut records = Erased {
        document: storage.read(custom::CUSTOM_DICTIONARY_SOURCE_KEY),
        lookup_stats: storage.read(worker::LOOKUP_STATS_KEY).unwrap_or_else(|| json!({ "generation": null, "revision": 0 })),
        marks: storage.read(worker::WORD_STATUS_OVERRIDES_KEY),
    };
    let mut values = Map::new();
    if erase {
        if let Some(stale) = erase_language_records(&mut records, message, &language, &state, personal.as_ref(), &current)? {
            return Ok(stale);
        }
        values.insert(custom::CUSTOM_DICTIONARY_SOURCE_KEY.to_owned(), records.document.clone().unwrap_or(Value::Null));
        values.insert(worker::LOOKUP_STATS_KEY.to_owned(), records.lookup_stats.clone());
        values.insert(worker::WORD_STATUS_OVERRIDES_KEY.to_owned(), records.marks.clone().unwrap_or(Value::Null));
    }
    let converted = profiles::single_profile_shape(Records {
        options: Some(&options),
        state: Some(&state),
        document: records.document.as_ref(),
        lookup_stats: Some(&records.lookup_stats),
        word_status_overrides: records.marks.as_ref(),
    });
    let mut written = if converted { profiles::options_without_profiles(Some(&options)) } else { options.clone() };
    written["revision"] = js::number(worker::options_revision(stored_options.as_ref()) + 1.0);
    values.insert(worker::OPTIONS_KEY.to_owned(), written);
    let committed = if converted { profiles::japanese_dictionary_view(Some(&state)).unwrap_or(Value::Null) } else { state };
    values.insert(store::DICTIONARY_STATE.to_owned(), committed.clone());
    if !converted
        && message.get("action").and_then(Value::as_str) == Some("remove")
        && let Some(next) = selection_after_removal(selection.as_ref(), message.get("profileId"), &options)
    {
        values.insert(profiles::LANGUAGE_SELECTION_KEY.to_owned(), next);
    }
    if converted {
        // `convertedBackRemovals`: the single-profile shape has no `languages`
        // maps; the top document stays unless it is empty.
        if let Some(document) = records.document.as_ref().filter(|document| !document.is_null()) {
            let top = custom::normalise_document(Some(document))?;
            let empty = top["revision"].as_f64() == Some(0.0) && top["text"] == "";
            values.insert(custom::CUSTOM_DICTIONARY_SOURCE_KEY.to_owned(), if empty { Value::Null } else { top });
        }
        if selection.is_some() {
            values.insert(profiles::LANGUAGE_SELECTION_KEY.to_owned(), Value::Null);
        }
    }
    storage.write(values)?;
    if erase {
        // `removeErasedLookupRows`: the erased language's rows; a failure leaves
        // them unreachable.
        let _ = worker::remove_obsolete_lookup_rows(storage);
    }
    let mut reply = object(json!({ "state": committed, "converted": converted }));
    if !converted {
        reply.insert("languages".to_owned(), Value::from(distinct_languages(Some(&options))));
    }
    Ok(reply)
}

/// `PROFILE_LOOKUP_TYPES`: the lookups the worker names a profile's fields on.
const PROFILE_LOOKUP_TYPES: [&str; 4] = ["hd_lookup", "hd_lookup_dictionary", "hd_segment", "hd_kanji"];
/// `PROFILE_LANGUAGE_TYPES`: personal-dictionary saves, for the language of the
/// profile they name.
const PROFILE_LANGUAGE_TYPES: [&str; 2] = ["hd_custom_save", "hd_custom_append"];

/// `languageOptionsFor`: the ids of a pack's optional preprocessors, transforms
/// and variant passes that the profile's boolean pack settings turn on.
pub(crate) fn language_options_for(language: &str, pack_settings: &Map<String, Value>) -> Vec<Value> {
    let descriptor = registry::get(language);
    let settings = array(descriptor.and_then(|descriptor| descriptor.get("settings")));
    settings
        .iter()
        .flat_map(|setting| {
            let effect = setting.get("effect");
            let field = |key: &str| effect.and_then(|effect| effect.get(key)).filter(|value| !value.is_null());
            if setting.get("type").and_then(Value::as_str) != Some("boolean")
                || profiles::pack_setting_value(descriptor, pack_settings, setting.get("id").and_then(Value::as_str).unwrap_or_default())
                    != Some(Value::Bool(true))
            {
                return Vec::new();
            }
            match field("preprocessor").or_else(|| field("variantPass")) {
                Some(id @ Value::String(_)) => vec![id.clone()],
                Some(_) => Vec::new(),
                None => array(field("transforms")).iter().filter(|item| item.is_string()).cloned().collect(),
            }
        })
        .collect()
}

/// A lookup profile of `setLookupProfiles`: its language, enabled package ids
/// and pack-setting effects.
fn lookup_profile(state: Option<&Value>, profile: &Value) -> (Value, Value, Value) {
    let id = profile.get("id").and_then(Value::as_str).unwrap_or_default();
    let language = profile.get("language").and_then(Value::as_str).unwrap_or_default();
    let view = profiles::project_dictionary_state(state, id);
    let dictionaries: Vec<Value> = array(view.as_ref().and_then(|view| view.get("dictionaries")))
        .iter()
        .filter(|row| row.get("enabled") != Some(&Value::Bool(false)))
        .map(|row| row.get("id").cloned().unwrap_or(Value::Null))
        .collect();
    let pack_settings = profile.get("packSettings").and_then(Value::as_object).cloned().unwrap_or_default();
    (Value::from(language), Value::Array(dictionaries), Value::Array(language_options_for(language, &pack_settings)))
}

/// `resolveLookupProfile`: the worker sets the profile's language, dictionaries
/// and language options on every lookup, replacing any a forwarded request
/// carried. A request that names no profile means the Japanese data; without a
/// Japanese profile it selects no dictionaries and finds nothing.
fn resolve_lookup_profile(mut message: Map<String, Value>, options: Option<&Value>, state: Option<&Value>) -> Result<Map<String, Value>> {
    let profile_id = message.remove("profileId");
    for key in ["language", "dictionaries", "languageOptions"] {
        message.remove(key);
    }
    if !profiles::has_profiles(options) {
        // The single-profile shape: its only profile is Japanese.
        if profile_id.as_ref().and_then(Value::as_str) != Some(JAPANESE) {
            return Err(coded(PROFILE_REMOVED, "profile-missing"));
        }
        return Ok(message);
    }
    let all = profiles::profiles(options);
    let find = |id: &str| all.iter().find(|profile| profile.get("id").and_then(Value::as_str) == Some(id));
    let (language, dictionaries, language_options) = match &profile_id {
        Some(id) => {
            let Some(profile) = id.as_str().and_then(find) else { return Err(coded(PROFILE_REMOVED, "profile-missing")) };
            message.insert("profileId".to_owned(), id.clone());
            lookup_profile(state, profile)
        }
        None => find(JAPANESE).map_or_else(|| (Value::from(JAPANESE), json!([]), json!([])), |profile| lookup_profile(state, profile)),
    };
    message.insert("language".to_owned(), language);
    message.insert("dictionaries".to_owned(), dictionaries);
    message.insert("languageOptions".to_owned(), language_options);
    Ok(message)
}

/// `resolveRequestLanguage`: the worker names the language, replacing any a
/// forwarded request carried.
fn resolve_request_language(mut message: Map<String, Value>, options: Option<&Value>) -> Result<Map<String, Value>> {
    message.remove("language");
    if !message.contains_key("profileId") {
        return Ok(message);
    }
    let id = request_profile_id(&Value::Object(message.clone()), options)?;
    let language = profile_languages(options).into_iter().find(|(profile, _)| *profile == id).map(|(_, language)| language);
    if let Some(language) = language {
        message.insert("language".to_owned(), Value::from(language));
    }
    Ok(message)
}

/// `relay`'s `withProfileFields`: an engine request of a profile carries its
/// fields before it reaches the engine (docs/languages.md 9.3); `None` when the
/// request is not one.
pub(crate) fn with_profile_fields(message: &Value, options: Option<&Value>, state: Option<&Value>) -> Option<Result<Value>> {
    if message.get("target").and_then(Value::as_str) != Some("hoshidicts-offscreen") {
        return None;
    }
    let kind = message.get("type").and_then(Value::as_str).unwrap_or_default();
    let lookup = PROFILE_LOOKUP_TYPES.contains(&kind) && (profiles::has_profiles(options) || message.get("profileId").is_some());
    let language = PROFILE_LANGUAGE_TYPES.contains(&kind) && (message.get("profileId").is_some() || message.get("language").is_some());
    if !lookup && !language {
        return None;
    }
    let message = object(message.clone());
    let resolved = if lookup { resolve_lookup_profile(message, options, state) } else { Ok(message) };
    Some(resolved.and_then(|message| if language { resolve_request_language(message, options) } else { Ok(message) }).map(Value::Object))
}

/// An engine request the host's own code builds (Anki's sentence segments,
/// the API's lookups), with its profile's fields as the relay to the offscreen
/// document adds them to every request it forwards.
pub(crate) fn engine_request(message: &Value, options: Option<&Value>, state: Option<&Value>) -> Result<Value> {
    let mut message = message.clone();
    if let Some(fields) = message.as_object_mut() {
        fields.insert("target".to_owned(), Value::from("hoshidicts-offscreen"));
    }
    with_profile_fields(&message, options, state).unwrap_or(Ok(message))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::worker::tests::Memory;
    use super::super::worker::{Sender, handle_from};
    use super::*;

    const PERSONAL: &str = custom::CUSTOM_DICTIONARY_ID;

    fn with_records(records: Value) -> Memory {
        let storage = Memory::default();
        storage.write(object(records)).unwrap();
        storage
    }

    fn page(storage: &Memory, fields: Value) -> Value {
        let mut message = fields;
        message["target"] = json!("hoshidicts-worker");
        handle_from(storage, &message, &[], Sender::Linked)
    }

    fn engine(storage: &Memory, fields: Value) -> Value {
        let mut message = fields;
        message["target"] = json!("hoshidicts-worker");
        handle_from(storage, &message, &[], Sender::Engine)
    }

    fn stored(storage: &Memory, key: &str) -> Value {
        storage.read(key).unwrap_or(Value::Null)
    }

    // The records of profiles.test.mjs, against the registry the relay ships.
    fn with_profiles() -> Value {
        json!({
            "revision": 9, "hoverEnabled": false, "popupWidthPx": 640, "keybinds": [],
            "anki": { "url": "http://127.0.0.1:8766", "apiKey": "secret" },
            "profiles": [
                { "id": "ja", "language": "ja", "settings": { "scanLength": 12, "popupTheme": "jl", "showPitchAccentGraph": true,
                    "anki": { "templates": [{ "id": "jp", "name": "Japanese", "deck": "JP", "model": "Lapis" }] } }, "packSettings": {} },
                { "id": "ar", "language": "ar", "settings": { "scanLength": 24, "showPitchAccentGraph": true, "wordHighlightEnabled": true,
                    "kanjiClickDictionary": "Some kanji", "pitchAccentFuriganaDictionary": "Pitch",
                    "anki": { "templates": [{ "id": "arabic", "name": "Arabic", "deck": "AR" }] } }, "packSettings": { "vowelMarks": "hide" } },
            ],
        })
    }

    fn schema2() -> Value {
        json!({
            "schemaVersion": 2, "revision": 31,
            "dictionaries": [
                { "id": PERSONAL, "title": "Hachidori Custom Dictionary", "displayName": null, "path": "/dicts/a", "favorite": false, "language": "ja" },
                { "id": "jitendex", "title": "Jitendex", "displayName": null, "path": "/dicts/b", "favorite": true, "language": "ja" },
                { "id": "arabic", "title": "Arabic", "displayName": "AR", "path": "/dicts/c", "favorite": false, "language": "ar" },
                { "id": "spare", "title": "Spare", "displayName": null, "path": "/dicts/d", "favorite": false, "language": "en" },
            ],
            "profiles": {
                "ja": { "dictionaries": [{ "id": PERSONAL, "enabled": true }, { "id": "jitendex", "enabled": false }], "groups": [] },
                "ar": { "dictionaries": [{ "id": "arabic", "enabled": true }, { "id": "jitendex", "enabled": true }],
                    "groups": [{ "id": "g1", "name": "Main", "dictionaryIds": ["arabic"] }] },
            },
        })
    }

    pub(crate) fn profile_records() -> Value {
        let mut state = schema2();
        state["profiles"]["ar"]["dictionaries"].as_array_mut().unwrap().push(json!({ "id": "spare", "enabled": false }));
        json!({ "options": with_profiles(), "dictionaryState": state })
    }

    fn with_english(records: &mut Value) {
        records["options"]["profiles"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "id": "en", "language": "en", "settings": {}, "packSettings": {} }));
        records["dictionaryState"]["profiles"]["en"] = json!({ "dictionaries": [], "groups": [] });
    }

    fn single_profile_records() -> Value {
        json!({
            "options": { "revision": 4, "scanLength": 20, "popupTheme": "jl", "hoverEnabled": false, "frequencyDictionary": "Jitendex",
                "anki": { "url": "http://127.0.0.1:8765", "apiKey": "",
                    "templates": [{ "id": "default", "name": "Default", "deck": "JP", "model": "Lapis" }] } },
            "dictionaryState": { "schemaVersion": 1, "revision": 6, "groups": [{ "id": "g", "name": "G", "dictionaryIds": ["jitendex"] }],
                "dictionaries": [
                    { "id": "jitendex", "title": "Jitendex", "displayName": null, "path": "/dicts/a", "enabled": true, "favorite": false,
                        "language": "ja", "frequencyCount": 1 },
                    { "id": "other", "title": "Other", "displayName": null, "path": "/dicts/b", "enabled": false, "favorite": false, "language": "ja" },
                ] },
        })
    }

    fn ok(reply: &Value) {
        assert_eq!(reply["ok"], true, "{reply}");
    }

    #[test]
    fn the_worker_writes_options_to_the_profile_a_request_names() {
        let storage = with_records(profile_records());
        let reply = page(
            &storage,
            json!({ "type": "hd_options_write", "baseRevision": 9, "profileId": "ar",
            "options": { "scanLength": 30, "hoverEnabled": true } }),
        );
        ok(&reply);
        let options = stored(&storage, "options");
        assert_eq!(options["revision"], 10);
        assert_eq!(options["hoverEnabled"], true);
        assert_eq!(options["profiles"][1]["settings"]["scanLength"], 30);
        assert_eq!(options["profiles"][0]["settings"]["scanLength"], 12, "Japanese keeps its own scan length");
        // A request that names no profile means the Japanese data (section 5.2).
        ok(&page(&storage, json!({ "type": "hd_options_write", "baseRevision": 10, "options": { "scanLength": 9 } })));
        assert_eq!(stored(&storage, "options")["profiles"][0]["settings"]["scanLength"], 9);
        let reply =
            page(&storage, json!({ "type": "hd_options_write", "baseRevision": 11, "profileId": "en", "options": { "scanLength": 9 } }));
        assert_eq!((reply["ok"].clone(), reply["errorCode"].clone()), (json!(false), json!("profile-missing")));
        ok(&page(
            &storage,
            json!({ "type": "hd_options_write", "baseRevision": 11, "profileId": "ar", "options": {},
            "packSettings": { "vowelMarks": "hide" } }),
        ));
        assert_eq!(stored(&storage, "options")["profiles"][1]["packSettings"], json!({ "vowelMarks": "hide" }));
        let reply = page(
            &storage,
            json!({ "type": "hd_options_write", "baseRevision": 11, "profileId": "ar", "options": {},
            "packSettings": { "vowelMarks": "maybe" } }),
        );
        assert_eq!(reply["ok"], false);
        assert!(reply["error"].as_str().unwrap().contains("invalid value"), "{reply}");
        assert_eq!(stored(&storage, "options")["revision"], 11, "an unchanged pack setting writes nothing");
    }

    #[test]
    fn a_request_naming_no_profile_is_refused_without_japanese_unless_it_writes_global_keys() {
        let mut records = profile_records();
        records["options"]["profiles"].as_array_mut().unwrap().retain(|profile| profile["id"] != "ja");
        records["dictionaryState"]["profiles"].as_object_mut().unwrap().remove("ja");
        let storage = with_records(records);
        let reply = page(&storage, json!({ "type": "hd_options_write", "baseRevision": 9, "options": { "scanLength": 3 } }));
        assert_eq!(reply["errorCode"], "no-japanese-profile");
        ok(&page(&storage, json!({ "type": "hd_options_write", "baseRevision": 9, "options": { "hoverEnabled": true } })));
        assert_eq!(stored(&storage, "options")["hoverEnabled"], true);
    }

    #[test]
    fn a_profiles_view_merges_into_the_whole_state_and_an_older_clients_view_means_japanese() {
        let storage = with_records(profile_records());
        let view = profiles::project_dictionary_state(Some(&stored(&storage, "dictionaryState")), "ar").unwrap();
        let mut jitendex = view["dictionaries"][1].clone();
        jitendex["displayName"] = json!("J");
        let reply = page(
            &storage,
            json!({ "type": "hd_state_cas", "baseRevision": 31, "profileId": "ar",
            "dictionaries": [jitendex, view["dictionaries"][0], view["dictionaries"][2]],
            "groups": [{ "id": "g1", "name": "Main", "dictionaryIds": ["arabic", "missing"] }] }),
        );
        ok(&reply);
        let state = stored(&storage, "dictionaryState");
        assert_eq!((state["schemaVersion"].clone(), state["revision"].clone()), (json!(2), json!(32)));
        let order: Vec<Value> =
            state["profiles"]["ar"]["dictionaries"].as_array().unwrap().iter().map(|entry| entry["id"].clone()).collect();
        assert_eq!(order, [json!("jitendex"), json!("arabic"), json!("spare")]);
        assert_eq!(state["profiles"]["ar"]["groups"], json!([{ "id": "g1", "name": "Main", "dictionaryIds": ["arabic"] }]));
        assert_eq!(state["profiles"]["ja"], schema2()["profiles"]["ja"], "other profiles are left alone");
        assert_eq!(state["dictionaries"][1]["displayName"], "J", "package-wide fields are written");
        assert!(state["dictionaries"][0].get("enabled").is_none());
        // Only this host's engine writes the whole record.
        let refused = page(&storage, json!({ "type": "hd_state_cas", "baseRevision": 32, "state": {} }));
        assert_eq!(refused["ok"], false, "{refused}");
        assert_eq!(stored(&storage, "dictionaryState")["revision"], 32);
    }

    #[test]
    fn the_engines_whole_state_commit_prunes_every_profiles_selections() {
        let mut records = profile_records();
        records["options"]["profiles"][1]["settings"]["frequencyDictionary"] = json!("Arabic");
        records["options"]["profiles"][0]["settings"]["frequencyDictionary"] = json!("Jitendex");
        records["dictionaryState"]["dictionaries"][1]["frequencyCount"] = json!(1);
        records["dictionaryState"]["dictionaries"][2]["frequencyCount"] = json!(1);
        let storage = with_records(records);
        let state = stored(&storage, "dictionaryState");
        let mut next = json!({ "dictionaries": state["dictionaries"], "profiles": state["profiles"] });
        next["profiles"]["ar"] =
            json!({ "dictionaries": [{ "id": "jitendex", "enabled": true }, { "id": "spare", "enabled": false }], "groups": [] });
        ok(&engine(&storage, json!({ "type": "hd_state_cas", "baseRevision": 31, "state": next })));
        let options = stored(&storage, "options");
        assert_eq!(options["profiles"][1]["settings"]["frequencyDictionary"], "", "Arabic left the profile");
        assert_eq!(options["profiles"][0]["settings"]["frequencyDictionary"], "", "Jitendex is disabled in Japanese");
        let conflict = engine(&storage, json!({ "type": "hd_state_cas", "baseRevision": 31, "state": next }));
        assert_eq!(conflict["ok"], false, "{conflict}");
    }

    #[test]
    fn adding_a_second_language_converts_the_records_and_removing_it_converts_them_back() {
        let records = single_profile_records();
        let storage = with_records(records.clone());
        let converted = profiles::dictionary_state_with_profiles(Some(&records["dictionaryState"]));
        let mut added = converted["profiles"].clone();
        added["en"] = json!({ "dictionaries": [], "groups": [] });
        let reply = engine(
            &storage,
            json!({ "type": "hd_profile_cas", "action": "add", "language": "en", "copyFrom": "ja", "baseRevision": 6,
            "state": { "dictionaries": converted["dictionaries"], "profiles": added } }),
        );
        ok(&reply);
        assert_eq!(reply["languages"], json!(["ja", "en"]));
        let options = stored(&storage, "options");
        assert_eq!(options["revision"], 5);
        assert_eq!(options["hoverEnabled"], false);
        let (ja, en) = (&options["profiles"][0]["settings"], &options["profiles"][1]["settings"]);
        assert_eq!(ja["scanLength"], 20);
        assert_eq!(ja["anki"]["templates"][0]["id"], "default");
        assert_eq!(en["scanLength"], 20, "the new profile copies the profile it was added from");
        assert_eq!(en["popupTheme"], "jl");
        assert_ne!(en["anki"]["templates"][0]["id"], "default", "Templates get new ids");
        assert_eq!(en["anki"]["templates"][0]["model"], "Lapis");
        assert_eq!(en["anki"]["templates"][0]["deck"], "");
        assert_eq!(en["customButtons"], json!([]));
        let state = stored(&storage, "dictionaryState");
        assert_eq!(state["schemaVersion"], 2);
        assert_eq!(page(&storage, json!({ "type": "hd_state_read" }))["languages"], json!(["ja", "en"]));
        // A selection, then removing English: the records convert back and the selection goes.
        storage.write(object(json!({ "languageSelection": { "revision": 1, "activeProfileId": "en", "autoSwitch": false } }))).unwrap();
        let reply = engine(
            &storage,
            json!({ "type": "hd_profile_cas", "action": "remove", "profileId": "en", "language": "en",
            "baseRevision": state["revision"], "state": { "dictionaries": state["dictionaries"], "profiles": { "ja": state["profiles"]["ja"] } } }),
        );
        ok(&reply);
        assert_eq!(reply["converted"], true);
        let back = stored(&storage, "dictionaryState");
        assert_eq!(back["schemaVersion"], 1);
        assert_eq!(back["dictionaries"].as_array().unwrap().len(), 2);
        let options = stored(&storage, "options");
        assert!(options.get("profiles").is_none());
        assert_eq!(options["scanLength"], 20);
        assert_eq!(options["anki"]["templates"][0]["deck"], "JP");
        assert!(storage.read("languageSelection").is_none());
        assert!(page(&storage, json!({ "type": "hd_state_read" })).get("languages").is_none());
    }

    #[test]
    fn a_language_request_changes_no_dictionary_and_the_last_language_stays() {
        let records = single_profile_records();
        let storage = with_records(records.clone());
        let converted = profiles::dictionary_state_with_profiles(Some(&records["dictionaryState"]));
        let mut added = converted["profiles"].clone();
        added["en"] = json!({ "dictionaries": [], "groups": [] });
        let fewer = converted["dictionaries"].as_array().unwrap()[1..].to_vec();
        let reply = engine(
            &storage,
            json!({ "type": "hd_profile_cas", "action": "add", "language": "en", "copyFrom": "ja", "baseRevision": 6,
            "state": { "dictionaries": fewer, "profiles": added } }),
        );
        assert_eq!(reply["ok"], false);
        let mut unknown = converted["profiles"].clone();
        unknown["xx"] = json!({ "dictionaries": [], "groups": [] });
        let reply = engine(
            &storage,
            json!({ "type": "hd_profile_cas", "action": "add", "language": "xx", "copyFrom": "ja", "baseRevision": 6,
            "state": { "dictionaries": converted["dictionaries"], "profiles": unknown } }),
        );
        assert_eq!(reply["errorCode"], "language-unknown", "{reply}");
        let reply = engine(
            &storage,
            json!({ "type": "hd_profile_cas", "action": "remove", "profileId": "ja", "language": "ja", "baseRevision": 6,
            "state": { "dictionaries": converted["dictionaries"], "profiles": {} } }),
        );
        assert_eq!(reply["ok"], false, "{reply}");
        // Languages are added and removed only by the engine.
        let refused = page(&storage, json!({ "type": "hd_profile_cas", "action": "add", "language": "en", "baseRevision": 6 }));
        assert_eq!(refused["error"], "Languages are added and removed only by the dictionary engine.");
        assert_eq!(stored(&storage, "options"), records["options"]);
        assert_eq!(stored(&storage, "dictionaryState"), records["dictionaryState"]);
    }

    #[test]
    fn erasing_a_language_empties_its_document_drops_its_counts_and_marks_and_keeps_the_japanese_ones() {
        let mut records = profile_records();
        with_english(&mut records);
        records["customDictionarySource"] = json!({ "schemaVersion": 1, "revision": 3, "semanticRevision": "a".repeat(64), "text": "猫, ねこ, cat\n",
            "languages": { "en": { "revision": 5, "semanticRevision": "b".repeat(64), "text": "run, , to move fast\n" } } });
        records["lookupStats"] = json!({ "generation": "g-ja", "revision": 4, "languages": { "en": "g-en", "ar": "g-ar" } });
        records["wordStatusOverrides"] =
            json!({ "revision": 2, "known": ["猫"], "ignored": [], "languages": { "en": { "known": ["run"], "ignored": [] } } });
        let (english, japanese) = ("lookupStats:\"g-en\":[\"run\",\"\"]", "lookupStats:\"g-ja\":[\"猫\",\"ねこ\"]");
        records[english] = json!({ "term": "run", "reading": "", "lookupCount": 1, "firstLookedUpAt": 1, "lastLookedUpAt": 1 });
        records[japanese] = json!({ "term": "猫", "reading": "ねこ", "lookupCount": 1, "firstLookedUpAt": 1, "lastLookedUpAt": 1 });
        let storage = with_records(records);
        let state = stored(&storage, "dictionaryState");
        let reply = engine(
            &storage,
            json!({ "type": "hd_profile_cas", "action": "remove", "profileId": "en", "language": "en", "erase": true,
            "baseRevision": 31, "baseDocumentRevision": 5,
            "state": { "dictionaries": state["dictionaries"], "profiles": { "ja": state["profiles"]["ja"], "ar": state["profiles"]["ar"] } } }),
        );
        ok(&reply);
        let document = stored(&storage, "customDictionarySource");
        assert_eq!((document["text"].clone(), document["revision"].clone()), (json!("猫, ねこ, cat\n"), json!(3)));
        assert_eq!(document["languages"]["en"]["text"], "");
        let descriptor = stored(&storage, "lookupStats");
        assert_eq!(descriptor["generation"], "g-ja");
        assert_eq!(descriptor["languages"], json!({ "ar": "g-ar" }));
        assert!(stored(&storage, "wordStatusOverrides")["languages"].get("en").is_none());
        assert!(storage.read(english).is_none(), "the erased language's rows are deleted");
        assert!(storage.read(japanese).is_some());
    }

    #[test]
    fn an_erase_whose_confirmed_document_revision_is_older_keeps_the_document() {
        let mut records = profile_records();
        records["customDictionarySource"] = json!({ "schemaVersion": 1, "revision": 0, "semanticRevision": custom::EMPTY_SEMANTIC_REVISION, "text": "",
            "languages": { "ar": { "revision": 4, "semanticRevision": "c".repeat(64), "text": "بيت, , house\n" } } });
        let storage = with_records(records.clone());
        let state = stored(&storage, "dictionaryState");
        let reply = engine(
            &storage,
            json!({ "type": "hd_profile_cas", "action": "remove", "profileId": "ar", "language": "ar", "erase": true,
            "baseRevision": 31, "baseDocumentRevision": 3,
            "state": { "dictionaries": state["dictionaries"], "profiles": { "ja": state["profiles"]["ja"] } } }),
        );
        assert_eq!(reply["ok"], false, "{reply}");
        assert!(reply["error"].as_str().unwrap().contains("changed after you confirmed"), "{reply}");
        assert_eq!(stored(&storage, "customDictionarySource"), records["customDictionarySource"]);
        assert_eq!(stored(&storage, "options"), records["options"]);
    }

    #[test]
    fn removing_the_active_profile_moves_lookups_to_japanese() {
        let mut records = profile_records();
        with_english(&mut records);
        records["languageSelection"] = json!({ "revision": 3, "activeProfileId": "en", "autoSwitch": true });
        let storage = with_records(records);
        let state = stored(&storage, "dictionaryState");
        ok(&engine(
            &storage,
            json!({ "type": "hd_profile_cas", "action": "remove", "profileId": "en", "language": "en", "baseRevision": 31,
            "state": { "dictionaries": state["dictionaries"], "profiles": { "ja": state["profiles"]["ja"], "ar": state["profiles"]["ar"] } } }),
        ));
        let selection = stored(&storage, "languageSelection");
        assert_eq!(selection["activeProfileId"], "ja", "{selection}");
    }

    #[test]
    fn counts_and_marks_are_kept_per_language_and_each_write_keeps_the_others() {
        let mut records = profile_records();
        records["lookupStats"] = json!({ "generation": "g-ja", "revision": 1 });
        records["wordStatusOverrides"] = json!({ "revision": 1, "known": ["猫"], "ignored": [] });
        let storage = with_records(records);
        ok(&page(&storage, json!({ "type": "hd_lookup_stats_record", "profileId": "ar", "term": "كتاب", "reading": "كتاب" })));
        let descriptor = stored(&storage, "lookupStats");
        assert_eq!(descriptor["generation"], "g-ja", "{descriptor}");
        assert!(descriptor["languages"]["ar"].is_string(), "{descriptor}");
        ok(&page(&storage, json!({ "type": "hd_lookup_stats_record", "term": "猫", "reading": "ねこ" })));
        assert_eq!(
            stored(&storage, "lookupStats")["languages"]["ar"],
            descriptor["languages"]["ar"],
            "a Japanese count keeps the Arabic ones"
        );
        let reply = page(&storage, json!({ "type": "hd_lookup_stats_read", "profileId": "ar", "term": "كتاب", "reading": "كتاب" }));
        assert_eq!(reply["statistics"]["lookupCount"], 1, "{reply}");
        ok(&page(&storage, json!({ "type": "hd_word_status_override", "profileId": "ar", "headword": "كتاب", "status": "known" })));
        let marks = stored(&storage, "wordStatusOverrides");
        assert_eq!(marks["known"], json!(["猫"]));
        assert_eq!(marks["languages"]["ar"]["known"], json!(["كتاب"]));
        // The cleanup keeps every live generation's rows.
        let stale = "lookupStats:\"stale\":[\"x\",\"\"]";
        storage
            .write(object(json!({ stale: { "term": "x", "reading": "", "lookupCount": 1, "firstLookedUpAt": 1, "lastLookedUpAt": 1 } })))
            .unwrap();
        let live = storage.0.borrow().keys().filter(|key| key.starts_with("lookupStats:")).count();
        worker::remove_obsolete_lookup_rows(&storage).unwrap();
        assert!(storage.read(stale).is_none());
        assert_eq!(storage.0.borrow().keys().filter(|key| key.starts_with("lookupStats:")).count(), live - 1);
    }

    #[test]
    fn the_personal_document_is_kept_per_language() {
        let mut records = profile_records();
        with_english(&mut records);
        records["customDictionarySource"] =
            json!({ "schemaVersion": 1, "revision": 3, "semanticRevision": "a".repeat(64), "text": "猫, ねこ, cat\n" });
        let storage = with_records(records);
        let reply = page(&storage, json!({ "type": "hd_custom_read", "profileId": "en" }));
        assert_eq!(reply["document"]["text"], "", "{reply}");
        assert_eq!(page(&storage, json!({ "type": "hd_custom_read" }))["document"]["text"], "猫, ねこ, cat\n");
        let missing = page(&storage, json!({ "type": "hd_custom_read", "profileId": "fr" }));
        assert_eq!(missing["errorCode"], "profile-missing", "{missing}");
    }

    #[test]
    fn the_worker_names_a_lookups_profile_fields() {
        let records = profile_records();
        let (options, state) = (Some(&records["options"]), Some(&records["dictionaryState"]));
        let resolve = |fields: Value| {
            let mut message = fields;
            message["target"] = json!("hoshidicts-offscreen");
            with_profile_fields(&message, options, state)
        };
        let arabic = resolve(json!({ "type": "hd_lookup", "text": "x", "profileId": "ar", "language": "ja", "dictionaries": ["forged"] }))
            .unwrap()
            .unwrap();
        assert_eq!(arabic["language"], "ar");
        assert_eq!(arabic["dictionaries"], json!(["arabic", "jitendex"]), "the enabled packages, in the profile's order");
        assert_eq!(arabic["languageOptions"], json!([]), "vowelMarks is a display setting");
        assert_eq!(arabic["profileId"], "ar");
        let japanese = resolve(json!({ "type": "hd_segment", "chunks": [] })).unwrap().unwrap();
        assert_eq!((japanese["language"].clone(), japanese["dictionaries"].clone()), (json!("ja"), json!([PERSONAL])));
        assert!(japanese.get("profileId").is_none());
        let missing = resolve(json!({ "type": "hd_kanji", "character": "x", "profileId": "en" })).unwrap().unwrap_err();
        assert_eq!(missing.code, Some("profile-missing"));
        let save = resolve(json!({ "type": "hd_custom_save", "profileId": "ar", "language": "ja" })).unwrap().unwrap();
        assert_eq!(save["language"], "ar");
        let save = resolve(json!({ "type": "hd_custom_append", "language": "ar" })).unwrap().unwrap();
        assert!(save.get("language").is_none(), "a forwarded language is replaced");
        assert!(resolve(json!({ "type": "hd_status", "profileId": "en" })).is_none());
        assert_eq!(
            resolve(json!({ "type": "hd_custom_save", "profileId": "" })).unwrap().unwrap_err().error,
            "the request named an invalid profile"
        );
        // A boolean pack setting turns its engine effect on.
        let mut settings = Map::new();
        settings.insert("informalTaMarbuta".to_owned(), json!(true));
        settings.insert("derivedForms".to_owned(), json!(true));
        assert_eq!(language_options_for("ar", &settings), [json!("convertHaToTaMarbuta"), json!("participle"), json!("verbalNoun")]);
        // Without profiles only Japanese is a lookup profile.
        let single = single_profile_records();
        let message = json!({ "target": "hoshidicts-offscreen", "type": "hd_lookup", "profileId": "ja" });
        let japanese = with_profile_fields(&message, Some(&single["options"]), Some(&single["dictionaryState"])).unwrap().unwrap();
        assert_eq!(japanese, json!({ "target": "hoshidicts-offscreen", "type": "hd_lookup" }));
        let message = json!({ "target": "hoshidicts-offscreen", "type": "hd_lookup", "profileId": "en" });
        assert!(with_profile_fields(&message, Some(&single["options"]), None).unwrap().is_err());
        assert!(with_profile_fields(&json!({ "target": "hoshidicts-offscreen", "type": "hd_lookup" }), None, None).is_none());
    }

    #[test]
    fn a_single_profile_worker_writes_nothing_of_the_profile_layer() {
        let records = single_profile_records();
        let storage = with_records(records.clone());
        ok(&page(&storage, json!({ "type": "hd_lookup_stats_record", "term": "猫", "reading": "ねこ" })));
        assert_eq!(stored(&storage, "options"), records["options"]);
        assert_eq!(stored(&storage, "dictionaryState"), records["dictionaryState"]);
        assert!(storage.read("languageSelection").is_none());
        assert!(stored(&storage, "lookupStats").get("languages").is_none());
        let refused = page(&storage, json!({ "type": "hd_options_write", "baseRevision": 4, "profileId": "en", "options": {} }));
        assert_eq!(refused["errorCode"], "profile-missing", "{refused}");
        let refused = page(&storage, json!({ "type": "hd_options_write", "baseRevision": 4, "options": {}, "packSettings": { "x": 1 } }));
        assert_eq!(refused["ok"], false, "{refused}");
        assert_eq!(stored(&storage, "options"), records["options"]);
    }
}
