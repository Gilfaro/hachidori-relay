// SPDX-License-Identifier: GPL-3.0-or-later
//! The engine requests that change the dictionaries (`engine-service.js`'
//! `hd_apply_state`, `hd_reload`, `hd_remove`, `hd_custom_save` and
//! `hd_custom_append`), answered as the extension's engine answers them.
//!
//! The extension's engine reads and commits storage by asking its service worker
//! (`hd_state_cas`, `hd_custom_cas`, ...); here the same worker requests are
//! answered in process ([`worker::handle`]), under the storage lock the caller
//! holds. A mutation loads what it is about to commit first, as the extension
//! does, so a package that will not load is never committed; once the commit
//! lands, that set is what the engine has loaded, a new generation.

use std::fs;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::ffi::Native;
use super::{Candidate, Engine, Loaded, envelope, failure_reply, frequency, import, object, package, text};
use crate::host::js::{self, strict_eq};
use crate::host::profile::JAPANESE;
use crate::host::registry;
use crate::host::settings::custom::{self, CUSTOM_DICTIONARY_ID, Identity};
use crate::host::settings::profiles::{self, NO_JAPANESE_PROFILE, PROFILE_REMOVED};
use crate::host::settings::worker::{self, Storage};
use crate::host::store;

/// The engine requests that change the dictionaries.
pub(crate) const MUTATIONS: [&str; 7] =
    ["hd_apply_state", "hd_reload", "hd_remove", "hd_custom_save", "hd_custom_append", "hd_profile_add", "hd_profile_remove"];

mod profile_changes;
/// `STORAGE_ATTEMPTS`: how often a commit that lost a race is tried again.
pub(super) const STORAGE_ATTEMPTS: usize = 3;
/// What the custom package's import calls its archive.
const CUSTOM_ARCHIVE: &str = "the custom dictionary archive";
pub(super) const IMPORT_COUNTS: [&str; 10] = [
    "termCount",
    "metaCount",
    "frequencyCount",
    "pitchCount",
    "kanjiCount",
    "mediaCount",
    "skippedRecordCount",
    "unresolvedRedirectCount",
    "missingResourceCount",
    "unreadableResourceCount",
];

pub(super) type Result<T> = std::result::Result<T, String>;

/// Where a mutation's error carries the `errorCode` the extension gives it, until
/// the reply is written.
const CODE_MARK: char = '\u{1}';

/// `profileRequestError`: an error with its `errorCode`.
pub(crate) fn coded(error: &str, code: &'static str) -> String {
    format!("{error}{CODE_MARK}{code}")
}

/// A mutation's error and the `errorCode` [`coded`] gave it.
pub(crate) fn split_code(error: &str) -> (&str, Option<&'static str>) {
    let Some((error, code)) = error.split_once(CODE_MARK) else { return (error, None) };
    let code = ["profile-missing", "no-japanese-profile", "language-unknown", "language-present"].into_iter().find(|known| *known == code);
    (error, code)
}

/// The personal dictionary a save is for: `language` is `None` for a request
/// that names none, which means Japanese (`customRequest`).
pub(super) struct Custom {
    language: Option<String>,
    identity: Identity,
}

/// `customRequest`.
fn custom_request(message: &Value) -> Result<Custom> {
    let Some(language) = message.get("language").filter(|language| !language.is_null()) else {
        return Ok(Custom { language: None, identity: custom::japanese() });
    };
    let Some(language) = language.as_str().filter(|code| crate::host::profile::language_code(code)) else {
        return Err("the custom dictionary request named an invalid language".to_owned());
    };
    let identity = custom::identity(language).ok_or_else(|| format!("This Hachidori has no personal dictionary for {language}."))?;
    Ok(Custom { language: Some(language.to_owned()), identity })
}

/// `engineState`: a schema-2 record as the engine reads it, every package once
/// (`allPackages`), with the record kept as `stored`.
pub(super) fn engine_state(state: Value) -> Value {
    if !profiles::has_dictionary_profiles(Some(&state)) {
        return state;
    }
    json!({ "schemaVersion": 1, "revision": state["revision"], "dictionaries": profiles::all_packages(Some(&state)), "groups": [],
        "stored": state })
}

/// `publicState`: the record a reply carries, not the engine's view of it.
pub(super) fn public_state(state: &Value) -> &Value {
    state.get("stored").unwrap_or(state)
}

/// `withPublicState`.
fn with_public_state(mut reply: Map<String, Value>) -> Map<String, Value> {
    if let Some(state) = reply.get_mut("state") {
        *state = public_state(state).clone();
    }
    reply
}

/// `validProfileState`.
fn valid_profile_state(state: &Value) -> bool {
    js::whole(state.get("revision")).is_some_and(|revision| revision >= 0.0)
        && state.get("dictionaries").is_some_and(Value::is_array)
        && state.get("profiles").is_some_and(Value::is_object)
}

/// The profile a commit's new packages join, and the installed packages that
/// join it too (`commitDictionaryState`'s context).
pub(super) struct Joining<'a> {
    pub(super) profile_id: &'a str,
    pub(super) joins: Vec<Value>,
}

impl Joining<'_> {
    pub(super) const JAPANESE: Joining<'static> = Joining { profile_id: JAPANESE, joins: Vec::new() };
}

/// `commitDictionaryCandidate`'s check: the single-profile shape has only the
/// Japanese profile.
pub(super) fn assert_profile_exists(state: &Value, profile_id: &str) -> Result<()> {
    if state.get("stored").is_none() && profile_id != JAPANESE {
        return Err(coded(PROFILE_REMOVED, "profile-missing"));
    }
    Ok(())
}

/// `joinedIds`: the ids of the packages at the generation path `joining` names.
pub(super) fn joined_ids(next: &[Value], joining: Option<&str>) -> Vec<Value> {
    let Some(path) = joining else { return Vec::new() };
    next.iter().filter(|row| text(row.get("path")) == path).map(|row| row.get("id").cloned().unwrap_or(Value::Null)).collect()
}

/// The custom source and dictionary state a custom save works from.
struct Snapshot {
    document: Value,
    state: Value,
}

impl Snapshot {
    fn revision(&self) -> Option<&Value> {
        self.document.get("revision")
    }

    fn into_fields(self) -> Map<String, Value> {
        object(json!({ "document": self.document, "state": self.state }))
    }
}

/// Why a custom generation was not committed (`CustomCommitRejectedError` or a failure).
enum Refused {
    Rejected(Map<String, Value>),
    Failed(String),
}

impl From<String> for Refused {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

pub(super) fn is_true(fields: &Map<String, Value>, key: &str) -> bool {
    fields.get(key) == Some(&Value::Bool(true))
}

/// `reply.error || fallback`.
pub(super) fn error_or(reply: &Map<String, Value>, fallback: &str) -> String {
    reply.get("error").and_then(Value::as_str).filter(|error| !error.is_empty()).unwrap_or(fallback).to_owned()
}

pub(super) fn list(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

pub(super) fn paths(dictionaries: &[Value]) -> Vec<&str> {
    dictionaries.iter().map(|dictionary| text(dictionary.get("path"))).collect()
}

/// The managed personal package of `identity`.
fn is_personal(dictionary: &Value, identity: &Identity) -> bool {
    dictionary.get("id").and_then(Value::as_str) == Some(identity.id.as_str())
}

/// `customStaleReply`.
fn stale_reply(snapshot: Snapshot) -> Map<String, Value> {
    let mut reply = object(json!({ "ok": false, "stale": true, "error": "the custom dictionary source changed while it was being saved" }));
    reply.extend(snapshot.into_fields());
    reply
}

/// `customSnapshotFromReply`.
fn snapshot_from(reply: &Map<String, Value>) -> Result<Snapshot> {
    Ok(Snapshot {
        document: custom::normalise_document(reply.get("document"))?,
        state: engine_state(reply.get("state").cloned().unwrap_or(Value::Null)),
    })
}

/// Whether a refused custom commit is a race worth trying again.
fn retry(reply: &Map<String, Value>, attempt: usize) -> bool {
    is_true(reply, "conflict")
        && !is_true(reply, "stale")
        && reply.contains_key("document")
        && reply.get("state").is_some_and(|state| !state.is_null())
        && attempt + 1 < STORAGE_ATTEMPTS
}

/// `groupsForDictionaries`: each group without the dictionaries that are gone.
fn groups_for(groups: Option<&Value>, dictionaries: &[Value]) -> Value {
    let installed: Vec<Option<&Value>> = dictionaries.iter().map(|dictionary| dictionary.get("id")).collect();
    let groups = list(groups).iter().map(|group| {
        let mut group = group.as_object().cloned().unwrap_or_default();
        let ids = list(group.get("dictionaryIds"));
        let kept: Vec<Value> = ids
            .iter()
            .enumerate()
            .filter(|(index, id)| {
                installed.iter().any(|installed| strict_eq(*installed, Some(id)))
                    && ids.iter().position(|other| strict_eq(Some(other), Some(id))) == Some(*index)
            })
            .map(|(_, id)| id.clone())
            .collect();
        group.insert("dictionaryIds".to_owned(), Value::Array(kept));
        Value::Object(group)
    });
    Value::Array(groups.collect())
}

/// `withCustomDictionary`: `stored` with `generated` as the managed package of
/// `identity`, first; `in_place` (a schema-2 union view) keeps an installed
/// personal package where it is.
fn with_custom(stored: &[Value], generated: &Value, identity: &Identity, in_place: bool) -> Result<Vec<Value>> {
    if stored.iter().any(|dictionary| !is_personal(dictionary, identity) && text(dictionary.get("title")) == identity.title) {
        return Err(format!("a dictionary named {} is already installed", identity.title));
    }
    let current = stored.iter().find(|dictionary| is_personal(dictionary, identity));
    let mut entry = generated.as_object().cloned().unwrap_or_default();
    let display_name = current.and_then(|current| current.get("displayName")).filter(|name| name.is_string());
    entry.extend(object(json!({
        "id": identity.id,
        "title": identity.title,
        "displayName": display_name.cloned().unwrap_or(Value::Null),
        "enabled": true,
        "favorite": current.and_then(|current| current.get("favorite")) == Some(&Value::Bool(true)),
        "isUpdatable": false,
        "indexUrl": null,
        "downloadUrl": null,
        "lastUpdateCheck": null,
    })));
    if in_place && current.is_some() {
        let entry = Value::Object(entry);
        return Ok(stored
            .iter()
            .map(|dictionary| if is_personal(dictionary, identity) { entry.clone() } else { dictionary.clone() })
            .collect());
    }
    Ok(std::iter::once(Value::Object(entry))
        .chain(stored.iter().filter(|dictionary| !is_personal(dictionary, identity)).cloned())
        .collect())
}

/// `normaliseReport`.
fn normalise_report(raw: &Value) -> Map<String, Value> {
    let mut report = object(json!({ "success": raw.get("success") == Some(&Value::Bool(true)), "title": text(raw.get("title")) }));
    for key in IMPORT_COUNTS {
        let count = js::to_number(raw.get(key));
        report.insert(key.to_owned(), js::number(if count.is_finite() { count } else { 0.0 }));
    }
    report.insert("error".to_owned(), Value::from(text(raw.get("error"))));
    report
}

/// `dictionaryImportError`: an import failure, as a message about `label` (the
/// archive's file name) and its `errorCode`.
pub(super) struct ImportError {
    pub(super) message: String,
    pub(super) code: &'static str,
}

/// `isImportMemoryError`, for a message.
fn memory_error(detail: &str) -> bool {
    let lower = detail.to_lowercase();
    [
        "bad_alloc",
        "out of memory",
        "not enough memory",
        "cannot allocate memory",
        "memory access out of bounds",
        "failed to grow memory",
        "could not allocate",
        "invalid array length",
        "invalid typed array length",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// `dictionaryImportError(detail, label, phase)`.
pub(super) fn import_error(label: &str, detail: &str, phase: &str) -> ImportError {
    let (detail, given) = split_code(detail);
    let memory = memory_error(detail);
    if detail.starts_with(&format!("{label}: ")) {
        return ImportError { message: detail.to_owned(), code: if memory { "import-memory" } else { given.unwrap_or("import-failed") } };
    }
    let lower = detail.to_lowercase();
    let generic =
        detail.is_empty() || ["exception", "unknown error", "fs error", "undefined", "null", "[object object]"].contains(&lower.as_str());
    let mut code = given.unwrap_or("import-failed");
    let (detail, advice) = if memory {
        code = "import-memory";
        (
            format!("The browser could not allocate enough memory. {detail}"),
            " Close other tabs or applications and enable Low memory mode in Settings → Advanced → Memory, then retry.",
        )
    } else if lower.contains("no space left") || lower.contains("disk full") {
        code = "import-storage-full";
        (
            format!("Dictionary storage is full or its browser quota is exhausted. {detail}"),
            " Free disk space or remove unused dictionaries, then retry.",
        )
    } else if generic {
        (
            "The dictionary engine stopped without an error description.".to_owned(),
            " Retry the import; if it still fails, report this filename and stage together with your browser and Hachidori versions.",
        )
    } else {
        (detail.to_owned(), "")
    };
    ImportError { message: format!("{label}: {phase} failed. {detail}{advice}"), code }
}

impl Engine {
    /// `handleEngineMessage` for a mutation. The caller holds the storage lock, so
    /// it runs one at a time with the worker requests.
    pub(crate) fn mutate(&self, storage: &impl Storage, message: &Value) -> Value {
        let kind = text(message.get("type")).to_owned();
        let request_id = message.get("requestId").cloned().unwrap_or(Value::Null);
        self.run(storage, &kind, &request_id, |session| match kind.as_str() {
            "hd_apply_state" => session.apply_state(message),
            "hd_reload" => session.reload(),
            "hd_remove" => session.remove(message),
            "hd_custom_save" => session.custom_save(message),
            "hd_custom_append" => session.custom_append(message),
            "hd_profile_add" => session.add_profile(message),
            "hd_profile_remove" => session.remove_profile(message),
            _ => Err(format!("unknown request type {}", Value::from(kind.as_str()))),
        })
    }

    /// One mutation `kind` in the engine's envelope: `answer`'s payload, or its error
    /// as the engine's failure reply. The caller holds the storage lock.
    pub(super) fn run<S: Storage>(
        &self,
        storage: &S,
        kind: &str,
        request_id: &Value,
        answer: impl FnOnce(&mut Session<'_, S>) -> Result<Map<String, Value>>,
    ) -> Value {
        let result = {
            let mut loaded = self.lock();
            let mut native = Native::lock();
            let mut session = Session { engine: self, storage, native: &mut native, loaded: &mut loaded };
            let result = answer(&mut session);
            drop(native);
            result.map(|payload| (payload, loaded.generation)).map_err(|error| {
                let (error, code) = split_code(&error);
                let mut reply = failure_reply(kind, request_id, error, &loaded);
                if let Some(code) = code {
                    reply.insert("errorCode".to_owned(), Value::from(code));
                }
                reply
            })
        };
        match result {
            Ok((mut payload, generation)) => {
                let mut reply = envelope(kind, request_id, generation);
                reply.insert("ok".to_owned(), payload.remove("ok").unwrap_or(Value::Bool(true)));
                reply.insert("error".to_owned(), payload.remove("error").unwrap_or(Value::Null));
                reply.extend(payload);
                Value::Object(reply)
            }
            Err(failure) => Value::Object(failure),
        }
    }
}

/// `ask`: a worker request, its reply without the envelope's routing fields.
pub(super) fn ask(storage: &impl Storage, kind: &str, fields: Value) -> Map<String, Value> {
    let mut message = object(fields);
    message.insert("type".to_owned(), Value::from(kind));
    let mut reply = object(worker::handle_from(storage, &Value::Object(message), &[], worker::Sender::Engine));
    for routing in ["type", "requestId", "generation"] {
        reply.remove(routing);
    }
    reply
}

/// `requestDictionaryStorage`'s state.
pub(super) fn dictionary_state(storage: &impl Storage) -> Result<Value> {
    let mut reply = ask(storage, "hd_state_read", json!({}));
    if !is_true(&reply, "ok") {
        return Err(error_or(&reply, "the service worker could not read dictionary state"));
    }
    let state = reply.remove("state").unwrap_or(Value::Null);
    if profiles::has_dictionary_profiles(Some(&state)) {
        if !valid_profile_state(&state) || !reply.get("languages").is_some_and(Value::is_array) {
            return Err("the service worker returned invalid dictionary state".to_owned());
        }
        return Ok(engine_state(state));
    }
    if !state.is_null()
        && (!strict_eq(state.get("schemaVersion"), Some(&json!(1)))
            || js::whole(state.get("revision")).is_none()
            || !state.get("dictionaries").is_some_and(Value::is_array))
    {
        return Err("the service worker returned invalid dictionary state".to_owned());
    }
    if !reply.get("legacyDictionaries").is_some_and(|legacy| legacy.is_null() || legacy.is_array()) {
        return Err("the service worker returned an invalid legacy dictionary list".to_owned());
    }
    Ok(state)
}

/// One mutation, holding the engine and its storage.
pub(super) struct Session<'a, S: Storage> {
    pub(super) engine: &'a Engine,
    pub(super) storage: &'a S,
    pub(super) native: &'a mut Native,
    pub(super) loaded: &'a mut Loaded,
}

impl<S: Storage> Session<'_, S> {
    fn ask(&self, kind: &str, fields: Value) -> Map<String, Value> {
        ask(self.storage, kind, fields)
    }

    pub(super) fn dictionary_state(&self) -> Result<Value> {
        dictionary_state(self.storage)
    }

    /// `readCustomStorage(language)`: `None` for a request that names none.
    fn custom_storage(&self, language: Option<&str>) -> Result<Snapshot> {
        let reply = self.ask("hd_custom_read", language.map_or_else(|| json!({}), |language| json!({ "language": language })));
        if !is_true(&reply, "ok") {
            return Err(error_or(&reply, "the service worker could not read the custom dictionary"));
        }
        let state = reply.get("state").cloned().unwrap_or(Value::Null);
        if profiles::has_dictionary_profiles(Some(&state)) {
            if !valid_profile_state(&state) {
                return Err("the service worker returned invalid custom dictionary state".to_owned());
            }
            return Ok(Snapshot { document: custom::normalise_document(reply.get("document"))?, state: engine_state(state) });
        }
        if !state.is_null()
            && (!strict_eq(state.get("schemaVersion"), Some(&json!(1)))
                || js::whole(state.get("revision")).is_none()
                || !state.get("dictionaries").is_some_and(Value::is_array)
                || !state.get("groups").is_some_and(Value::is_array))
        {
            return Err("the service worker returned invalid custom dictionary state".to_owned());
        }
        Ok(Snapshot { document: custom::normalise_document(reply.get("document"))?, state })
    }

    /// `loadDictionaries(dictionaries, { committed })`: loads what a mutation is about
    /// to commit. Only a package at a `committed` path may fail to load.
    pub(super) fn preload(&mut self, dictionaries: &[Value], committed: &[&str]) -> Result<Candidate> {
        for dictionary in dictionaries {
            if package::root(dictionary).is_none() {
                return Err(format!("refusing to load an invalid dictionary path: {}", text(dictionary.get("path"))));
            }
        }
        // Whatever happens, the engine no longer holds the published set.
        self.native.owner = None;
        self.engine.load(self.native, dictionaries, Some(committed))
    }

    /// `publishLoadedDictionaries`: the loaded candidate is now what is committed.
    pub(super) fn publish(&mut self, candidate: Candidate) {
        let state = self.engine.store.get(store::DICTIONARY_STATE).unwrap_or(Value::Null);
        self.engine.install(self.native, self.loaded, candidate, state, true);
    }

    /// `restoreCommittedDictionaries`: loads the committed state again.
    pub(super) fn restore(&mut self) -> Result<()> {
        let state = self.engine.store.get(store::DICTIONARY_STATE).unwrap_or(Value::Null);
        self.engine.reload(self.native, self.loaded, state, true).map_err(|failure| failure.0)
    }

    /// Removes the generations of `dropped` that the committed state no longer names
    /// (`cleanupCommittedDictionaries`, for what this mutation dropped).
    pub(super) fn cleanup(&self, dropped: &[&str]) {
        let state = self.engine.store.get(store::DICTIONARY_STATE).unwrap_or(Value::Null);
        let kept = store::generations(&state);
        for path in dropped {
            if !kept.iter().any(|(_, kept)| kept == path)
                && let Err(error) = self.engine.store.remove_dictionary(path)
            {
                tracing::warn!(%error, %path, "could not remove an unreferenced dictionary");
            }
        }
    }

    /// `withTargetLanguage`: the row with the target language its index declares.
    fn with_target_language(&self, row: &Value) -> Value {
        let Some(path) = row.get("path").and_then(Value::as_str) else { return row.clone() };
        let Ok(index) = import::read_index(&self.engine.store.dictionary_dir(path)) else { return row.clone() };
        let mut rest = row.as_object().cloned().unwrap_or_default();
        rest.remove("targetLanguage");
        if let Some(target) = import::optional_text(index.get("targetLanguage")) {
            rest.insert("targetLanguage".to_owned(), target.clone());
        }
        Value::Object(rest)
    }

    /// `commitDictionaryState` of the engine's next list against `state`, the
    /// engine's view it was built from: with profiles the list is merged into the
    /// stored record (`commitProfileDictionaryState`), and new packages join
    /// `joining`'s profile. The reply's state is the engine's view.
    pub(super) fn commit_state(&self, state: &Value, dictionaries: &[Value], joining: &Joining<'_>) -> Result<Map<String, Value>> {
        let base = state.get("revision").filter(|revision| !revision.is_null()).cloned().unwrap_or(json!(0));
        let mut reply = match state.get("stored") {
            None => self.ask("hd_state_cas", json!({ "baseRevision": base, "dictionaries": dictionaries })),
            Some(stored) => {
                let rows: Vec<Value> = dictionaries.iter().map(|row| self.with_target_language(row)).collect();
                let next = profiles::merge_engine_dictionaries(stored, &rows, joining.profile_id, &joining.joins)
                    .map_err(|refused| coded(refused.error, refused.code))?;
                self.ask(
                    "hd_state_cas",
                    json!({ "baseRevision": base,
                    "state": { "dictionaries": next["dictionaries"], "profiles": next["profiles"] } }),
                )
            }
        };
        if let Some(committed) = reply.get_mut("state") {
            *committed = engine_state(committed.take());
        }
        Ok(reply)
    }

    fn apply_state(&mut self, message: &Value) -> Result<Map<String, Value>> {
        let base = message.get("baseRevision").filter(|base| js::whole(Some(base)).is_some());
        let Some(base) = base else { return Err("the dictionary state change carried no valid base revision".to_owned()) };
        let Some(dictionaries) = message.get("dictionaries").and_then(Value::as_array) else {
            return Err("the dictionary state change carried no dictionary list".to_owned());
        };
        if let Some(applied) = self.apply_profile_state(message, base)? {
            return Ok(applied);
        }
        let mut restoring = false;
        let result = self.apply(base, dictionaries, &mut restoring);
        if result.is_err() && !restoring {
            // The error is the answer; a failed restore is reported by hd_status.
            let _ = self.restore();
        }
        result
    }

    fn apply(&mut self, base: &Value, dictionaries: &[Value], restoring: &mut bool) -> Result<Map<String, Value>> {
        let state = self.dictionary_state()?;
        let committed = paths(list(state.get("dictionaries")));
        let candidate = self.preload(dictionaries, &committed)?;
        let mut reply = self.ask("hd_state_cas", json!({ "baseRevision": base, "dictionaries": dictionaries }));
        if is_true(&reply, "ok") {
            self.publish(candidate);
            return Ok(object(owned_json!({ "state": reply.remove("state").unwrap_or(Value::Null), "loadPath": "full" })));
        }
        let state = reply.remove("state").unwrap_or(Value::Null);
        if !is_true(&reply, "conflict") || state.is_null() {
            return Err(error_or(&reply, "the dictionary state could not be saved"));
        }
        *restoring = true;
        self.restore()?;
        Ok(object(json!({
            "ok": false, "conflict": true,
            "error": error_or(&reply, "the dictionary state changed while it was being written"), "state": state,
        })))
    }

    /// `applyProfileState`: once the records have profiles, the profile's schema-1
    /// view is merged into the whole record before anything loads, so the loaded
    /// set stays the union of every profile's. A view that names no profile is an
    /// older client's and means the Japanese profile. `None` when the records have
    /// no profiles, for the single-profile path.
    fn apply_profile_state(&mut self, message: &Value, base: &Value) -> Result<Option<Map<String, Value>>> {
        let state = self.dictionary_state()?;
        let named = message.get("profileId");
        let Some(stored) = state.get("stored") else {
            if named.is_some_and(|named| named != JAPANESE) {
                return Err(coded(PROFILE_REMOVED, "profile-missing"));
            }
            return Ok(None);
        };
        if !strict_eq(state.get("revision"), Some(base)) {
            return Ok(Some(object(json!({ "ok": false, "conflict": true,
                "error": "the dictionary state changed while it was being written", "state": stored }))));
        }
        let profile_id = named.and_then(Value::as_str).unwrap_or(JAPANESE);
        if !stored["profiles"].get(profile_id).is_some_and(Value::is_object) {
            return Err(if named.is_none() {
                coded(NO_JAPANESE_PROFILE, "no-japanese-profile")
            } else {
                coded(PROFILE_REMOVED, "profile-missing")
            });
        }
        let view = json!({ "dictionaries": message.get("dictionaries"), "groups": message.get("groups") });
        let next = profiles::merge_dictionary_view(stored, profile_id, &view, named.is_none())?;
        let union = profiles::all_packages(Some(&next));
        let attempted = (|| {
            let candidate = self.preload(&union, &paths(list(state.get("dictionaries"))))?;
            let reply = self.ask(
                "hd_state_cas",
                json!({ "baseRevision": base,
                "state": { "dictionaries": next["dictionaries"], "profiles": next["profiles"] } }),
            );
            Ok::<_, String>((candidate, reply))
        })();
        let (candidate, mut reply) = match attempted {
            Ok(attempted) => attempted,
            Err(error) => {
                let _ = self.restore();
                return Err(error);
            }
        };
        if is_true(&reply, "ok") {
            self.publish(candidate);
            return Ok(Some(object(owned_json!({ "state": reply.remove("state").unwrap_or(Value::Null), "loadPath": "full" }))));
        }
        let committed = reply.remove("state").unwrap_or(Value::Null);
        if !is_true(&reply, "conflict") || committed.is_null() {
            let _ = self.restore();
            return Err(error_or(&reply, "the dictionary state could not be saved"));
        }
        self.restore()?;
        Ok(Some(object(json!({ "ok": false, "conflict": true,
            "error": error_or(&reply, "the dictionary state changed while it was being written"), "state": committed }))))
    }

    /// `reloadFromStorage`: the stored state, loaded again.
    fn reload(&mut self) -> Result<Map<String, Value>> {
        self.dictionary_state()?;
        self.restore()?;
        Ok(object(json!({ "dictionaryCount": self.loaded.dictionary_count })))
    }

    fn remove(&mut self, message: &Value) -> Result<Map<String, Value>> {
        let title = text(message.get("title"));
        let id = message.get("id").and_then(Value::as_str).filter(|id| !id.is_empty());
        if !package::usable_title(title) {
            return Err("the remove request carried an unusable dictionary title".to_owned());
        }
        let state = self.dictionary_state()?;
        if state.is_null() {
            return Err("the dictionary state is unavailable".to_owned());
        }
        let dictionaries = list(state.get("dictionaries"));
        // removalTarget
        let target = dictionaries.iter().find(|dictionary| match id {
            None => text(dictionary.get("title")) == title,
            Some(id) => dictionary.get("id").and_then(Value::as_str) == Some(id),
        });
        let Some(target) = target else { return Ok(Map::new()) };
        if text(target.get("title")) != title {
            return Err("the remove request dictionary ID and title do not match".to_owned());
        }
        if strict_eq(target.get("id"), Some(&json!(CUSTOM_DICTIONARY_ID))) || custom::personal_language(target).is_some() {
            return Err("the managed custom dictionary can only be removed by saving an empty source".to_owned());
        }
        assert_legacy_removal(&state, message, target)?;
        let remaining: Vec<Value> =
            dictionaries.iter().filter(|dictionary| !strict_eq(dictionary.get("id"), target.get("id"))).cloned().collect();
        let candidate = match self.preload(&remaining, &paths(&remaining)) {
            Ok(candidate) => candidate,
            Err(error) => {
                return Err(match self.restore() {
                    Ok(()) => error,
                    Err(restore) => format!("{error}; removal rollback failed: {restore}"),
                });
            }
        };
        // A removal removes the package from every profile.
        let mut reply = match self.commit_state(&state, &remaining, &Joining::JAPANESE) {
            Ok(reply) => reply,
            Err(error) => {
                return Err(match self.restore() {
                    Ok(()) => error,
                    Err(restore) => format!("{error}; removal rollback failed: {restore}"),
                });
            }
        };
        if is_true(&reply, "ok") {
            self.publish(candidate);
            self.cleanup(&[text(target.get("path"))]);
            return Ok(Map::new());
        }
        let error = error_or(&reply, "the dictionary removal could not be saved");
        if let Err(restore) = self.restore() {
            return Err(format!("{error}; removal rollback failed: {restore}"));
        }
        let state = reply.remove("state").unwrap_or(Value::Null);
        Ok(object(json!({
            "ok": false, "conflict": is_true(&reply, "conflict"), "error": error, "state": public_state(&state),
        })))
    }

    fn custom_save(&mut self, message: &Value) -> Result<Map<String, Value>> {
        let base = message.get("baseDocumentRevision").and_then(|base| js::whole(Some(base)));
        let Some(base) = base else { return Err("the custom dictionary save carried no valid document revision".to_owned()) };
        let Some(source) = message.get("text").and_then(Value::as_str) else {
            return Err("the custom dictionary save carried no source text".to_owned());
        };
        let custom = custom_request(message)?;
        let snapshot = self.custom_storage(custom.language.as_deref())?;
        if snapshot.revision().and_then(Value::as_f64) != Some(base) {
            return Ok(with_public_state(stale_reply(snapshot)));
        }
        self.save_custom(snapshot, source, &custom).map(with_public_state)
    }

    fn custom_append(&mut self, message: &Value) -> Result<Map<String, Value>> {
        // This read happens under the mutation lock: a Note submitted behind a
        // Settings save appends to that newly committed source.
        let custom = custom_request(message)?;
        let snapshot = self.custom_storage(custom.language.as_deref())?;
        let source = custom::append(text(snapshot.document.get("text")), message.get("entry"), custom.identity.reading_required)?;
        self.save_custom(snapshot, &source, &custom).map(with_public_state)
    }

    /// `saveCustomDictionary`.
    fn save_custom(&mut self, snapshot: Snapshot, source: &str, custom: &Custom) -> Result<Map<String, Value>> {
        let identity = &custom.identity;
        let (entries, errors) = custom::parse(source, identity.reading_required);
        let semantic = custom::semantic_revision(&entries, identity.reading_required)?;
        let dictionaries = list(js::property(Some(&snapshot.state), "dictionaries")?).to_vec();
        let with_errors = |mut reply: Map<String, Value>, rest: Value| {
            reply.insert("errors".to_owned(), Value::Array(errors.clone()));
            reply.extend(object(rest));
            reply
        };
        if entries.is_empty() {
            return Ok(with_errors(self.remove_custom(snapshot, source, &semantic, custom)?, json!({})));
        }
        if dictionaries.iter().any(|dictionary| {
            !is_personal(dictionary, identity) && dictionary.get("title").and_then(Value::as_str) == Some(identity.title.as_str())
        }) {
            return Err(format!("a dictionary named {} is already installed", identity.title));
        }
        if snapshot.document.get("semanticRevision").and_then(Value::as_str) == Some(semantic.as_str())
            && self.custom_package_satisfies(&snapshot.state, &semantic, entries.len(), identity)
        {
            let saved = self.save_custom_source(snapshot, source, &semantic, custom)?;
            return Ok(with_errors(saved, json!({ "rebuilt": false, "removed": false })));
        }
        let archive = custom::build_zip(&entries, &semantic, identity)?;
        let root = package::new_generation_root()?;
        let report = self.import_custom(&archive, &root)?;
        if !is_true(&report, "success") {
            self.discard(&root, identity);
            return Ok(with_errors(Map::new(), json!({ "rebuilt": true, "removed": false, "report": report })));
        }
        match self.commit_custom_generation(snapshot, source, &semantic, &root, &report, custom) {
            Ok(committed) => Ok(with_errors(committed, json!({ "rebuilt": true, "removed": false, "report": report }))),
            Err(refused) => {
                self.discard(&root, identity);
                match refused {
                    Refused::Rejected(reply) => Ok(with_errors(reply, json!({}))),
                    Refused::Failed(error) => Err(error),
                }
            }
        }
    }

    /// `commitCustomStorage`: the custom source, and the dictionary list when it changes.
    fn commit_custom(
        &self,
        snapshot: &Snapshot,
        source: &str,
        semantic: &str,
        dictionaries: Option<&[Value]>,
        custom: &Custom,
    ) -> Result<Map<String, Value>> {
        if snapshot.state.is_null() {
            return Err("the dictionary state is unavailable".to_owned());
        }
        if let Some(stored) = snapshot.state.get("stored") {
            return self.commit_profile_custom(snapshot, stored, source, semantic, dictionaries, custom);
        }
        let mut fields = object(json!({
            "baseDocumentRevision": snapshot.revision(), "baseRevision": snapshot.state.get("revision"),
            "text": source, "semanticRevision": semantic,
        }));
        if let Some(dictionaries) = dictionaries {
            let groups = groups_for(snapshot.state.get("groups"), dictionaries);
            let list = Value::Array(dictionaries.to_vec());
            let same = |key: &str, value: &Value| snapshot.state.get(key).is_some_and(|current| js::same_json(current, value));
            if !same("dictionaries", &list) || !same("groups", &groups) {
                fields.insert("dictionaries".to_owned(), list);
                fields.insert("groups".to_owned(), groups);
            }
        }
        Ok(self.ask("hd_custom_cas", Value::Object(fields)))
    }

    /// `commitProfileCustomStorage`: `hd_custom_cas` for records with profiles, the
    /// language's document, and the whole schema-2 record when the package list changes.
    fn commit_profile_custom(
        &self,
        snapshot: &Snapshot,
        stored: &Value,
        source: &str,
        semantic: &str,
        dictionaries: Option<&[Value]>,
        custom: &Custom,
    ) -> Result<Map<String, Value>> {
        let next = match dictionaries {
            None => None,
            Some(dictionaries) => {
                let rows: Vec<Value> = dictionaries.iter().map(|row| self.with_target_language(row)).collect();
                Some(
                    profiles::merge_engine_dictionaries(stored, &rows, JAPANESE, &[])
                        .map_err(|refused| coded(refused.error, refused.code))?,
                )
            }
        };
        let changes = next.as_ref().filter(|next| {
            !(js::same_json(&next["dictionaries"], &stored["dictionaries"]) && js::same_json(&next["profiles"], &stored["profiles"]))
        });
        let mut fields = object(json!({
            "language": custom.language.as_deref().unwrap_or(JAPANESE),
            "baseDocumentRevision": snapshot.revision(), "baseRevision": snapshot.state.get("revision"),
            "text": source, "semanticRevision": semantic,
        }));
        if let Some(next) = changes {
            fields.insert("state".to_owned(), json!({ "dictionaries": next["dictionaries"], "profiles": next["profiles"] }));
        }
        let mut reply = self.ask("hd_custom_cas", Value::Object(fields));
        if let Some(state) = reply.get_mut("state") {
            *state = engine_state(state.take());
        }
        Ok(reply)
    }

    /// `customPackageSatisfies`: whether the committed custom package already holds
    /// these entries, and loads.
    fn custom_package_satisfies(&mut self, state: &Value, semantic: &str, entry_count: usize, identity: &Identity) -> bool {
        let dictionaries = list(state.get("dictionaries"));
        let in_place = state.get("stored").is_some();
        let custom = if in_place { dictionaries.iter().find(|row| is_personal(row, identity)) } else { dictionaries.first() };
        let Some(custom) = custom else { return false };
        if !is_personal(custom, identity)
            || custom.get("title").and_then(Value::as_str) != Some(identity.title.as_str())
            || custom.get("enabled") != Some(&Value::Bool(true))
            || custom.get("revision").and_then(Value::as_str) != Some(semantic)
            || !strict_eq(custom.get("termCount"), Some(&json!(entry_count)))
            || !package::in_generation(custom)
        {
            return false;
        }
        let path = text(custom.get("path"));
        let Ok(generated) = package::from_index(&self.engine.store.dictionary_dir(path), path) else { return false };
        if !custom::metadata_matches(&generated, semantic, entry_count, identity) {
            return false;
        }
        let Ok(with) = with_custom(dictionaries, &generated, identity, in_place) else { return false };
        if !js::same_json(&Value::Array(dictionaries.to_vec()), &Value::Array(with)) {
            return false;
        }
        let others: Vec<Value> = dictionaries.iter().filter(|row| !is_personal(row, identity)).cloned().collect();
        let others = paths(&others);
        let Ok(candidate) = self.preload(dictionaries, &others) else { return false };
        // Loaded, not published: the committed state is unchanged.
        let stored = self.engine.store.get(store::DICTIONARY_STATE).unwrap_or(Value::Null);
        let publish = self.loaded.state.as_ref() != Some(&stored);
        self.engine.install(self.native, self.loaded, candidate, stored, publish);
        true
    }

    /// `commitCustomSourceOnly`: a source change that keeps the package's entries.
    fn save_custom_source(&self, initial: Snapshot, source: &str, semantic: &str, custom: &Custom) -> Result<Map<String, Value>> {
        let base = initial.revision().cloned();
        let mut snapshot = initial;
        for _ in 0..STORAGE_ATTEMPTS {
            if snapshot.revision() != base.as_ref() {
                return Ok(stale_reply(snapshot));
            }
            let reply = self.commit_custom(&snapshot, source, semantic, None, custom)?;
            if is_true(&reply, "ok") || is_true(&reply, "stale") {
                return Ok(reply);
            }
            if !is_true(&reply, "conflict") || !reply.contains_key("document") || reply.get("state").is_none_or(Value::is_null) {
                return Ok(reply);
            }
            snapshot = snapshot_from(&reply)?;
        }
        let mut reply = object(json!({
            "ok": false, "conflict": true, "error": "the dictionary state kept changing while the custom source was being saved",
        }));
        reply.extend(snapshot.into_fields());
        Ok(reply)
    }

    /// `commitCustomRemoval`: an empty source, which drops the managed package.
    fn remove_custom(&mut self, initial: Snapshot, source: &str, semantic: &str, custom: &Custom) -> Result<Map<String, Value>> {
        let identity = &custom.identity;
        let base = initial.revision().cloned();
        let mut snapshot = initial;
        for attempt in 0..STORAGE_ATTEMPTS {
            if snapshot.revision() != base.as_ref() {
                return Ok(stale_reply(snapshot));
            }
            let current = list(js::property(Some(&snapshot.state), "dictionaries")?).to_vec();
            let dropped: Vec<String> =
                current.iter().filter(|entry| is_personal(entry, identity)).map(|entry| text(entry.get("path")).to_owned()).collect();
            let dictionaries: Vec<Value> = current.iter().filter(|entry| !is_personal(entry, identity)).cloned().collect();
            let removes = dictionaries.len() != current.len();
            let attempted = (|| {
                let candidate = if removes { Some(self.preload(&dictionaries, &paths(&dictionaries))?) } else { None };
                let reply = self.commit_custom(&snapshot, source, semantic, removes.then_some(dictionaries.as_slice()), custom)?;
                Ok::<_, String>((candidate, reply))
            })();
            let (candidate, mut reply) = match attempted {
                Ok(attempted) => attempted,
                Err(error) if removes => {
                    return Err(match self.restore() {
                        Ok(()) => error,
                        Err(restore) => format!("{error}; custom dictionary rollback failed: {restore}"),
                    });
                }
                Err(error) => return Err(error),
            };
            if is_true(&reply, "ok") {
                if let Some(candidate) = candidate {
                    self.publish(candidate);
                    self.cleanup(&dropped.iter().map(String::as_str).collect::<Vec<_>>());
                }
                reply.extend(object(json!({ "rebuilt": false, "removed": removes })));
                return Ok(reply);
            }
            if retry(&reply, attempt) {
                snapshot = snapshot_from(&reply)?;
                continue;
            }
            if removes {
                self.restore()?;
            }
            return Ok(reply);
        }
        Err("the dictionary state kept changing while the custom dictionary was removed".to_owned())
    }

    /// The custom archive, compiled into generation `root`.
    fn import_custom(&mut self, archive: &[u8], root: &str) -> Result<Map<String, Value>> {
        let zip = self.engine.store.download_file(root);
        let staged = fs::write(&zip, archive)
            .map_err(|error| import_error(CUSTOM_ARCHIVE, &error.to_string(), "staging the archive"))
            .and_then(|()| self.import_archive(&zip, root, CUSTOM_ARCHIVE));
        let _ = fs::remove_file(&zip);
        staged.map_err(|error| error.message)
    }

    /// `importDictionaryArchive`: the archive at `zip` (`label` names it) compiled
    /// into generation `root`, from an empty engine. Returns the import report, which
    /// says whether it worked.
    pub(super) fn import_archive(&mut self, zip: &Path, root: &str, label: &str) -> std::result::Result<Map<String, Value>, ImportError> {
        let store = &self.engine.store;
        let failed = |phase: &str, error: &dyn std::fmt::Display| import_error(label, &error.to_string(), phase);
        // resetEngine(): the import starts from an empty engine.
        self.native.reset();
        let staging = store.staging_dir(root).map_err(|error| failed("staging the archive", &error))?;
        let raw = self.native.import(&zip.to_string_lossy(), &staging.to_string_lossy());
        let raw: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        let mut report = normalise_report(&raw);
        let title = text(report.get("title")).to_owned();
        if is_true(&report, "success") && title.is_empty() {
            report.insert("success".to_owned(), Value::Bool(false));
            report.insert("error".to_owned(), Value::from(format!("{label} declares no dictionary title")));
        }
        if !is_true(&report, "success") {
            let detail = text(report.get("error"));
            let detail = if detail.is_empty() { "The engine returned no import error." } else { detail };
            report.insert("error".to_owned(), Value::from(import_error(label, detail, "compiling dictionary banks").message));
            let _ = fs::remove_dir_all(&staging);
            return Ok(report);
        }
        let path = format!("{root}/{}", package::folder_name(&title));
        let staged = staging.join(package::folder_name(&title));
        if let Err(error) = frequency::correct_rank_frequency_mode(zip, &staged) {
            let _ = fs::remove_dir_all(&staging);
            return Err(failed("checking the frequency mode", &error));
        }
        let installed = store.commit_dictionary(&path, &staged);
        let _ = fs::remove_dir_all(&staging);
        installed.map_err(|error| failed("compiling dictionary banks", &error))?;
        Ok(report)
    }

    /// `rollbackImportedGeneration`: drops a generation that was not committed.
    fn discard(&mut self, root: &str, identity: &Identity) {
        let _ = self.restore();
        self.cleanup(&[&format!("{root}/{}", package::folder_name(&identity.title))]);
    }

    /// `commitCustomGeneration`: the freshly compiled package, committed with its source.
    fn commit_custom_generation(
        &mut self,
        initial: Snapshot,
        source: &str,
        semantic: &str,
        root: &str,
        report: &Map<String, Value>,
        custom: &Custom,
    ) -> std::result::Result<Map<String, Value>, Refused> {
        let identity = &custom.identity;
        let title = text(report.get("title"));
        let path = format!("{root}/{}", package::folder_name(title));
        let generated = package::from_index(&self.engine.store.dictionary_dir(&path), &path)?;
        if title != identity.title || text(generated.get("title")) != identity.title {
            return Err(Refused::Failed("the compiled custom archive has the wrong dictionary title".to_owned()));
        }
        if text(generated.get("revision")) != semantic {
            return Err(Refused::Failed("the compiled custom archive has the wrong semantic revision".to_owned()));
        }
        let base = initial.revision().cloned();
        let mut snapshot = initial;
        for attempt in 0..STORAGE_ATTEMPTS {
            if snapshot.revision() != base.as_ref() {
                return Err(Refused::Rejected(stale_reply(snapshot)));
            }
            let stored = list(js::property(Some(&snapshot.state), "dictionaries")?);
            let dropped: Vec<String> =
                stored.iter().filter(|entry| is_personal(entry, identity)).map(|entry| text(entry.get("path")).to_owned()).collect();
            let dictionaries = with_custom(stored, &generated, identity, snapshot.state.get("stored").is_some())?;
            let candidate = self.preload(&dictionaries, &paths(stored))?;
            let reply = self.commit_custom(&snapshot, source, semantic, Some(&dictionaries), custom)?;
            if is_true(&reply, "ok") {
                self.publish(candidate);
                self.cleanup(&dropped.iter().map(String::as_str).collect::<Vec<_>>());
                return Ok(reply);
            }
            if retry(&reply, attempt) {
                snapshot = snapshot_from(&reply)?;
                continue;
            }
            return Err(Refused::Rejected(reply));
        }
        Err(Refused::Failed("the dictionary state kept changing while the custom dictionary was saved".to_owned()))
    }
}

/// `assertLegacyRemoval`: an `hd_remove` that names no profile comes from an older
/// client; it may not delete a package that a profile other than Japanese uses.
fn assert_legacy_removal(state: &Value, message: &Value, target: &Value) -> Result<()> {
    let Some(stored) = state.get("stored") else { return Ok(()) };
    if message.get("profileId").is_some() {
        return Ok(());
    }
    let user = stored["profiles"].as_object().into_iter().flatten().find(|(id, profile)| {
        id.as_str() != JAPANESE && list(profile.get("dictionaries")).iter().any(|entry| strict_eq(entry.get("id"), target.get("id")))
    });
    match user {
        Some((id, _)) => Err(format!("This dictionary is used by {} on the linked Hachidori. Remove it there.", registry::name(id))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_keep_installed_dictionaries_once() {
        let dictionaries = [json!({ "id": "a" }), json!({ "id": "b" })];
        let groups = json!([{ "id": "g", "name": "G", "dictionaryIds": ["a", "x", "b", "a"] }, { "id": "h" }]);
        assert_eq!(
            groups_for(Some(&groups), &dictionaries),
            json!([{ "id": "g", "name": "G", "dictionaryIds": ["a", "b"] }, { "id": "h", "dictionaryIds": [] }])
        );
        assert_eq!(groups_for(None, &dictionaries), json!([]));
    }

    #[test]
    fn the_custom_package_goes_first_with_its_presentation_kept() {
        let stored = [json!({ "id": "a", "title": "A" }), json!({ "id": CUSTOM_DICTIONARY_ID, "displayName": "Mine", "favorite": true })];
        let japanese = custom::japanese();
        let generated = json!({ "id": "x", "title": japanese.title, "termCount": 2, "isUpdatable": true });
        let with = with_custom(&stored, &generated, &japanese, false).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(with.len(), 2);
        assert_eq!(with[0]["id"], CUSTOM_DICTIONARY_ID);
        assert_eq!(with[0]["displayName"], "Mine");
        assert_eq!(with[0]["favorite"], true);
        assert_eq!(with[0]["isUpdatable"], false);
        assert_eq!(with[0]["termCount"], 2);
        assert_eq!(with[1]["id"], "a");
        let clash = [json!({ "id": "a", "title": japanese.title })];
        assert!(with_custom(&clash, &generated, &japanese, false).is_err());
    }

    #[test]
    fn a_union_view_keeps_each_personal_package_in_place() {
        let english = custom::identity("en").unwrap_or_else(|| panic!("English has a personal dictionary"));
        let stored = [
            json!({ "id": "a", "title": "A" }),
            json!({ "id": english.id, "title": english.title, "favorite": true }),
            json!({ "id": "b" }),
        ];
        let generated = json!({ "id": "x", "title": english.title, "termCount": 1 });
        let with = with_custom(&stored, &generated, &english, true).unwrap_or_else(|error| panic!("{error}"));
        let ids: Vec<&Value> = with.iter().map(|row| &row["id"]).collect();
        assert_eq!(ids, [&json!("a"), &json!(english.id), &json!("b")]);
        assert_eq!(with[1]["favorite"], true);
        assert_eq!(with[1]["termCount"], 1);
        // A first save of the language's package still goes first.
        let fresh =
            with_custom(&[json!({ "id": "a", "title": "A" })], &generated, &english, true).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(fresh[0]["id"], english.id.as_str());
    }

    #[test]
    fn a_coded_error_keeps_its_code_through_the_import_wording() {
        let error = coded(PROFILE_REMOVED, "profile-missing");
        assert_eq!(split_code(&error), (PROFILE_REMOVED, Some("profile-missing")));
        assert_eq!(split_code("plain"), ("plain", None));
        let import = import_error("a.zip", &error, "importing the dictionary");
        assert_eq!(import.code, "profile-missing");
        assert_eq!(import.message, format!("a.zip: importing the dictionary failed. {PROFILE_REMOVED}"));
    }

    #[test]
    fn the_engine_reads_a_schema_2_record_as_one_list() {
        let stored = json!({ "schemaVersion": 2, "revision": 4,
            "dictionaries": [{ "id": "a", "title": "A", "path": "g/A" }, { "id": "b", "title": "B", "path": "g/B" }],
            "profiles": { "ja": { "dictionaries": [{ "id": "a", "enabled": true }], "groups": [] },
                "en": { "dictionaries": [{ "id": "b", "enabled": false }], "groups": [] } } });
        let view = engine_state(stored.clone());
        assert_eq!(view["schemaVersion"], 1);
        assert_eq!(view["revision"], 4);
        assert_eq!(view["stored"], stored);
        assert_eq!(view["dictionaries"].as_array().map(Vec::len), Some(2));
        assert_eq!(public_state(&view), &stored);
        let single = json!({ "schemaVersion": 1, "revision": 1, "dictionaries": [] });
        assert_eq!(engine_state(single.clone()), single);
        assert_eq!(public_state(&single), &single);
    }

    #[test]
    fn an_import_for_a_profile_joins_the_packages_at_its_path() {
        let next = [json!({ "id": "a", "path": "g1/A" }), json!({ "id": "b", "path": "g2/B" })];
        assert_eq!(joined_ids(&next, Some("g2/B")), [json!("b")]);
        assert_eq!(joined_ids(&next, None), Vec::<Value>::new());
        // The single-profile shape has only the Japanese profile.
        let single = json!({ "schemaVersion": 1, "revision": 0, "dictionaries": [] });
        assert!(assert_profile_exists(&single, "ja").is_ok());
        assert_eq!(assert_profile_exists(&single, "en").err(), Some(coded(PROFILE_REMOVED, "profile-missing")));
        let view = engine_state(json!({ "schemaVersion": 2, "revision": 0, "dictionaries": [], "profiles": {} }));
        assert!(assert_profile_exists(&view, "en").is_ok());
    }

    #[test]
    fn an_older_client_may_not_remove_a_package_another_language_uses() {
        let stored = json!({ "schemaVersion": 2, "revision": 1,
            "dictionaries": [{ "id": "a", "title": "A", "path": "g/A" }, { "id": "b", "title": "B", "path": "g/B" }],
            "profiles": { "ja": { "dictionaries": [{ "id": "a", "enabled": true }], "groups": [] },
                "en": { "dictionaries": [{ "id": "a", "enabled": false }, { "id": "b", "enabled": true }], "groups": [] } } });
        let view = engine_state(stored);
        let target = json!({ "id": "a" });
        assert_eq!(
            assert_legacy_removal(&view, &json!({ "type": "hd_remove" }), &target).err().as_deref(),
            Some("This dictionary is used by English on the linked Hachidori. Remove it there.")
        );
        assert!(assert_legacy_removal(&view, &json!({ "profileId": "ja" }), &target).is_ok());
        let single = json!({ "schemaVersion": 1, "revision": 1, "dictionaries": [{ "id": "a" }] });
        assert!(assert_legacy_removal(&single, &json!({}), &target).is_ok());
    }

    #[test]
    fn import_errors_read_as_the_extension_words_them() {
        let message = |detail: &str| import_error(CUSTOM_ARCHIVE, detail, "x").message;
        assert_eq!(
            import_error(CUSTOM_ARCHIVE, "bad zip", "compiling dictionary banks").message,
            "the custom dictionary archive: compiling dictionary banks failed. bad zip"
        );
        assert!(message("").contains("stopped without an error description"));
        assert!(message("std::bad_alloc").contains("could not allocate enough memory"));
        assert_eq!(import_error("a.zip", "std::bad_alloc", "x").code, "import-memory");
        assert_eq!(import_error("a.zip", "No space left on device", "x").code, "import-storage-full");
        assert_eq!(message("the custom dictionary archive: done"), "the custom dictionary archive: done");
    }
}
