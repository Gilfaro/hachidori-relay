// SPDX-License-Identifier: GPL-3.0-or-later
//! Importing a Yomitan archive a linked browser uploaded: the decision
//! `linked-import.js` makes for a remote sender (`uploadImportDecision`, from the
//! archive's `index.json`), then `engine-service.js`' `hd_import` for a local
//! archive with that decision. The archive is compiled into a new generation, which
//! is committed as a new dictionary, as the replacement of the one it matches, or
//! beside it under a numbered title (`commitImportedGeneration`).

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::mutations::{IMPORT_COUNTS, ImportError, Result, STORAGE_ATTEMPTS, Session, error_or, import_error, is_true, list, paths};
use super::{Engine, object, package, text};
use crate::host::js::{self, strict_eq};
use crate::host::settings::custom::CUSTOM_DICTIONARY_TITLE;
use crate::host::settings::worker::Storage;
use crate::host::store;

/// `MAX_PREFLIGHT_INDEX_BYTES`.
const MAX_INDEX_BYTES: u64 = 1024 * 1024;
/// The phase an import's commit fails in.
const IMPORTING: &str = "importing the dictionary";

/// `optionalExactString`: a non-empty string, or `null`.
fn exact(value: Option<&Value>) -> Value {
    value.filter(|value| value.as_str().is_some_and(|text| !text.is_empty())).cloned().unwrap_or(Value::Null)
}

/// `dictionaryArchiveIdentity`.
fn archive_identity(index: &Value) -> Result<Value> {
    let title = index.get("title").and_then(Value::as_str).filter(|title| !title.is_empty());
    let Some(title) = title.filter(|_| index.is_object()) else {
        return Err("The selected archive's index.json has no dictionary title.".to_owned());
    };
    Ok(json!({
        "title": title,
        "revision": index.get("revision").filter(|revision| revision.is_string()).cloned().unwrap_or(Value::Null),
        "indexUrl": exact(index.get("indexUrl")),
        "downloadUrl": exact(index.get("downloadUrl")),
    }))
}

/// `readDictionaryArchiveIdentity`: who the archive at `path` says it is.
fn read_identity(path: &Path) -> Result<Value> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|error| format!("The selected archive is not a ZIP archive: {error}"))?;
    let indexes: Vec<usize> = (0..zip.len())
        .filter(|&index| zip.name_for_index(index).and_then(std::result::Result::ok).as_deref() == Some("index.json"))
        .collect();
    let index = match indexes[..] {
        [index] => index,
        [] => return Err("The selected archive has no index.json.".to_owned()),
        _ => return Err("The selected archive has more than one index.json.".to_owned()),
    };
    {
        let entry = zip.by_index_raw(index).map_err(|error| error.to_string())?;
        let kind = entry.unix_mode().map_or(0, |mode| mode & 0o170_000);
        if entry.is_dir() || entry.is_symlink() || (kind != 0 && kind != 0o100_000) {
            return Err("The selected archive's index.json is not a regular file.".to_owned());
        }
        if entry.encrypted() {
            return Err("The selected archive's index.json is encrypted.".to_owned());
        }
        if entry.size() > MAX_INDEX_BYTES {
            return Err(format!("The selected archive's index.json exceeds {MAX_INDEX_BYTES} bytes."));
        }
    }
    let mut data = Vec::new();
    let entry = zip.by_index(index).map_err(|error| error.to_string())?;
    entry.take(MAX_INDEX_BYTES + 1).read_to_end(&mut data).map_err(|error| error.to_string())?;
    if data.len() as u64 > MAX_INDEX_BYTES {
        return Err(format!("The selected archive's index.json exceeds {MAX_INDEX_BYTES} bytes."));
    }
    let index: Value = serde_json::from_slice(&data).map_err(|error| format!("The selected archive's index.json is malformed: {error}"))?;
    archive_identity(&index)
}

/// `dictionaryImportTarget`.
fn import_target(dictionary: &Value) -> Value {
    let string = |name: &str| dictionary.get(name).filter(|value| value.is_string()).cloned().unwrap_or_else(|| json!(""));
    json!({
        "id": string("id"),
        "title": string("title"),
        "path": string("path"),
        "revision": string("revision"),
        "sourceId": exact(dictionary.get("sourceId")),
        "indexUrl": exact(dictionary.get("indexUrl")),
        "downloadUrl": exact(dictionary.get("downloadUrl")),
        "isUpdatable": dictionary.get("isUpdatable") == Some(&Value::Bool(true)),
    })
}

/// `uploadImportDecision`: a new title installs; an installed one (by title, or else
/// by update source) is replaced, or kept beside the new copy.
fn upload_decision(identity: &Value, dictionaries: &[Value], replace: bool) -> Value {
    let by_title = dictionaries.iter().find(|dictionary| strict_eq(dictionary.get("title"), identity.get("title")));
    let index_url = identity.get("indexUrl").filter(|url| !url.is_null());
    let matched = by_title.map(|dictionary| (dictionary, "title")).or_else(|| {
        let url = index_url?;
        dictionaries.iter().find(|dictionary| exact(dictionary.get("indexUrl")) == *url).map(|dictionary| (dictionary, "source"))
    });
    match matched {
        None => json!({ "action": "install", "identity": identity, "matchKind": null, "target": null }),
        Some((dictionary, kind)) => json!({
            "action": if replace { "replace" } else { "separate" },
            "identity": identity,
            "matchKind": kind,
            "target": import_target(dictionary),
        }),
    }
}

/// What `importDecisionTarget` refuses of a decision made from stored entries.
fn check_decision(decision: &Value) -> Result<()> {
    let target = &decision["target"];
    let filled = |name: &str| target.get(name).and_then(Value::as_str).is_some_and(|text| !text.is_empty());
    if !(target.is_null() || filled("id") && filled("title") && filled("path")) {
        return Err("the import decision carried an invalid replacement target".to_owned());
    }
    Ok(())
}

/// `emptyReport`.
pub(super) fn empty_report(error: &str) -> Value {
    let mut report = object(json!({ "success": false, "title": "" }));
    for key in IMPORT_COUNTS {
        report.insert(key.to_owned(), json!(0));
    }
    report.insert("error".to_owned(), Value::from(error));
    Value::Object(report)
}

/// `sameImportedIdentity`.
fn same_identity(left: &Value, right: &Value) -> bool {
    ["title", "revision", "indexUrl", "downloadUrl"].iter().all(|name| strict_eq(left.get(*name), right.get(*name)))
}

/// `storedImportIdentityIndex`.
fn stored_identity_index(stored: &[Value], identity: &Value) -> Option<usize> {
    let by_title = stored.iter().position(|dictionary| strict_eq(dictionary.get("title"), identity.get("title")));
    let url = identity.get("indexUrl").filter(|url| !url.is_null());
    match (by_title, url) {
        (Some(index), _) => Some(index),
        (None, None) => None,
        (None, Some(url)) => stored.iter().position(|dictionary| exact(dictionary.get("indexUrl")) == *url),
    }
}

/// `interactiveTargetIndex`: where the chosen dictionary still is, unchanged.
fn target_index(stored: &[Value], decision: &Value) -> Result<usize> {
    let (target, identity) = (&decision["target"], &decision["identity"]);
    let index = stored.iter().position(|dictionary| strict_eq(dictionary.get("id"), target.get("id")));
    let Some(index) = index.filter(|&index| js::same_json(&import_target(&stored[index]), target)) else {
        return Err("the chosen dictionary changed while the archive was being imported".to_owned());
    };
    let existing = &stored[index];
    if decision["matchKind"] == "title" {
        if !strict_eq(existing.get("title"), identity.get("title")) {
            return Err("the chosen dictionary no longer matches the imported title".to_owned());
        }
    } else if identity["indexUrl"].is_null()
        || exact(existing.get("indexUrl")) != identity["indexUrl"]
        || stored.iter().any(|dictionary| strict_eq(dictionary.get("title"), identity.get("title")))
    {
        return Err("the chosen dictionary no longer matches the imported source".to_owned());
    }
    Ok(index)
}

/// `optionalText`: a non-empty string, or `null`.
pub(super) fn optional_text(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| value.as_str().is_some_and(|text| !text.is_empty()))
}

/// `value ?? fallback`.
fn or_else<'a>(value: Option<&'a Value>, fallback: Option<&'a Value>) -> Value {
    value.filter(|value| !value.is_null()).or(fallback).cloned().unwrap_or(Value::Null)
}

/// `withStoredPresentation`: the new package, keeping how the stored one was shown.
pub(super) fn with_stored_presentation(generated: &Value, stored: &Value) -> Map<String, Value> {
    let mut entry = stored.as_object().cloned().unwrap_or_default();
    entry.extend(generated.as_object().cloned().unwrap_or_default());
    let field = |name: &str| stored.get(name);
    entry.insert("id".to_owned(), or_else(optional_text(field("id")), generated.get("id")));
    entry.insert("displayName".to_owned(), field("displayName").filter(|name| name.is_string()).cloned().unwrap_or(Value::Null));
    entry.insert("enabled".to_owned(), Value::Bool(field("enabled") != Some(&Value::Bool(false))));
    entry.insert("favorite".to_owned(), Value::Bool(field("favorite") == Some(&Value::Bool(true))));
    let updatable = field("isUpdatable") == Some(&Value::Bool(true)) || generated.get("isUpdatable") == Some(&Value::Bool(true));
    entry.insert("isUpdatable".to_owned(), Value::Bool(updatable));
    entry.insert("indexUrl".to_owned(), or_else(field("indexUrl"), generated.get("indexUrl")));
    entry.insert("downloadUrl".to_owned(), or_else(field("downloadUrl"), generated.get("downloadUrl")));
    entry.insert("lastUpdateCheck".to_owned(), or_else(field("lastUpdateCheck"), None));
    if let Some(schedule) = field("updateScheduleOverride") {
        entry.insert("updateScheduleOverride".to_owned(), schedule.clone());
    }
    if let Some(source) = optional_text(field("sourceId")) {
        entry.insert("sourceId".to_owned(), source.clone());
    }
    entry
}

/// `withInteractiveReplacement`: `stored` with the package at `index` replaced.
fn with_replacement(stored: &[Value], generated: &Value, index: usize) -> Result<Vec<Value>> {
    let current = &stored[index];
    let collision = stored.iter().enumerate().any(|(other, dictionary)| {
        other != index && (strict_eq(dictionary.get("id"), current.get("id")) || strict_eq(dictionary.get("title"), generated.get("title")))
    });
    if collision {
        return Err(format!("a dictionary named {} is already installed", text(generated.get("title"))));
    }
    let mut replacement = with_stored_presentation(generated, current);
    // A local archive cannot acquire or redirect update ownership while replacing a
    // package: the target's source contract is kept verbatim.
    replacement.insert("isUpdatable".to_owned(), Value::Bool(current.get("isUpdatable") == Some(&Value::Bool(true))));
    replacement.insert("indexUrl".to_owned(), optional_text(current.get("indexUrl")).cloned().unwrap_or(Value::Null));
    replacement.insert("downloadUrl".to_owned(), optional_text(current.get("downloadUrl")).cloned().unwrap_or(Value::Null));
    replacement.insert("lastUpdateCheck".to_owned(), Value::Null);
    if current.get("sourceId").is_none() {
        replacement.remove("sourceId");
    }
    let mut next = stored.to_vec();
    next[index] = Value::Object(replacement);
    Ok(next)
}

/// `separateDictionaryTitle`: `title (2)`, `title (3)`, ... whichever is free first.
fn separate_title(title: &str, stored: &[Value]) -> String {
    let taken = |name: &str, value: &str| stored.iter().any(|dictionary| dictionary.get(name).and_then(Value::as_str) == Some(value));
    #[allow(clippy::maybe_infinite_iter, reason = "only finitely many titles are taken")]
    (2_u64..)
        .map(|suffix| format!("{title} ({suffix})"))
        .find(|candidate| package::usable_title(candidate) && !taken("title", candidate) && !taken("id", &package::stable_id(candidate)))
        .unwrap_or_default()
}

/// The `index.json` of the package installed in `dir`.
fn read_index(dir: &Path) -> Result<Value> {
    let index = fs::read_to_string(dir.join("index.json")).map_err(|error| error.to_string())?;
    serde_json::from_str(&index).map_err(|error| error.to_string())
}

impl Engine {
    /// `importUpload`: the uploaded archive at `archive`, imported as `hd_import`
    /// imports a local file with the decision a remote sender gets. An archive whose
    /// identity cannot be read fails before the engine is asked; the caller holds the
    /// storage lock.
    pub(crate) fn import_upload(
        &self,
        storage: &impl Storage,
        archive: &Path,
        file_name: &str,
        replace: bool,
        request_id: &Value,
    ) -> Result<Value> {
        let identity = read_identity(archive)?;
        let state = storage.read(store::DICTIONARY_STATE).unwrap_or(Value::Null);
        let decision = upload_decision(&identity, list(state.get("dictionaries")), replace);
        Ok(self.run(storage, "hd_import", request_id, |session| Ok(session.import(archive, file_name, &decision))))
    }
}

impl<S: Storage> Session<'_, S> {
    /// `hd_import` for a local archive with an interactive `decision`. Every failure is
    /// a reply: `{ok: false, error, errorCode, report}`.
    fn import(&mut self, archive: &Path, file_name: &str, decision: &Value) -> Map<String, Value> {
        let fail = |error: ImportError| {
            let report = empty_report(&error.message);
            object(json!({ "ok": false, "error": error.message, "errorCode": error.code, "report": report }))
        };
        if let Err(error) = check_decision(decision) {
            return fail(import_error(file_name, &error, "checking the import request"));
        }
        let root = match package::new_generation_root() {
            Ok(root) => root,
            Err(error) => return fail(import_error(file_name, &error, IMPORTING)),
        };
        let mut report = match self.import_archive(archive, &root, file_name) {
            Ok(report) => report,
            Err(error) => {
                let _ = self.restore();
                return fail(error);
            }
        };
        if !is_true(&report, "success") {
            let _ = self.restore();
            // `finishImportOutcome`: the report's error is already about this archive.
            let error = import_error(file_name, text(report.get("error")), IMPORTING);
            report.insert("error".to_owned(), Value::from(error.message.as_str()));
            report.insert("errorCode".to_owned(), Value::from(error.code));
            return object(json!({ "ok": false, "error": error.message, "report": report, "errorCode": error.code }));
        }
        let mut path = format!("{root}/{}", package::folder_name(text(report.get("title"))));
        match self.commit_import(&mut path, &mut report, decision) {
            Ok(()) => object(json!({ "report": report })),
            Err(error) => {
                // `rollbackImportedGeneration`.
                let _ = self.restore();
                self.cleanup(&[&path]);
                fail(import_error(file_name, &error, IMPORTING))
            }
        }
    }

    /// `commitImportedGeneration` for an interactive decision. `path` follows the
    /// package when a separate copy is retitled.
    fn commit_import(&mut self, path: &mut String, report: &mut Map<String, Value>, decision: &Value) -> Result<()> {
        let title = text(report.get("title")).to_owned();
        let root = path[..path.len() - package::folder_name(&title).len() - 1].to_owned();
        let store = self.engine.store.clone();
        let mut generated = package::from_index(&store.dictionary_dir(path), path)?;
        if text(generated.get("title")) != title {
            return Err("the imported dictionary title changed while it was being committed".to_owned());
        }
        let identity = &decision["identity"];
        if !same_identity(&archive_identity(&read_index(&store.dictionary_dir(path))?)?, identity) {
            return Err("the imported dictionary metadata did not match the reviewed archive".to_owned());
        }
        if title == CUSTOM_DICTIONARY_TITLE {
            return Err(format!("{CUSTOM_DICTIONARY_TITLE} is reserved for the managed custom dictionary"));
        }
        let mut candidate_title = title;
        for attempt in 0..STORAGE_ATTEMPTS {
            let state = self.dictionary_state()?;
            let stored = list(state.get("dictionaries")).to_vec();
            let next = match decision["action"].as_str() {
                Some("install") => {
                    if stored_identity_index(&stored, identity).is_some() {
                        return Err("the dictionary library changed; review this import again".to_owned());
                    }
                    [stored.clone(), vec![generated.clone()]].concat()
                }
                Some("replace") => with_replacement(&stored, &generated, target_index(&stored, decision)?)?,
                _ => {
                    let next_title = separate_title(text(identity.get("title")), &stored);
                    if candidate_title != next_title {
                        *path = self.retitle(&root, &candidate_title, &next_title)?;
                        candidate_title = next_title;
                        generated = package::from_index(&store.dictionary_dir(path), path)?;
                    }
                    [stored.clone(), vec![generated.clone()]].concat()
                }
            };
            let candidate = self.preload(&next, &paths(&stored))?;
            let base = state.get("revision").filter(|revision| !revision.is_null()).cloned().unwrap_or(json!(0));
            let reply = self.commit_state(&base, &next);
            if is_true(&reply, "ok") {
                let committed = reply.get("state");
                if !strict_eq(committed.and_then(|state| state.get("schemaVersion")), Some(&json!(1)))
                    || js::whole(committed.and_then(|state| state.get("revision"))).is_none()
                    || !committed.and_then(|state| state.get("dictionaries")).is_some_and(Value::is_array)
                {
                    return Err("the service worker returned invalid committed dictionary state".to_owned());
                }
                self.publish(candidate);
                report.insert("title".to_owned(), generated.get("title").cloned().unwrap_or(Value::Null));
                let kept = paths(&next);
                let dropped: Vec<&str> = paths(&stored).into_iter().filter(|stored| !kept.contains(stored)).collect();
                self.cleanup(&dropped);
                return Ok(());
            }
            if !is_true(&reply, "conflict") || attempt + 1 >= STORAGE_ATTEMPTS {
                return Err(error_or(&reply, "the service worker could not save dictionary state"));
            }
        }
        Err("the service worker could not save dictionary state".to_owned())
    }

    /// `retitleImportedPackage`: the staged package moved to `next`'s folder, with its
    /// index retitled. Returns its new path.
    fn retitle(&self, root: &str, current: &str, next: &str) -> Result<String> {
        if !package::usable_title(next) {
            return Err("the separate dictionary title cannot be used as a filesystem path".to_owned());
        }
        let store = &self.engine.store;
        let (current_path, next_path) =
            (format!("{root}/{}", package::folder_name(current)), format!("{root}/{}", package::folder_name(next)));
        if !store.has_dictionary(&current_path) || store.has_dictionary(&next_path) {
            return Err("the staged dictionary title changed unexpectedly".to_owned());
        }
        let mut index = read_index(&store.dictionary_dir(&current_path))?;
        if index.get("title").and_then(Value::as_str) != Some(current) {
            return Err("the staged dictionary index does not match its directory".to_owned());
        }
        fs::rename(store.dictionary_dir(&current_path), store.dictionary_dir(&next_path)).map_err(|error| error.to_string())?;
        index["title"] = Value::from(next);
        fs::write(store.dictionary_dir(&next_path).join("index.json"), index.to_string()).map_err(|error| error.to_string())?;
        Ok(next_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, title: &str, index_url: Option<&str>) -> Value {
        json!({ "id": id, "title": title, "path": format!("/dicts/{title}"), "revision": "1", "indexUrl": index_url })
    }

    #[test]
    fn a_remote_sender_installs_replaces_or_keeps_both() {
        let identity = json!({ "title": "JMdict", "revision": "2", "indexUrl": "https://x/index.json", "downloadUrl": null });
        let stored = [entry("a", "Other", Some("https://x/index.json")), entry("b", "JMdict", None)];
        let decision = upload_decision(&identity, &stored, true);
        assert_eq!(decision["action"], "replace");
        assert_eq!(decision["matchKind"], "title");
        assert_eq!(decision["target"]["id"], "b");
        let decision = upload_decision(&identity, &stored[..1], false);
        assert_eq!((decision["action"].as_str(), decision["matchKind"].as_str()), (Some("separate"), Some("source")));
        assert_eq!(upload_decision(&identity, &[], false)["action"], "install");
        assert_eq!(target_index(&stored[..1], &decision).ok(), Some(0));
        let mut moved = stored[..1].to_vec();
        moved[0]["revision"] = json!("9");
        assert!(target_index(&moved, &decision).is_err());
    }

    #[test]
    fn a_separate_copy_takes_the_first_free_number() {
        let stored = [entry("a", "JMdict", None), entry("b", "JMdict (2)", None)];
        assert_eq!(separate_title("JMdict", &stored), "JMdict (3)");
    }

    #[test]
    fn a_replacement_keeps_presentation_and_source() {
        let mut current = entry("old", "JMdict", Some("https://x/index.json"));
        current["displayName"] = json!("Mine");
        current["enabled"] = json!(false);
        current["isUpdatable"] = json!(true);
        let generated = json!({ "id": "new", "title": "JMdict", "path": "/dicts/g/JMdict", "isUpdatable": false, "indexUrl": null });
        let next = with_replacement(&[current], &generated, 0).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(next[0]["id"], "old");
        assert_eq!(next[0]["path"], "/dicts/g/JMdict");
        assert_eq!(next[0]["displayName"], "Mine");
        assert_eq!(next[0]["enabled"], false);
        assert_eq!(next[0]["isUpdatable"], true);
        assert_eq!(next[0]["indexUrl"], "https://x/index.json");
        assert!(next[0].get("sourceId").is_none());
    }

    #[test]
    fn identities_need_a_title() {
        assert!(archive_identity(&json!({ "title": "" })).is_err());
        assert_eq!(
            archive_identity(&json!({ "title": "T", "revision": 3, "indexUrl": "" })).ok(),
            Some(json!({ "title": "T", "revision": null, "indexUrl": null, "downloadUrl": null }))
        );
    }
}
