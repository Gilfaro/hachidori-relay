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
use super::{Candidate, Engine, Loaded, envelope, failure_reply, object, package, text};
use crate::host::js::{self, strict_eq};
use crate::host::settings::custom::{self, CUSTOM_DICTIONARY_ID, CUSTOM_DICTIONARY_TITLE};
use crate::host::settings::worker::{self, Storage};
use crate::host::store;

/// The engine requests that change the dictionaries.
pub(crate) const MUTATIONS: [&str; 5] = ["hd_apply_state", "hd_reload", "hd_remove", "hd_custom_save", "hd_custom_append"];
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

fn is_custom(dictionary: &Value) -> bool {
    dictionary.get("id").and_then(Value::as_str) == Some(CUSTOM_DICTIONARY_ID)
}

/// `customStaleReply`.
fn stale_reply(snapshot: Snapshot) -> Map<String, Value> {
    let mut reply = object(json!({ "ok": false, "stale": true, "error": "the custom dictionary source changed while it was being saved" }));
    reply.extend(snapshot.into_fields());
    reply
}

/// `customSnapshotFromReply`.
fn snapshot_from(reply: &Map<String, Value>) -> Result<Snapshot> {
    Ok(Snapshot { document: custom::normalise_document(reply.get("document"))?, state: reply.get("state").cloned().unwrap_or(Value::Null) })
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

/// `withCustomDictionary`: `stored` with `generated` as its managed custom package, first.
fn with_custom(stored: &[Value], generated: &Value) -> Result<Vec<Value>> {
    if stored.iter().any(|dictionary| !is_custom(dictionary) && text(dictionary.get("title")) == CUSTOM_DICTIONARY_TITLE) {
        return Err(format!("a dictionary named {CUSTOM_DICTIONARY_TITLE} is already installed"));
    }
    let current = stored.iter().find(|dictionary| is_custom(dictionary));
    let mut entry = generated.as_object().cloned().unwrap_or_default();
    let display_name = current.and_then(|current| current.get("displayName")).filter(|name| name.is_string());
    entry.extend(object(json!({
        "id": CUSTOM_DICTIONARY_ID,
        "title": CUSTOM_DICTIONARY_TITLE,
        "displayName": display_name.cloned().unwrap_or(Value::Null),
        "enabled": true,
        "favorite": current.and_then(|current| current.get("favorite")) == Some(&Value::Bool(true)),
        "isUpdatable": false,
        "indexUrl": null,
        "downloadUrl": null,
        "lastUpdateCheck": null,
    })));
    Ok(std::iter::once(Value::Object(entry)).chain(stored.iter().filter(|dictionary| !is_custom(dictionary)).cloned()).collect())
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
    let memory = memory_error(detail);
    if detail.starts_with(&format!("{label}: ")) {
        return ImportError { message: detail.to_owned(), code: if memory { "import-memory" } else { "import-failed" } };
    }
    let lower = detail.to_lowercase();
    let generic =
        detail.is_empty() || ["exception", "unknown error", "fs error", "undefined", "null", "[object object]"].contains(&lower.as_str());
    let mut code = "import-failed";
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
            result.map(|payload| (payload, loaded.generation)).map_err(|error| failure_reply(kind, request_id, &error, &loaded))
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
    let mut reply = object(worker::handle(storage, &Value::Object(message), &[]));
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

    /// `readCustomStorage`.
    fn custom_storage(&self) -> Result<Snapshot> {
        let reply = self.ask("hd_custom_read", json!({}));
        if !is_true(&reply, "ok") {
            return Err(error_or(&reply, "the service worker could not read the custom dictionary"));
        }
        let state = reply.get("state").cloned().unwrap_or(Value::Null);
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

    /// `commitDictionaryState`.
    pub(super) fn commit_state(&self, base_revision: &Value, dictionaries: &[Value]) -> Map<String, Value> {
        self.ask("hd_state_cas", json!({ "baseRevision": base_revision, "dictionaries": dictionaries }))
    }

    fn apply_state(&mut self, message: &Value) -> Result<Map<String, Value>> {
        let base = message.get("baseRevision").filter(|base| js::whole(Some(base)).is_some());
        let Some(base) = base else { return Err("the dictionary state change carried no valid base revision".to_owned()) };
        let Some(dictionaries) = message.get("dictionaries").and_then(Value::as_array) else {
            return Err("the dictionary state change carried no dictionary list".to_owned());
        };
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
        let mut reply = self.commit_state(base, dictionaries);
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
        if is_custom(target) {
            return Err("the managed custom dictionary can only be removed by saving an empty source".to_owned());
        }
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
        let mut reply = self.commit_state(&state["revision"], &remaining);
        if is_true(&reply, "ok") {
            self.publish(candidate);
            self.cleanup(&[text(target.get("path"))]);
            return Ok(Map::new());
        }
        let error = error_or(&reply, "the dictionary removal could not be saved");
        if let Err(restore) = self.restore() {
            return Err(format!("{error}; removal rollback failed: {restore}"));
        }
        Ok(object(json!({
            "ok": false, "conflict": is_true(&reply, "conflict"), "error": error,
            "state": reply.remove("state").unwrap_or(Value::Null),
        })))
    }

    fn custom_save(&mut self, message: &Value) -> Result<Map<String, Value>> {
        let base = message.get("baseDocumentRevision").and_then(|base| js::whole(Some(base)));
        let Some(base) = base else { return Err("the custom dictionary save carried no valid document revision".to_owned()) };
        let Some(source) = message.get("text").and_then(Value::as_str) else {
            return Err("the custom dictionary save carried no source text".to_owned());
        };
        let snapshot = self.custom_storage()?;
        if snapshot.revision().and_then(Value::as_f64) != Some(base) {
            return Ok(stale_reply(snapshot));
        }
        self.save_custom(snapshot, source)
    }

    fn custom_append(&mut self, message: &Value) -> Result<Map<String, Value>> {
        let snapshot = self.custom_storage()?;
        let source = custom::append(text(snapshot.document.get("text")), message.get("entry"))?;
        self.save_custom(snapshot, &source)
    }

    /// `saveCustomDictionary`.
    fn save_custom(&mut self, snapshot: Snapshot, source: &str) -> Result<Map<String, Value>> {
        let (entries, errors) = custom::parse(source);
        let semantic = custom::semantic_revision(&entries)?;
        let dictionaries = list(js::property(Some(&snapshot.state), "dictionaries")?).to_vec();
        let with_errors = |mut reply: Map<String, Value>, rest: Value| {
            reply.insert("errors".to_owned(), Value::Array(errors.clone()));
            reply.extend(object(rest));
            reply
        };
        if entries.is_empty() {
            return Ok(with_errors(self.remove_custom(snapshot, source, &semantic)?, json!({})));
        }
        if dictionaries
            .iter()
            .any(|dictionary| !is_custom(dictionary) && dictionary.get("title").and_then(Value::as_str) == Some(CUSTOM_DICTIONARY_TITLE))
        {
            return Err(format!("a dictionary named {CUSTOM_DICTIONARY_TITLE} is already installed"));
        }
        if snapshot.document.get("semanticRevision").and_then(Value::as_str) == Some(semantic.as_str())
            && self.custom_package_satisfies(&snapshot.state, &semantic, entries.len())
        {
            let saved = self.save_custom_source(snapshot, source, &semantic)?;
            return Ok(with_errors(saved, json!({ "rebuilt": false, "removed": false })));
        }
        let archive = custom::build_zip(&entries, &semantic)?;
        let root = package::new_generation_root()?;
        let report = self.import_custom(&archive, &root)?;
        if !is_true(&report, "success") {
            self.discard(&root);
            return Ok(with_errors(Map::new(), json!({ "rebuilt": true, "removed": false, "report": report })));
        }
        match self.commit_custom_generation(snapshot, source, &semantic, &root, &report) {
            Ok(committed) => Ok(with_errors(committed, json!({ "rebuilt": true, "removed": false, "report": report }))),
            Err(refused) => {
                self.discard(&root);
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
    ) -> Result<Map<String, Value>> {
        if snapshot.state.is_null() {
            return Err("the dictionary state is unavailable".to_owned());
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

    /// `customPackageSatisfies`: whether the committed custom package already holds
    /// these entries, and loads.
    fn custom_package_satisfies(&mut self, state: &Value, semantic: &str, entry_count: usize) -> bool {
        let dictionaries = list(state.get("dictionaries"));
        let Some(custom) = dictionaries.first() else { return false };
        if !is_custom(custom)
            || custom.get("title").and_then(Value::as_str) != Some(CUSTOM_DICTIONARY_TITLE)
            || custom.get("enabled") != Some(&Value::Bool(true))
            || custom.get("revision").and_then(Value::as_str) != Some(semantic)
            || !strict_eq(custom.get("termCount"), Some(&json!(entry_count)))
            || !package::in_generation(custom)
        {
            return false;
        }
        let path = text(custom.get("path"));
        let Ok(generated) = package::from_index(&self.engine.store.dictionary_dir(path), path) else { return false };
        if !custom::metadata_matches(&generated, semantic, entry_count) {
            return false;
        }
        let Ok(with) = with_custom(dictionaries, &generated) else { return false };
        if !js::same_json(&Value::Array(dictionaries.to_vec()), &Value::Array(with)) {
            return false;
        }
        let others = paths(&dictionaries[1..]);
        let Ok(candidate) = self.preload(dictionaries, &others) else { return false };
        // Loaded, not published: the committed state is unchanged.
        let stored = self.engine.store.get(store::DICTIONARY_STATE).unwrap_or(Value::Null);
        let publish = self.loaded.state.as_ref() != Some(&stored);
        self.engine.install(self.native, self.loaded, candidate, stored, publish);
        true
    }

    /// `commitCustomSourceOnly`: a source change that keeps the package's entries.
    fn save_custom_source(&self, initial: Snapshot, source: &str, semantic: &str) -> Result<Map<String, Value>> {
        let base = initial.revision().cloned();
        let mut snapshot = initial;
        for _ in 0..STORAGE_ATTEMPTS {
            if snapshot.revision() != base.as_ref() {
                return Ok(stale_reply(snapshot));
            }
            let reply = self.commit_custom(&snapshot, source, semantic, None)?;
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
    fn remove_custom(&mut self, initial: Snapshot, source: &str, semantic: &str) -> Result<Map<String, Value>> {
        let base = initial.revision().cloned();
        let mut snapshot = initial;
        for attempt in 0..STORAGE_ATTEMPTS {
            if snapshot.revision() != base.as_ref() {
                return Ok(stale_reply(snapshot));
            }
            let current = list(js::property(Some(&snapshot.state), "dictionaries")?).to_vec();
            let dropped: Vec<String> =
                current.iter().filter(|entry| is_custom(entry)).map(|entry| text(entry.get("path")).to_owned()).collect();
            let dictionaries: Vec<Value> = current.iter().filter(|entry| !is_custom(entry)).cloned().collect();
            let removes = dictionaries.len() != current.len();
            let attempted = (|| {
                let candidate = if removes { Some(self.preload(&dictionaries, &paths(&dictionaries))?) } else { None };
                let reply = self.commit_custom(&snapshot, source, semantic, removes.then_some(dictionaries.as_slice()))?;
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
        let installed = store.commit_dictionary(&path, &staging.join(package::folder_name(&title)));
        let _ = fs::remove_dir_all(&staging);
        installed.map_err(|error| failed("compiling dictionary banks", &error))?;
        Ok(report)
    }

    /// `rollbackImportedGeneration`: drops a generation that was not committed.
    fn discard(&mut self, root: &str) {
        let _ = self.restore();
        self.cleanup(&[&format!("{root}/{}", package::folder_name(CUSTOM_DICTIONARY_TITLE))]);
    }

    /// `commitCustomGeneration`: the freshly compiled package, committed with its source.
    fn commit_custom_generation(
        &mut self,
        initial: Snapshot,
        source: &str,
        semantic: &str,
        root: &str,
        report: &Map<String, Value>,
    ) -> std::result::Result<Map<String, Value>, Refused> {
        let title = text(report.get("title"));
        let path = format!("{root}/{}", package::folder_name(title));
        let generated = package::from_index(&self.engine.store.dictionary_dir(&path), &path)?;
        if title != CUSTOM_DICTIONARY_TITLE || text(generated.get("title")) != CUSTOM_DICTIONARY_TITLE {
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
                stored.iter().filter(|entry| is_custom(entry)).map(|entry| text(entry.get("path")).to_owned()).collect();
            let dictionaries = with_custom(stored, &generated)?;
            let candidate = self.preload(&dictionaries, &paths(stored))?;
            let reply = self.commit_custom(&snapshot, source, semantic, Some(&dictionaries))?;
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
        let generated = json!({ "id": "x", "title": CUSTOM_DICTIONARY_TITLE, "termCount": 2, "isUpdatable": true });
        let with = with_custom(&stored, &generated).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(with.len(), 2);
        assert_eq!(with[0]["id"], CUSTOM_DICTIONARY_ID);
        assert_eq!(with[0]["displayName"], "Mine");
        assert_eq!(with[0]["favorite"], true);
        assert_eq!(with[0]["isUpdatable"], false);
        assert_eq!(with[0]["termCount"], 2);
        assert_eq!(with[1]["id"], "a");
        let clash = [json!({ "id": "a", "title": CUSTOM_DICTIONARY_TITLE })];
        assert!(with_custom(&clash, &generated).is_err());
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
