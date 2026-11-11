// SPDX-License-Identifier: GPL-3.0-or-later
//! `hd_import` from an `archiveUrl` (`engine-service.js`): a checked managed update,
//! or a first install of a recommended source from its catalogue archive. The
//! request is checked against storage, the archive downloaded with no lock held,
//! and the request checked again before the archive is compiled and committed
//! (`commitImportedGeneration` without an interactive decision, `withImport`).

use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use serde_json::{Map, Value, json};

use super::import::{empty_report, optional_text, with_stored_presentation};
use super::mutations::{
    ImportError, Joining, Result, STORAGE_ATTEMPTS, Session, dictionary_state, error_or, import_error, is_true, list, paths,
};
use super::{Engine, failure_reply, object, package, text};
use crate::host::js::{self, strict_eq};
use crate::host::remote::{self, Fetch, MANAGED_DICTIONARY_CHANGED, Recommended};
use crate::host::settings::custom;
use crate::host::settings::worker::Storage;

/// The phases a failure is reported in.
const CHECKING: &str = "checking the import request";
const READING: &str = "reading the archive";
const IMPORTING: &str = "importing the dictionary";
/// The reply to an import while another one runs.
pub(crate) const IMPORT_IN_PROGRESS: &str = "another dictionary import is already in progress";
/// Where the archive waits while it downloads.
const DOWNLOAD_KEY: &str = "remote-import";
/// `PROGRESS_INTERVAL_MS`: the least time between download reports.
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// An import's progress, as the engine reports it to the installer that issued it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Progress {
    /// `{phase: "downloading", receivedBytes, totalBytes}`.
    Downloading { received: u64, total: Option<u64> },
    /// `{phase: "installing"}`: the archive is staged and compiling.
    Installing,
}

/// A managed update's checked source (`managedSourceForImport`).
struct Managed {
    fingerprint: Value,
    archive_url: String,
    checked_at: String,
}

/// `prepareImportRequest` for a remote import.
struct Prepared {
    archive_url: String,
    expected_revision: Option<String>,
    file_name: String,
    managed: Option<Managed>,
    recommended: Option<&'static Recommended>,
}

impl Prepared {
    /// `samePreparedImport`.
    fn same(&self, other: &Self) -> bool {
        let managed = |prepared: &Self| prepared.managed.as_ref().map(|managed| (managed.checked_at.clone(), managed.fingerprint.clone()));
        self.archive_url == other.archive_url
            && self.expected_revision == other.expected_revision
            && self.file_name == other.file_name
            && self.recommended.map(|source| source.source_id) == other.recommended.map(|source| source.source_id)
            && managed(self) == managed(other)
    }
}

/// `optionalText`, as a string.
fn optional(value: Option<&Value>) -> Option<String> {
    optional_text(value).and_then(Value::as_str).map(str::to_owned)
}

/// `recommendedSourceForImport`.
fn recommended_for(message: &Value) -> Result<Option<&'static Recommended>> {
    let (source_id, final_url) = (optional(message.get("sourceId")), optional(message.get("finalUrl")));
    if source_id.is_none() && final_url.is_none() {
        return Ok(None);
    }
    let source = remote::recommended(source_id.as_deref()).ok_or("the recommended import names an unknown catalogue source")?;
    if final_url.is_some() && !remote::download_url_matches(source, final_url.as_deref()) {
        return Err(format!("{} downloaded from an unexpected final URL", source.name));
    }
    Ok(Some(source))
}

/// `managedSourceForImport`.
fn managed_for(message: &Value, stored: &[Value], recommended: Option<&Recommended>) -> Result<Option<Managed>> {
    let requested = match message.get("managedFingerprint") {
        None | Some(Value::Null) => return Ok(None),
        Some(requested) => requested,
    };
    let dictionary = stored.iter().find(|entry| strict_eq(entry.get("id"), requested.get("id")));
    if !remote::matches(dictionary, requested) {
        return Err(MANAGED_DICTIONARY_CHANGED.to_owned());
    }
    let fingerprint = dictionary.and_then(remote::fingerprint).ok_or(MANAGED_DICTIONARY_CHANGED)?;
    let source = &fingerprint["source"];
    let is_recommended = source["kind"] == "recommended";
    if (is_recommended && recommended.map(|source| source.source_id) != source["sourceId"].as_str())
        || (!is_recommended && recommended.is_some())
    {
        return Err(MANAGED_DICTIONARY_CHANGED.to_owned());
    }
    let archive_url = remote::https_url(message.get("archiveUrl"))
        .filter(|url| !is_recommended || source["downloadUrl"].as_str() == Some(url))
        .ok_or("the managed dictionary update carried an invalid archive URL")?;
    let checked_at = optional(message.get("checkedAt")).ok_or("the managed dictionary update carried no check time")?;
    Ok(Some(Managed { fingerprint, archive_url, checked_at }))
}

/// `prepareImportRequest`, for a request with no `blobUrl`.
fn prepare(message: &Value, stored: &[Value]) -> Result<Prepared> {
    let recommended = recommended_for(message)?;
    let managed = managed_for(message, stored, recommended)?;
    let expected_revision = optional(message.get("expectedRevision"));
    // `validateRemoteImportRequest`.
    let archive_url = if let Some(managed) = &managed {
        if expected_revision.is_none() {
            return Err("the managed import request carried no expected revision".to_owned());
        }
        managed.archive_url.clone()
    } else {
        let source = recommended.ok_or("the import request carried no archive URL")?;
        if remote::https_url(message.get("archiveUrl")).as_deref() != Some(source.download_url) {
            return Err(format!("{} must be downloaded from its catalogue archive URL", source.name));
        }
        source.download_url.to_owned()
    };
    if message.get("importDecision").is_some() {
        return Err("only an ordinary local import can carry an interactive decision".to_owned());
    }
    let named = text(message.get("fileName"));
    let file_name = if named.is_empty() { recommended.map_or("the archive", |source| source.archive_name) } else { named }.to_owned();
    // `importResources`: only a local .mdx import carries MDD files.
    match message.get("resources") {
        None => {}
        Some(Value::Array(resources)) if resources.is_empty() => {}
        Some(Value::Array(_)) => return Err("only a local .mdx import can carry resource files".to_owned()),
        Some(_) => return Err("the import request carried an invalid resource list".to_owned()),
    }
    Ok(Prepared { archive_url, expected_revision, file_name, managed, recommended })
}

/// [`prepare`], reading the stored dictionaries only for a managed update
/// (`readStoredDictionaries` in `managedSourceForImport`).
fn prepare_with(message: &Value, read: impl FnOnce() -> Result<Value>) -> Result<Prepared> {
    if matches!(message.get("managedFingerprint"), None | Some(Value::Null)) {
        return prepare(message, &[]);
    }
    let state = read()?;
    prepare(message, list(state.get("dictionaries")))
}

/// `withRecommendedSource`.
fn with_recommended_source(mut dictionary: Map<String, Value>, source: &Recommended) -> Value {
    dictionary.insert("sourceId".to_owned(), Value::from(source.source_id));
    dictionary.insert("isUpdatable".to_owned(), Value::Bool(source.index_url.is_some()));
    dictionary.insert("indexUrl".to_owned(), source.index_url.map_or(Value::Null, Value::from));
    dictionary.insert("downloadUrl".to_owned(), Value::from(source.download_url));
    Value::Object(dictionary)
}

/// `importReplacementIndex`.
fn replacement_index(stored: &[Value], generated: &Value, prepared: &Prepared) -> Result<Option<usize>> {
    if let Some(managed) = &prepared.managed {
        let index = stored.iter().position(|dictionary| strict_eq(dictionary.get("id"), managed.fingerprint.get("id")));
        if !index.is_some_and(|index| remote::matches(Some(&stored[index]), &managed.fingerprint)) {
            return Err(MANAGED_DICTIONARY_CHANGED.to_owned());
        }
        return Ok(index);
    }
    let by_source = prepared
        .recommended
        .and_then(|source| stored.iter().position(|dictionary| optional(dictionary.get("sourceId")).as_deref() == Some(source.source_id)));
    Ok(by_source.or_else(|| {
        stored.iter().position(|dictionary| {
            strict_eq(dictionary.get("id"), generated.get("id")) || text(dictionary.get("title")) == text(generated.get("title"))
        })
    }))
}

/// `withImport`: `stored` with `generated` added, or replacing the package it updates.
fn with_import(stored: &[Value], generated: &Value, prepared: &Prepared) -> Result<Vec<Value>> {
    let title = text(generated.get("title"));
    if custom::is_personal_title(title) {
        return Err(format!("{title} is reserved for the managed custom dictionary"));
    }
    let Some(index) = replacement_index(stored, generated, prepared)? else {
        let added = match prepared.recommended {
            Some(source) => with_recommended_source(object(generated.clone()), source),
            None => generated.clone(),
        };
        return Ok([stored.to_vec(), vec![added]].concat());
    };
    let collision = stored.iter().enumerate().any(|(other, dictionary)| {
        other != index && (strict_eq(dictionary.get("id"), generated.get("id")) || text(dictionary.get("title")) == title)
    });
    if collision {
        return Err(format!("a dictionary named {title} is already installed"));
    }
    let mut replacement = with_stored_presentation(generated, &stored[index]);
    replacement.insert("lastUpdateCheck".to_owned(), Value::Null);
    if let Some(managed) = &prepared.managed {
        let source = &managed.fingerprint["source"];
        replacement.insert("isUpdatable".to_owned(), Value::Bool(true));
        replacement.insert("indexUrl".to_owned(), source["indexUrl"].clone());
        replacement.insert("downloadUrl".to_owned(), source["downloadUrl"].clone());
        replacement.insert(
            "lastUpdateCheck".to_owned(),
            json!({ "checkedAt": managed.checked_at, "status": "up-to-date", "remoteRevision": generated["revision"], "error": null }),
        );
    }
    let mut next = stored.to_vec();
    next[index] = match prepared.recommended {
        Some(source) => with_recommended_source(replacement, source),
        None => Value::Object(replacement),
    };
    Ok(next)
}

/// `fetchImportArchive` and `stageImportArchive`: the archive, written to `path`.
fn download(fetch: &dyn Fetch, prepared: &Prepared, path: &Path, progress: &dyn Fn(Progress)) -> Result<()> {
    let name = &prepared.file_name;
    let response = fetch.get(&prepared.archive_url, None)?;
    if !(200..300).contains(&response.status) {
        return Err(format!("could not read {name}: HTTP {}", response.status));
    }
    let trusted = match prepared.recommended {
        None => remote::https_url(Some(&Value::from(response.url.as_str()))).is_some(),
        Some(source) => remote::download_url_matches(source, Some(&response.url)),
    };
    if !trusted {
        return Err(format!("{name} downloaded from an unexpected final URL"));
    }
    let (mut body, total) = (response.body, response.total);
    // `consumeResponse`: a report at most every interval, and one at the end.
    let report = |received: u64| progress(Progress::Downloading { received, total: total.filter(|total| received <= *total) });
    let written = File::create(path).and_then(|mut file| {
        let mut buffer = vec![0u8; 64 * 1024];
        let (mut received, mut reported) = (0u64, None::<std::time::Instant>);
        loop {
            let read = match io::Read::read(&mut body, &mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            io::Write::write_all(&mut file, &buffer[..read])?;
            received += read as u64;
            if reported.is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL) {
                reported = Some(std::time::Instant::now());
                report(received);
            }
        }
        report(received);
        Ok(received)
    });
    match written {
        Ok(0) => Err(format!("{name} is empty")),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::StorageFull => Err(format!("no space left on device: {error}")),
        Err(_) => Err(remote::FAILED_TO_FETCH.to_owned()),
    }
}

impl Engine {
    /// `hd_import` of a remote archive. `lock` is the storage lock, taken to read the
    /// request's sources and again to commit, but not while the archive downloads.
    /// `progress` hears the download and the start of the installation.
    pub(crate) fn import_remote(
        &self,
        storage: &impl Storage,
        lock: &Mutex<()>,
        fetch: &dyn Fetch,
        message: &Value,
        progress: &dyn Fn(Progress),
    ) -> Value {
        let request_id = message.get("requestId").cloned().unwrap_or(Value::Null);
        let thrown = |error: ImportError| {
            let mut reply = failure_reply("hd_import", &request_id, &error.message, &self.lock());
            reply.insert("errorCode".to_owned(), Value::from(error.code));
            Value::Object(reply)
        };
        let Ok(_importing) = self.importing.try_lock() else {
            let reply = failure_reply("hd_import", &request_id, IMPORT_IN_PROGRESS, &self.lock());
            return Value::Object(reply);
        };
        let named = text(message.get("fileName"));
        let name = if named.is_empty() { "The dictionary archive" } else { named };
        let prepared = {
            let _serialised = lock.lock().unwrap_or_else(PoisonError::into_inner);
            prepare_with(message, || dictionary_state(storage))
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return thrown(import_error(name, &error, CHECKING)),
        };
        let archive = self.store.download_file(DOWNLOAD_KEY);
        if let Err(error) = download(fetch, &prepared, &archive, progress) {
            let _ = fs::remove_file(&archive);
            return thrown(import_error(&prepared.file_name, &error, READING));
        }
        progress(Progress::Installing);
        let reply = {
            let _serialised = lock.lock().unwrap_or_else(PoisonError::into_inner);
            self.run(storage, "hd_import", &request_id, |session| Ok(session.import_remote(&archive, &prepared, message)))
        };
        let _ = fs::remove_file(&archive);
        reply
    }
}

impl<S: Storage> Session<'_, S> {
    /// The downloaded archive at `archive`, compiled and committed if the request still holds.
    fn import_remote(&mut self, archive: &Path, prepared: &Prepared, message: &Value) -> Map<String, Value> {
        let name = &prepared.file_name;
        let fail = |error: ImportError| {
            let report = empty_report(&error.message);
            object(json!({ "ok": false, "error": error.message, "errorCode": error.code, "report": report }))
        };
        // The commit re-reads the request: a managed package can change while its archive downloads.
        let again = prepare_with(message, || self.dictionary_state());
        match again {
            Ok(again) if again.same(prepared) => {}
            Ok(_) => return fail(import_error(name, "the dictionary import request changed while its archive was downloading", IMPORTING)),
            Err(error) => return fail(import_error(name, &error, IMPORTING)),
        }
        let root = match package::new_generation_root() {
            Ok(root) => root,
            Err(error) => return fail(import_error(name, &error, IMPORTING)),
        };
        let mut report = match self.import_archive(archive, &root, name) {
            Ok(report) => report,
            Err(error) => {
                let _ = self.restore();
                return fail(error);
            }
        };
        if !is_true(&report, "success") {
            let _ = self.restore();
            let error = import_error(name, text(report.get("error")), IMPORTING);
            report.insert("error".to_owned(), Value::from(error.message.as_str()));
            report.insert("errorCode".to_owned(), Value::from(error.code));
            return object(json!({ "ok": false, "error": error.message, "report": report, "errorCode": error.code }));
        }
        let path = format!("{root}/{}", package::folder_name(text(report.get("title"))));
        match self.commit_remote(&path, &mut report, prepared) {
            Ok(()) => object(json!({ "report": report })),
            Err(error) => {
                let _ = self.restore();
                self.cleanup(&[&path]);
                fail(import_error(name, &error, IMPORTING))
            }
        }
    }

    /// `commitImportedGeneration` with no interactive decision.
    fn commit_remote(&mut self, path: &str, report: &mut Map<String, Value>, prepared: &Prepared) -> Result<()> {
        let store = self.engine.store.clone();
        let generated = package::from_index(&store.dictionary_dir(path), path)?;
        let title = text(report.get("title")).to_owned();
        if text(generated.get("title")) != title {
            return Err("the imported dictionary title changed while it was being committed".to_owned());
        }
        if prepared.expected_revision.as_deref().is_some_and(|expected| generated["revision"].as_str() != Some(expected)) {
            return Err("the downloaded dictionary revision did not match its update index".to_owned());
        }
        if let Some(managed) = &prepared.managed
            && remote::https_url(generated.get("indexUrl")).as_deref() != managed.fingerprint["source"]["indexUrl"].as_str()
        {
            return Err("the downloaded dictionary did not match its update source".to_owned());
        }
        if let Some(source) = prepared.recommended {
            let counts = (js::to_number(report.get("termCount")), js::to_number(report.get("frequencyCount")));
            remote::assert_recommended(source, &title, &generated["indexUrl"], &generated["revision"], counts)?;
        }
        if custom::is_personal_title(&title) {
            return Err(format!("{title} is reserved for the managed custom dictionary"));
        }
        for attempt in 0..STORAGE_ATTEMPTS {
            let state = self.dictionary_state()?;
            let stored = list(state.get("dictionaries")).to_vec();
            let next = with_import(&stored, &generated, prepared)?;
            let candidate = self.preload(&next, &paths(&stored))?;
            // Managed updates name no profile, so they leave every profile's list as it is.
            let reply = match self.commit_state(&state, &next, &Joining::JAPANESE) {
                Ok(reply) => reply,
                Err(error) => {
                    let _ = self.restore();
                    return Err(error);
                }
            };
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed(id: &str, title: &str) -> Value {
        json!({ "id": id, "title": title, "path": format!("/dicts/g1/{title}"), "revision": "1", "isUpdatable": true,
            "indexUrl": "https://x.example/index.json", "downloadUrl": "https://x.example/d.zip", "displayName": "Mine",
            "enabled": false, "lastUpdateCheck": null })
    }

    #[test]
    fn requests_name_where_they_download_from() {
        let stored = [managed("a", "A")];
        let print = remote::fingerprint(&stored[0]).unwrap();
        let update = json!({ "managedFingerprint": print, "archiveUrl": "https://x.example/new.zip", "expectedRevision": "2",
            "checkedAt": "2025-01-01T00:00:00.000Z" });
        let prepared = prepare(&update, &stored).unwrap();
        assert_eq!(prepared.archive_url, "https://x.example/new.zip");
        assert_eq!(prepared.file_name, "the archive");
        let mut stale = update.clone();
        stale["managedFingerprint"]["revision"] = json!("0");
        assert_eq!(prepare(&stale, &stored).err().as_deref(), Some(MANAGED_DICTIONARY_CHANGED));
        let mut unchecked = update.clone();
        unchecked.as_object_mut().unwrap().remove("expectedRevision");
        assert_eq!(prepare(&unchecked, &stored).err().as_deref(), Some("the managed import request carried no expected revision"));
        let mut insecure = update;
        insecure["archiveUrl"] = json!("http://x.example/new.zip");
        assert_eq!(prepare(&insecure, &stored).err().as_deref(), Some("the managed dictionary update carried an invalid archive URL"));
        assert_eq!(
            prepare(&json!({ "archiveUrl": "https://x.example/d.zip" }), &stored).err().as_deref(),
            Some("the import request carried no archive URL")
        );
        let pinned = json!({ "sourceId": "jiten", "archiveUrl": "https://api.jiten.moe/other" });
        assert_eq!(
            prepare(&pinned, &[]).err().as_deref(),
            Some("Jiten Frequency Dictionary must be downloaded from its catalogue archive URL")
        );
        let first = json!({ "sourceId": "jiten", "archiveUrl": remote::RECOMMENDED[3].download_url });
        assert_eq!(prepare(&first, &[]).unwrap().file_name, "jiten-frequency.zip");
    }

    #[test]
    fn an_update_replaces_its_package_and_records_the_check() {
        let stored = [managed("a", "A"), managed("b", "B")];
        let print = remote::fingerprint(&stored[0]).unwrap();
        let message = json!({ "managedFingerprint": print, "archiveUrl": "https://x.example/d.zip", "expectedRevision": "2",
            "checkedAt": "2025-01-01T00:00:00.000Z" });
        let prepared = prepare(&message, &stored).unwrap();
        let generated = json!({ "id": "a2", "title": "A", "path": "/dicts/g2/A", "revision": "2", "isUpdatable": false,
            "indexUrl": "https://x.example/index.json", "downloadUrl": null, "displayName": null, "enabled": true, "lastUpdateCheck": null });
        let next = with_import(&stored, &generated, &prepared).unwrap();
        assert_eq!(next.len(), 2);
        assert_eq!(next[0]["id"], "a");
        assert_eq!(next[0]["path"], "/dicts/g2/A");
        assert_eq!(next[0]["displayName"], "Mine");
        assert_eq!(next[0]["enabled"], false);
        assert_eq!(next[0]["downloadUrl"], "https://x.example/d.zip");
        assert_eq!(
            next[0]["lastUpdateCheck"],
            json!({ "checkedAt": "2025-01-01T00:00:00.000Z", "status": "up-to-date", "remoteRevision": "2", "error": null })
        );
        let mut renamed = generated;
        renamed["title"] = json!("B");
        assert_eq!(with_import(&stored, &renamed, &prepared).unwrap_err(), "a dictionary named B is already installed");
    }

    #[test]
    fn a_recommended_install_carries_its_catalogue_source() {
        let message = json!({ "sourceId": "jiten", "archiveUrl": remote::RECOMMENDED[3].download_url });
        let prepared = prepare(&message, &[]).unwrap();
        let generated =
            json!({ "id": "j", "title": "Jiten", "path": "/dicts/g/Jiten", "revision": "5", "isUpdatable": false, "indexUrl": null });
        let next = with_import(&[], &generated, &prepared).unwrap();
        assert_eq!(next[0]["sourceId"], "jiten");
        assert_eq!(next[0]["isUpdatable"], true);
        assert_eq!(next[0]["indexUrl"], "https://api.jiten.moe/api/frequency-list/index");
    }
}
