// SPDX-License-Identifier: GPL-3.0-or-later
//! Managed dictionary updates (`background-updates.js`): Settings' schedule, its
//! checks and installs, and the schedule's own runs. A check reads each managed
//! dictionary's update index; an install downloads the newer archive through the
//! engine's remote import. Cycles run one at a time.

use std::io::Read;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::engine::Engine;
use super::js::{self, strict_eq};
use super::remote::{self, FAILED_TO_FETCH, Fetch, MANAGED_DICTIONARY_CHANGED, TIMED_OUT};
use super::settings::worker::{self, Storage};
use super::store;
use crate::relay::Shared;

/// `UPDATE_SETTINGS_KEY`.
pub(crate) const SETTINGS_KEY: &str = "dictionaryUpdates";
/// `UPDATE_CHECK_IDLE_MS`.
const IDLE: Duration = Duration::from_secs(120);
/// The most of an update index the relay reads.
const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024;
/// How often the schedule looks for a dictionary that is due (the alarm).
const TICK: Duration = Duration::from_secs(60);

type Result<T> = std::result::Result<T, String>;

/// One managed dictionary a cycle looks at (`managedCandidates`).
struct Candidate {
    id: Value,
    title: Value,
    fingerprint: Value,
}

/// A newer revision and where to download it (`remoteUpdate`).
struct Update {
    revision: String,
    archive_url: String,
}

/// What a check found (`checkManagedCandidate`).
struct Checked {
    update: Option<Update>,
    available: Option<Value>,
    outcome: Value,
}

/// The updates service.
pub(crate) struct Updates<S> {
    storage: S,
    /// The storage lock (`serialiseStorage`).
    lock: Arc<Mutex<()>>,
    engine: Arc<Engine>,
    fetch: Arc<dyn Fetch>,
    /// One cycle at a time (`updateTail`).
    queue: Mutex<u64>,
}

impl<S: Storage> Updates<S> {
    pub(crate) fn new(storage: S, lock: Arc<Mutex<()>>, engine: Arc<Engine>, fetch: Arc<dyn Fetch>) -> Self {
        Self { storage, lock, engine, fetch, queue: Mutex::new(0) }
    }

    fn serialised<T>(&self, run: impl FnOnce() -> T) -> T {
        let _serialised = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        run()
    }

    fn dictionaries(&self) -> Vec<Value> {
        let state = self.storage.read(store::DICTIONARY_STATE).unwrap_or(Value::Null);
        state.get("dictionaries").and_then(Value::as_array).cloned().unwrap_or_default()
    }

    fn settings(&self) -> Value {
        remote::update_settings(self.storage.read(SETTINGS_KEY).as_ref())
    }

    /// `handleUpdatesRequest`: the reply to a `hachidori-updates` request.
    pub(crate) fn handle(&self, message: &Value) -> Value {
        let kind = message.get("type").and_then(Value::as_str).unwrap_or_default();
        let result = match kind {
            "hd_updates_schedule" => self.schedule(message),
            "hd_updates_check" => match message.get("dictionaryIds") {
                None => Ok(self.cycle(None, false, false)),
                Some(Value::Array(ids)) => Ok(self.cycle(Some(ids), false, false)),
                Some(_) => Err("the dictionary update request carried no dictionary IDs".to_owned()),
            },
            "hd_updates_install" => match message.get("dictionaryIds") {
                Some(Value::Array(ids)) => Ok(self.cycle(Some(ids), true, false)),
                _ => Err("the dictionary update request carried no dictionary IDs".to_owned()),
            },
            _ => Err(format!("unknown update request type {}", Value::from(kind))),
        };
        match result {
            Ok(mut payload) => {
                let ok = payload.remove("ok").unwrap_or(Value::Bool(true));
                let error = payload.remove("error").unwrap_or(Value::Null);
                let mut reply = json!({ "type": format!("{kind}_result"), "requestId": message.get("requestId").cloned().unwrap_or(Value::Null),
                        "ok": ok, "error": error });
                if let Some(fields) = reply.as_object_mut() {
                    fields.extend(payload);
                }
                reply
            }
            Err(error) => super::local::failure(message, &error),
        }
    }

    /// `writeUpdateSettings`.
    fn write_settings(&self, update: impl FnOnce(&Value) -> Option<Value>) -> Result<Map<String, Value>> {
        self.serialised(|| {
            let current = self.settings();
            let Some(next) = update(&current) else {
                return Ok(json_map(json!({ "ok": false,
                    "error": "The update settings changed elsewhere. Review the current schedule before retrying.", "settings": current })));
            };
            if js::same_json(&next, &current) {
                return Ok(json_map(json!({ "settings": current })));
            }
            let mut settings = next;
            settings["revision"] = js::number(js::to_number(current.get("revision")) + 1.0);
            self.storage.write(json_map(json!({ SETTINGS_KEY: settings.clone() })))?;
            Ok(json_map(json!({ "settings": settings })))
        })
    }

    /// `hd_updates_schedule`.
    fn schedule(&self, message: &Value) -> Result<Map<String, Value>> {
        let schedule = remote::schedule(message.get("schedule")).ok_or("the dictionary update schedule is invalid")?.to_owned();
        self.write_settings(|current| {
            strict_eq(message.get("baseRevision"), current.get("revision")).then(|| {
                let mut next = current.clone();
                next["schedule"] = Value::from(schedule);
                next
            })
        })
    }

    /// `updateDictionaryCheck`: records `last` on the dictionary, if it is still the
    /// one checked. Its stored entry afterwards, or `None` when it changed.
    fn record_check(&self, fingerprint: &Value, last: Value) -> Result<Option<Value>> {
        self.serialised(|| {
            let state = self.storage.read(store::DICTIONARY_STATE).unwrap_or(Value::Null);
            let mut dictionaries = state.get("dictionaries").and_then(Value::as_array).cloned().unwrap_or_default();
            let index = dictionaries.iter().position(|dictionary| strict_eq(dictionary.get("id"), fingerprint.get("id")));
            let Some(index) = index.filter(|&index| remote::matches(Some(&dictionaries[index]), fingerprint)) else { return Ok(None) };
            dictionaries[index]["lastUpdateCheck"] = last;
            let message = json!({ "type": "hd_state_cas", "baseRevision": state.get("revision").cloned().unwrap_or(Value::Null),
                "dictionaries": dictionaries });
            let reply = worker::handle(&self.storage, &message, &[]);
            if reply.get("ok") == Some(&Value::Bool(false)) {
                let error = reply.get("error").and_then(Value::as_str).filter(|error| !error.is_empty());
                return Err(error.unwrap_or("the dictionary update state could not be saved").to_owned());
            }
            let committed = reply.pointer("/state/dictionaries").and_then(Value::as_array);
            Ok(committed.and_then(|list| list.iter().find(|dictionary| strict_eq(dictionary.get("id"), fingerprint.get("id")))).cloned())
        })
    }

    /// `managedCandidates`.
    fn candidates(&self, ids: Option<&[Value]>) -> Vec<Candidate> {
        self.serialised(|| self.dictionaries())
            .into_iter()
            .filter(|dictionary| ids.is_none_or(|ids| ids.iter().any(|id| strict_eq(Some(id), dictionary.get("id")))))
            .filter_map(|dictionary| {
                let fingerprint = remote::fingerprint(&dictionary)?;
                let title = Some(&dictionary["displayName"]).filter(|name| js::truthy(Some(name))).unwrap_or(&dictionary["title"]).clone();
                Some(Candidate { id: dictionary["id"].clone(), title, fingerprint })
            })
            .collect()
    }

    /// `readUpdateIndex` and `remoteUpdate`.
    fn remote_update(&self, candidate: &Candidate) -> Result<Update> {
        let source = &candidate.fingerprint["source"];
        let timed_out = |error: String| {
            if error == TIMED_OUT {
                "Dictionary update check received no data for two minutes. Try checking again.".to_owned()
            } else {
                error
            }
        };
        let response = self.fetch.get(source["indexUrl"].as_str().unwrap_or_default(), Some(IDLE)).map_err(timed_out)?;
        if !(200..300).contains(&response.status) {
            return Err(format!("update index request failed with HTTP {}", response.status));
        }
        if source["kind"] == "recommended" {
            let entry = remote::recommended(source["sourceId"].as_str());
            if !entry.is_some_and(|entry| remote::index_url_matches(entry, Some(&response.url))) {
                return Err("update index downloaded from an unexpected final URL".to_owned());
            }
        } else if remote::https_url(Some(&Value::from(response.url.as_str()))).is_none() {
            return Err("update index redirected to a non-HTTPS URL".to_owned());
        }
        let mut body = Vec::new();
        response.body.take(MAX_INDEX_BYTES + 1).read_to_end(&mut body).map_err(|error| {
            timed_out(if error.kind() == std::io::ErrorKind::TimedOut { TIMED_OUT.to_owned() } else { FAILED_TO_FETCH.to_owned() })
        })?;
        if body.len() as u64 > MAX_INDEX_BYTES {
            return Err(format!("update index exceeds the {} MiB the Hachidori relay reads", MAX_INDEX_BYTES >> 20));
        }
        let index: Value = serde_json::from_slice(&body).map_err(|error| format!("update index is not valid JSON: {error}"))?;
        let revision = index.get("revision").and_then(Value::as_str).filter(|revision| !revision.is_empty());
        let revision = revision.ok_or("update index did not declare a revision")?.to_owned();
        let mut archive_url = source["downloadUrl"].as_str().unwrap_or_default().to_owned();
        if source["kind"] == "generic"
            && let Some(url) = index.get("downloadUrl").filter(|url| url.as_str().is_some_and(|url| !url.is_empty()))
        {
            archive_url = remote::https_url(Some(url)).ok_or("update index returned a non-HTTPS download URL")?;
        }
        Ok(Update { revision, archive_url })
    }

    /// `recordManagedOutcome`.
    fn record_outcome(&self, candidate: &Candidate, last: Value, outcome: Value) -> Value {
        match self.record_check(&candidate.fingerprint, last) {
            Ok(Some(_)) => outcome,
            Ok(None) => json!({ "id": candidate.id, "status": "check-failed", "error": MANAGED_DICTIONARY_CHANGED }),
            Err(error) => json!({ "id": candidate.id, "status": "check-failed", "error": error }),
        }
    }

    /// `checkManagedCandidate`.
    fn check(&self, candidate: &Candidate, checked_at: &str) -> Checked {
        let update = match self.remote_update(candidate) {
            Ok(update) => update,
            Err(error) => {
                let last = json!({ "checkedAt": checked_at, "status": "check-failed", "remoteRevision": null, "error": error });
                let outcome = self.record_outcome(candidate, last, json!({ "id": candidate.id, "status": "check-failed", "error": error }));
                return Checked { update: None, available: None, outcome };
            }
        };
        if candidate.fingerprint["revision"].as_str() == Some(update.revision.as_str()) {
            let last = json!({ "checkedAt": checked_at, "status": "up-to-date", "remoteRevision": update.revision, "error": null });
            let outcome = self.record_outcome(candidate, last, json!({ "id": candidate.id, "status": "up-to-date" }));
            return Checked { update: None, available: None, outcome };
        }
        let available = json!({ "checkedAt": checked_at, "status": "update-available", "remoteRevision": update.revision, "error": null });
        let outcome = self.record_outcome(candidate, available.clone(), json!({ "id": candidate.id, "status": "update-available" }));
        let update = (outcome["status"] == "update-available").then_some(update);
        Checked { update, available: Some(available), outcome }
    }

    /// `installManagedCandidate`.
    fn install(&self, candidate: &Candidate, update: &Update, checked_at: &str, counter: u64) -> Result<()> {
        let source = &candidate.fingerprint["source"];
        let source_id = if source["kind"] == "recommended" { source["sourceId"].clone() } else { Value::Null };
        let message = json!({
            "target": "hoshidicts-offscreen",
            "type": "hd_import",
            "requestId": format!("managed-update-{counter}"),
            "managedFingerprint": candidate.fingerprint,
            "sourceId": source_id,
            "archiveUrl": update.archive_url,
            "expectedRevision": update.revision,
            "checkedAt": checked_at,
            "fileName": candidate.title,
        });
        let reply = self.engine.import_remote(&self.storage, &self.lock, self.fetch.as_ref(), &message);
        if reply["ok"] == true && reply["report"]["success"] == true {
            return Ok(());
        }
        let error = [&reply["error"], &reply["report"]["error"]].into_iter().find(|error| js::truthy(Some(error)));
        Err(error.map_or_else(|| "the dictionary update failed".to_owned(), |error| js::to_string(Some(error))))
    }

    /// `installCheckedCandidate`.
    fn install_checked(&self, candidate: &Candidate, checked: Checked, checked_at: &str, counter: u64) -> Value {
        let (Some(update), Some(available)) = (checked.update, checked.available) else { return checked.outcome };
        let Err(mut error) = self.install(candidate, &update, checked_at, counter) else {
            return json!({ "id": candidate.id, "status": "updated" });
        };
        let mut last = available;
        last["error"] = Value::from(error.as_str());
        let failed = self.record_check(&candidate.fingerprint, last).ok().flatten();
        if failed.is_none() {
            MANAGED_DICTIONARY_CHANGED.clone_into(&mut error);
        }
        json!({ "id": candidate.id, "status": if failed.is_none() { "check-failed" } else { "update-available" }, "error": error })
    }

    /// `scheduledCandidateIsDue`.
    fn due(&self, candidate: &Candidate) -> bool {
        let (dictionaries, settings) = self.serialised(|| (self.dictionaries(), self.settings()));
        let current = dictionaries.iter().find(|dictionary| strict_eq(dictionary.get("id"), Some(&candidate.id)));
        let Some(current) = current.filter(|current| remote::matches(Some(current), &candidate.fingerprint)) else { return false };
        let now = js::now();
        remote::next_check(current, settings["schedule"].as_str().unwrap_or("off"), now).is_some_and(|due| due <= now)
    }

    /// `queueManagedUpdate` and `runManagedUpdateCycle`: `{outcomes, settings}`.
    fn cycle(&self, ids: Option<&[Value]>, install: bool, due_only: bool) -> Map<String, Value> {
        let mut counter = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        let mut outcomes = Vec::new();
        for candidate in self.candidates(ids) {
            // A later package can be switched Off while an earlier fetch is in flight.
            if due_only && !self.due(&candidate) {
                continue;
            }
            let checked_at = js::now_iso();
            let checked = self.check(&candidate, &checked_at);
            outcomes.push(if install {
                *counter += 1;
                self.install_checked(&candidate, checked, &checked_at, *counter)
            } else {
                checked.outcome
            });
        }
        if due_only && outcomes.is_empty() {
            return json_map(json!({ "outcomes": outcomes, "settings": self.serialised(|| self.settings()) }));
        }
        let written = self.write_settings(|current| {
            let mut next = current.clone();
            next["lastCheckedAt"] = Value::from(js::now_iso());
            Some(next)
        });
        let settings = written.ok().and_then(|mut written| written.remove("settings")).unwrap_or_else(|| self.settings());
        json_map(json!({ "outcomes": outcomes, "settings": settings }))
    }

    /// Whether a managed dictionary is due by the schedule (`nextManagedUpdateCheck`).
    fn anything_due(&self) -> bool {
        let (dictionaries, settings) = self.serialised(|| (self.dictionaries(), self.settings()));
        let now = js::now();
        remote::next_managed_check(&dictionaries, settings["schedule"].as_str().unwrap_or("off"), now).is_some_and(|due| due <= now)
    }
}

fn json_map(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// The schedule's runs (the update alarm): every minute, installs what is due. Not
/// while a browser host is connected, which runs its own schedule and whose
/// dictionaries the relay mirrors.
pub(crate) fn run_schedule<S: Storage + Send + Sync + 'static>(updates: Arc<Updates<S>>, relay: Weak<Shared>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            let Some(relay) = relay.upgrade() else { return };
            if relay.browser_host().borrow().is_some() {
                continue;
            }
            drop(relay);
            let updates = updates.clone();
            let ran = tokio::task::spawn_blocking(move || {
                if updates.anything_due() {
                    updates.cycle(None, true, true);
                }
            });
            if let Err(error) = ran.await {
                tracing::warn!(%error, "a scheduled dictionary update stopped");
            }
        }
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::io::Cursor;

    use super::*;
    use crate::host::remote::Fetched;

    /// `(status, final URL, body)` by URL.
    pub(crate) type Responses = HashMap<String, (u16, String, Vec<u8>)>;

    /// Serves fixed responses.
    #[derive(Default)]
    pub(crate) struct Served(pub(crate) Mutex<Responses>);

    impl Fetch for Served {
        fn get(&self, url: &str, _idle: Option<Duration>) -> Result<Fetched> {
            let served = self.0.lock().unwrap().get(url).cloned().ok_or_else(|| FAILED_TO_FETCH.to_owned())?;
            Ok(Fetched { status: served.0, url: served.1, body: Box::new(Cursor::new(served.2)) })
        }
    }

    pub(crate) struct Memory(pub(crate) Mutex<Map<String, Value>>);

    impl Storage for Memory {
        fn read(&self, key: &str) -> Option<Value> {
            self.0.lock().unwrap().get(key).cloned()
        }

        fn write(&self, values: Map<String, Value>) -> Result<()> {
            self.0.lock().unwrap().extend(values);
            Ok(())
        }
    }

    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn service(dictionaries: &Value, served: Served) -> Updates<Memory> {
        let storage = Memory(Mutex::new(json_map(
            json!({ "dictionaryState": { "schemaVersion": 1, "revision": 3, "dictionaries": dictionaries, "groups": [] } }),
        )));
        let store = Arc::new(
            store::Store::open(store::tests::scratch(&format!("updates-{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))))
                .unwrap(),
        );
        Updates::new(storage, Arc::default(), Arc::new(Engine::new(store)), Arc::new(served))
    }

    fn managed(id: &str, revision: &str) -> Value {
        json!({ "id": id, "title": id, "displayName": null, "path": format!("/dicts/{id}"), "revision": revision, "enabled": true,
            "favorite": false, "isUpdatable": true, "indexUrl": format!("https://x.example/{id}.json"),
            "downloadUrl": format!("https://x.example/{id}.zip"), "lastUpdateCheck": null })
    }

    fn index(served: &Served, id: &str, status: u16, body: &str) {
        let url = format!("https://x.example/{id}.json");
        served.0.lock().unwrap().insert(url.clone(), (status, url, body.as_bytes().to_vec()));
    }

    #[test]
    fn a_check_records_what_each_index_says() {
        let served = Served::default();
        index(&served, "a", 200, r#"{"revision":"1"}"#);
        index(&served, "b", 200, r#"{"revision":"2"}"#);
        index(&served, "c", 404, "");
        let updates = service(
            &json!([managed("a", "1"), managed("b", "1"), managed("c", "1"), { "id": "plain", "path": "/p", "revision": "1" }]),
            served,
        );
        let reply = updates.handle(&json!({ "target": "hachidori-updates", "type": "hd_updates_check", "requestId": "q" }));
        assert_eq!(reply["type"], "hd_updates_check_result");
        assert_eq!(reply["ok"], true);
        assert_eq!(
            reply["outcomes"],
            json!([
                { "id": "a", "status": "up-to-date" },
                { "id": "b", "status": "update-available" },
                { "id": "c", "status": "check-failed", "error": "update index request failed with HTTP 404" },
            ])
        );
        assert!(reply["settings"]["lastCheckedAt"].is_string());
        assert_eq!(reply["settings"]["revision"], 1);
        let stored = updates.dictionaries();
        assert_eq!(stored[1]["lastUpdateCheck"]["status"], "update-available");
        assert_eq!(stored[1]["lastUpdateCheck"]["remoteRevision"], "2");
        assert_eq!(stored[2]["lastUpdateCheck"]["error"], "update index request failed with HTTP 404");
        assert!(stored[3].get("lastUpdateCheck").is_none());
    }

    #[test]
    fn a_failed_install_keeps_the_update_available() {
        let served = Served::default();
        index(&served, "a", 200, r#"{"revision":"2","downloadUrl":"https://x.example/new.zip"}"#);
        served.0.lock().unwrap().insert("https://x.example/new.zip".to_owned(), (500, "https://x.example/new.zip".to_owned(), Vec::new()));
        let updates = service(&json!([managed("a", "1")]), served);
        let reply = updates.handle(&json!({ "type": "hd_updates_install", "requestId": "q", "dictionaryIds": ["a"] }));
        assert_eq!(reply["outcomes"][0]["status"], "update-available");
        assert_eq!(reply["outcomes"][0]["error"], "a: reading the archive failed. could not read a: HTTP 500");
        assert_eq!(updates.dictionaries()[0]["lastUpdateCheck"]["error"], reply["outcomes"][0]["error"]);
        let missing = updates.handle(&json!({ "type": "hd_updates_install", "requestId": "q" }));
        assert_eq!(missing["error"], "the dictionary update request carried no dictionary IDs");
    }

    /// A one-term Yomitan archive titled `title` at `revision`, updated from `a.json`.
    fn archive(title: &str, revision: &str) -> Vec<u8> {
        zipped(&json!({ "title": title, "revision": revision, "format": 3, "isUpdatable": true,
            "indexUrl": "https://x.example/a.json", "downloadUrl": "https://x.example/a.zip" }))
    }

    /// A one-term Yomitan archive with `index` as its `index.json`.
    pub(crate) fn zipped(index: &Value) -> Vec<u8> {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("index.json", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(index.to_string().as_bytes()).unwrap();
        zip.start_file("term_bank_1.json", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(json!([["読む", "よむ", "", "", 0, ["to read"], 1, ""]]).to_string().as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn an_install_downloads_and_commits_the_newer_revision() {
        let served = Served::default();
        index(&served, "a", 200, r#"{"revision":"2"}"#);
        let download = "https://x.example/a.zip".to_owned();
        served.0.lock().unwrap().insert(download.clone(), (200, download, archive("a", "2")));
        let updates = service(&json!([managed("a", "1")]), served);
        let reply = updates.handle(&json!({ "type": "hd_updates_install", "requestId": "q", "dictionaryIds": ["a"] }));
        assert_eq!(reply["outcomes"], json!([{ "id": "a", "status": "updated" }]), "{reply}");
        let stored = updates.dictionaries();
        assert_eq!((&stored[0]["id"], &stored[0]["revision"], &stored[0]["termCount"]), (&json!("a"), &json!("2"), &json!(1)));
        assert_ne!(stored[0]["path"], "/dicts/a");
        assert_eq!(stored[0]["lastUpdateCheck"]["status"], "up-to-date");
        assert_eq!(stored[0]["lastUpdateCheck"]["remoteRevision"], "2");
        // The installed package is what lookups now read.
        let found = updates.engine.handle(&json!({ "type": "hd_lookup", "requestId": "l", "text": "読む" }));
        assert_eq!(found["results"][0]["term"]["expression"], "読む", "{found}");
        // The next check finds it up to date.
        let again = updates.handle(&json!({ "type": "hd_updates_check", "requestId": "q" }));
        assert_eq!(again["outcomes"], json!([{ "id": "a", "status": "up-to-date" }]));
    }

    #[test]
    fn the_schedule_is_written_against_its_revision() {
        let updates = service(&json!([]), Served::default());
        let set = |base: Value, schedule: &str| {
            updates.handle(&json!({ "type": "hd_updates_schedule", "requestId": "s", "baseRevision": base, "schedule": schedule }))
        };
        assert_eq!(set(json!(0), "daily")["settings"], json!({ "revision": 1, "schedule": "daily", "lastCheckedAt": null }));
        let stale = set(json!(0), "weekly");
        assert_eq!(stale["ok"], false);
        assert_eq!(stale["error"], "The update settings changed elsewhere. Review the current schedule before retrying.");
        assert_eq!(set(json!(1), "yearly")["error"], "the dictionary update schedule is invalid");
        assert_eq!(set(json!(1), "daily")["settings"]["revision"], 1);
    }
}
