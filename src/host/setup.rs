// SPDX-License-Identifier: GPL-3.0-or-later
//! Installing the recommended dictionaries (`setup-installer.js`), for Settings
//! and setup in a linked browser. One run at a time downloads and imports the
//! requested catalogue sources through the engine's remote import. A request
//! attaches to the active run, or starts one when none is active; an empty
//! request only observes. Each outcome is recorded as the extension's worker
//! records it for a shared request (`hd_setup_record` without `recordSetup`): a
//! source's first install selects it for its dictionary option once, while
//! that option is still Automatic.

use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::engine::{Engine, IMPORT_IN_PROGRESS};
use super::js;
use super::remote::{self, Fetch, RECOMMENDED, Recommended};
use super::settings::options;
use super::settings::worker::{self, Storage};
use super::store;

/// `RECOMMENDED_SELECTIONS_KEY`: the sources whose first install chose their option.
const SELECTIONS_KEY: &str = "recommendedDictionarySelections";
/// `SETUP_STATE_KEY`: startup's record, which may carry older selections.
const SETUP_STATE_KEY: &str = "setupState";
const OPTIONS_KEY: &str = "options";
/// `IDLE_POLL_MS`: how often a waiting install looks for the engine to be free.
const IDLE_POLL: Duration = Duration::from_millis(250);
/// `RECORD_RETRY_MS` and `RECORD_RETRY_MAX_MS`.
const RECORD_RETRY: Duration = Duration::from_millis(250);
const RECORD_RETRY_MAX: Duration = Duration::from_secs(2);

type Result<T> = std::result::Result<T, String>;

/// One requested source in a run.
struct Entry {
    source: &'static Recommended,
    phase: &'static str,
    seconds: Option<f64>,
    error: Option<String>,
}

/// The active or last run.
struct Run {
    id: String,
    sequence: u64,
    finished: bool,
    entries: Vec<Entry>,
}

/// How a source settled.
struct Outcome {
    status: &'static str,
    seconds: Option<f64>,
    error: Option<String>,
}

/// The installer.
pub(crate) struct Setup<S> {
    storage: S,
    /// The storage lock (`serialiseStorage`).
    lock: Arc<Mutex<()>>,
    engine: Arc<Engine>,
    fetch: Arc<dyn Fetch>,
    run: Mutex<Option<Run>>,
}

/// `requestedSetupSources`: the catalogue sources named, in catalogue order.
fn requested(source_ids: Option<&Value>) -> Result<Vec<&'static Recommended>> {
    let ids = source_ids.and_then(Value::as_array).filter(|ids| ids.iter().all(Value::is_string));
    let ids = ids.ok_or("the setup install request carried no source list")?;
    Ok(RECOMMENDED.iter().filter(|source| ids.iter().any(|id| id.as_str() == Some(source.source_id))).collect())
}

/// `FIRST_INSTALL_SELECTORS`: the option's value for a committed title.
fn selection(option: &str, title: &Value) -> Value {
    if option == "kanjiClickDictionary" { json!({ "title": title, "kind": "term" }) } else { title.clone() }
}

/// `firstInstallSelections`: the sources applied, and the options to store, if changed.
fn first_install_selections(
    previous: &[Value],
    outcome: (&str, &str),
    state: &Value,
    stored: Option<&Value>,
) -> Result<(Vec<&'static str>, Option<Value>)> {
    let dictionaries = state.get("dictionaries").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let effective = options::normalise(stored);
    let (mut applied, mut patch) = (Vec::new(), Map::new());
    for source in &RECOMMENDED {
        let Some(option) = source.first_install_option else { continue };
        let settled = outcome.0 == source.source_id && matches!(outcome.1, "installed" | "already-installed");
        if !settled || previous.iter().any(|id| id.as_str() == Some(source.source_id)) {
            continue;
        }
        let Some(committed) = remote::installed(source, dictionaries) else { continue };
        applied.push(source.source_id);
        if effective.get(option).and_then(Value::as_str) == Some("") {
            patch.insert(option.to_owned(), selection(option, &committed["title"]));
        }
    }
    if patch.is_empty() {
        return Ok((applied, None));
    }
    let revision = js::number(worker::options_revision(stored));
    let mut merged = options::project_stored(stored);
    merged.extend(options::validate_patch(Some(&Value::Object(patch)))?);
    merged.insert("revision".to_owned(), revision.clone());
    let groups = state.get("groups").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let mut next = options::normalise_dictionary_selections(&Value::Object(merged), dictionaries, groups)?;
    let mut base = stored.and_then(Value::as_object).cloned().unwrap_or_default();
    base.insert("revision".to_owned(), revision);
    if js::same_json(&next, &Value::Object(base)) {
        return Ok((applied, None));
    }
    next["revision"] = js::number(worker::options_revision(stored) + 1.0);
    Ok((applied, Some(next)))
}

impl<S: Storage + Send + Sync + 'static> Setup<S> {
    pub(crate) fn new(storage: S, lock: Arc<Mutex<()>>, engine: Arc<Engine>, fetch: Arc<dyn Fetch>) -> Self {
        Self { storage, lock, engine, fetch, run: Mutex::new(None) }
    }

    fn current(&self) -> std::sync::MutexGuard<'_, Option<Run>> {
        self.run.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `snapshot`.
    fn snapshot(run: Option<&Run>) -> Map<String, Value> {
        let Some(run) = run else {
            return worker_map(json!({ "runId": null, "sequence": 0, "finished": true, "entries": [] }));
        };
        let entries: Vec<Value> = run
            .entries
            .iter()
            .map(|entry| {
                json!({ "sourceId": entry.source.source_id, "phase": entry.phase, "receivedBytes": 0, "totalBytes": null,
                    "seconds": entry.seconds, "error": entry.error })
            })
            .collect();
        worker_map(json!({ "runId": run.id, "sequence": run.sequence, "finished": run.finished, "entries": entries }))
    }

    /// Changes the entry at `index` (the run's last change when `None`), as one step of the run.
    fn step(&self, index: Option<usize>, change: impl FnOnce(&mut Entry)) {
        if let Some(run) = self.current().as_mut() {
            match index {
                Some(index) => change(&mut run.entries[index]),
                None => run.finished = true,
            }
            run.sequence += 1;
        }
    }

    /// The reply to a `hachidori-setup` request: `attach`.
    pub(crate) fn handle(self: &Arc<Self>, message: &Value) -> Value {
        let kind = message.get("type").and_then(Value::as_str).unwrap_or_default();
        if kind != "hd_setup_install" {
            return super::local::failure(message, &format!("unknown setup request type {}", Value::from(kind)));
        }
        let sources = match requested(message.get("sourceIds")) {
            Ok(sources) => sources,
            Err(error) => return super::local::failure(message, &error),
        };
        let snapshot = {
            let mut run = self.current();
            if run.as_ref().is_none_or(|run| run.finished) && !sources.is_empty() {
                let id = worker::random_uuid().unwrap_or_else(|_| format!("{:x}", js::now().to_bits()));
                let entries = sources.iter().map(|&source| Entry { source, phase: "waiting", seconds: None, error: None }).collect();
                *run = Some(Run { id: id.clone(), sequence: 0, finished: false, entries });
                let setup = self.clone();
                thread::spawn(move || setup.execute(&id, sources));
            }
            Self::snapshot(run.as_ref())
        };
        let mut reply = worker_map(json!({ "type": "hd_setup_install_result",
            "requestId": message.get("requestId").cloned().unwrap_or(Value::Null), "ok": true, "error": null }));
        reply.extend(snapshot);
        Value::Object(reply)
    }

    fn dictionaries(&self) -> Vec<Value> {
        let state = self.storage.read(store::DICTIONARY_STATE).unwrap_or(Value::Null);
        state.get("dictionaries").and_then(Value::as_array).cloned().unwrap_or_default()
    }

    /// `execute`: each source in turn, never one that is already installed.
    fn execute(&self, id: &str, sources: Vec<&'static Recommended>) {
        for (index, source) in sources.into_iter().enumerate() {
            let outcome = if remote::installed(source, &self.dictionaries()).is_some() {
                Outcome { status: "already-installed", seconds: None, error: None }
            } else {
                self.import(id, index, source)
            };
            self.record(source, &outcome);
            self.step(Some(index), |entry| {
                entry.phase = outcome.status;
                entry.seconds = outcome.seconds;
                entry.error = outcome.error;
            });
        }
        self.step(None, |_| {});
    }

    /// `importEntry`: waits for any other import, then downloads and imports `source`.
    fn import(&self, run: &str, index: usize, source: &'static Recommended) -> Outcome {
        self.step(Some(index), |entry| entry.phase = "downloading");
        let message = json!({ "target": "hoshidicts-offscreen", "type": "hd_import",
            "requestId": format!("setup:{run}:import:{}:{index}", source.source_id),
            "sourceId": source.source_id, "archiveUrl": source.download_url, "fileName": source.archive_name });
        loop {
            // The wait may have been another import of this very source.
            if remote::installed(source, &self.dictionaries()).is_some() {
                return Outcome { status: "already-installed", seconds: None, error: None };
            }
            let reply = self.engine.import_remote(&self.storage, &self.lock, self.fetch.as_ref(), &message);
            if reply["error"] == IMPORT_IN_PROGRESS {
                thread::sleep(IDLE_POLL);
                continue;
            }
            if reply["ok"] == true && reply["report"]["success"] == true {
                return Outcome { status: "installed", seconds: Some(0.0), error: None };
            }
            let error = [&reply["error"], &reply["report"]["error"]].into_iter().find_map(|error| error.as_str().filter(|e| !e.is_empty()));
            let error = error.unwrap_or("the import did not complete").to_owned();
            return Outcome { status: "failed", seconds: None, error: Some(error) };
        }
    }

    /// `record` and `hd_setup_record`: the first-install selections, retried until stored.
    fn record(&self, source: &Recommended, outcome: &Outcome) {
        let mut delay = RECORD_RETRY;
        while let Err(error) = self.record_once(source, outcome) {
            tracing::warn!(%error, "could not record a setup outcome, retrying");
            thread::sleep(delay);
            delay = (delay * 2).min(RECORD_RETRY_MAX);
        }
    }

    fn record_once(&self, source: &Recommended, outcome: &Outcome) -> Result<()> {
        let _serialised = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let stored = self.storage.read(OPTIONS_KEY);
        let setup = self.storage.read(SETUP_STATE_KEY);
        let previous = match self.storage.read(SELECTIONS_KEY) {
            Some(Value::Array(previous)) => previous,
            Some(Value::Null) | None => setup
                .as_ref()
                .and_then(|setup| setup.pointer("/dictionaries/selectionsApplied"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            Some(_) => Vec::new(),
        };
        let state = self.storage.read(store::DICTIONARY_STATE).unwrap_or(Value::Null);
        let (applied, options) = first_install_selections(&previous, (source.source_id, outcome.status), &state, stored.as_ref())?;
        let mut values = Map::new();
        if !applied.is_empty() {
            let mut selections = previous;
            for id in applied {
                if !selections.iter().any(|selected| selected == id) {
                    selections.push(Value::from(id));
                }
            }
            values.insert(SELECTIONS_KEY.to_owned(), Value::Array(selections));
        }
        if let Some(options) = options {
            values.insert(OPTIONS_KEY.to_owned(), options);
        }
        if values.is_empty() { Ok(()) } else { self.storage.write(values) }
    }
}

fn worker_map(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::host::updates::tests::{Memory, Served, zipped};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn installer(stored: Value, served: Served) -> Arc<Setup<Memory>> {
        let storage = Memory(Mutex::new(worker_map(stored)));
        let store =
            Arc::new(store::Store::open(store::tests::scratch(&format!("setup-{}", NEXT.fetch_add(1, Ordering::Relaxed)))).unwrap());
        Arc::new(Setup::new(storage, Arc::default(), Arc::new(Engine::new(store)), Arc::new(served)))
    }

    fn state(dictionaries: &Value) -> Value {
        json!({ "dictionaryState": { "schemaVersion": 1, "revision": 1, "dictionaries": dictionaries, "groups": [] } })
    }

    fn source(id: &str) -> &'static Recommended {
        remote::recommended(Some(id)).unwrap()
    }

    /// Serves `source`'s catalogue archive at its download URL.
    fn serve(served: &Served, source: &Recommended, title: &str) {
        let index = json!({ "title": title, "revision": "2026.01.01", "format": 3, "isUpdatable": true,
            "indexUrl": source.index_url, "downloadUrl": source.download_url });
        served.0.lock().unwrap().insert(source.download_url.to_owned(), (200, source.download_url.to_owned(), zipped(&index)));
    }

    fn install(setup: &Arc<Setup<Memory>>, ids: &[&str]) -> Value {
        setup.handle(&json!({ "target": "hachidori-setup", "type": "hd_setup_install", "requestId": "s", "sourceIds": ids }))
    }

    /// Observes the run until it finishes.
    fn finished(setup: &Arc<Setup<Memory>>) -> Value {
        for _ in 0..400 {
            let reply = install(setup, &[]);
            if reply["finished"] == true {
                return reply;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("the setup run did not finish");
    }

    #[test]
    fn requests_are_checked_and_an_empty_one_observes() {
        let setup = installer(state(&json!([])), Served::default());
        assert_eq!(
            install(&setup, &[]),
            json!({ "type": "hd_setup_install_result", "requestId": "s", "ok": true, "error": null,
                "runId": null, "sequence": 0, "finished": true, "entries": [] })
        );
        let malformed = setup.handle(&json!({ "type": "hd_setup_install", "requestId": "s", "sourceIds": [1] }));
        assert_eq!((&malformed["ok"], &malformed["error"]), (&json!(false), &json!("the setup install request carried no source list")));
        // Unknown sources are not requested at all.
        assert_eq!(install(&setup, &["nothing"])["runId"], Value::Null);
    }

    #[test]
    fn a_run_installs_each_source_once_and_applies_its_first_install_option() {
        let served = Served::default();
        let kanji = source("bees-ultimate-kanji-dictionary");
        serve(&served, kanji, "Bee's Ultimate Kanji Dictionary");
        // Jiten fails to download.
        let setup = installer(state(&json!([])), served);
        let started = install(&setup, &["jiten", "bees-ultimate-kanji-dictionary"]);
        assert_eq!(started["finished"], false);
        let order: Vec<&Value> = started["entries"].as_array().unwrap().iter().map(|entry| &entry["sourceId"]).collect();
        assert_eq!(order, [&json!("bees-ultimate-kanji-dictionary"), &json!("jiten")], "catalogue order");
        // A second request attaches to the same run.
        assert_eq!(install(&setup, &["jitendex"])["runId"], started["runId"]);

        let done = finished(&setup);
        assert_eq!(done["runId"], started["runId"]);
        let phases: Vec<&Value> = done["entries"].as_array().unwrap().iter().map(|entry| &entry["phase"]).collect();
        assert_eq!(phases, [&json!("installed"), &json!("failed")], "{done}");
        assert_eq!(done["entries"][1]["error"], "jiten-frequency.zip: reading the archive failed. Failed to fetch");

        let stored = setup.dictionaries();
        let installed = stored.iter().find(|entry| entry["sourceId"] == kanji.source_id).unwrap();
        assert_eq!(installed["title"], "Bee's Ultimate Kanji Dictionary");
        let options = setup.storage.read(OPTIONS_KEY).unwrap();
        assert_eq!(options["kanjiClickDictionary"], json!({ "title": "Bee's Ultimate Kanji Dictionary", "kind": "term" }));
        assert_eq!(options["revision"], 1);
        assert_eq!(setup.storage.read(SELECTIONS_KEY), Some(json!(["bees-ultimate-kanji-dictionary"])));
        // Nothing runs twice: a new run finds it installed.
        let again = install(&setup, &["bees-ultimate-kanji-dictionary"]);
        assert_ne!(again["runId"], started["runId"]);
        assert_eq!(finished(&setup)["entries"][0]["phase"], "already-installed");
    }

    #[test]
    fn a_source_is_installed_by_its_catalogue_source_or_its_update_index() {
        let jmnedict = source("jmnedict");
        assert!(remote::installed(jmnedict, &[json!({ "sourceId": "jmnedict" })]).is_some());
        assert!(remote::installed(jmnedict, &[json!({ "indexUrl": jmnedict.index_url })]).is_some());
        assert!(
            remote::installed(jmnedict, &[json!({ "title": "JMnedict [2026-01-01]", "indexUrl": "https://x.example/n.json" })]).is_none()
        );
    }

    #[test]
    fn a_first_install_option_is_applied_once_and_only_while_automatic() {
        let jitendex = json!({ "id": "j", "title": "Jitendex.org [2026-01-01]", "sourceId": "jitendex", "termCount": 1 });
        let dictionaries = state(&json!([jitendex]));
        let state = &dictionaries["dictionaryState"];
        let chosen = |previous: &[Value], stored: Option<&Value>| {
            first_install_selections(previous, ("jitendex", "installed"), state, stored).unwrap()
        };
        let (applied, options) = chosen(&[], None);
        assert_eq!(applied, ["jitendex"]);
        let options = options.unwrap();
        assert_eq!(
            (&options["compactDefinitionSummaryDictionary"], &options["revision"]),
            (&json!("Jitendex.org [2026-01-01]"), &json!(1))
        );
        // Already applied once, or an option the user chose: nothing changes.
        assert_eq!(chosen(&[json!("jitendex")], None), (Vec::new(), None));
        let (applied, options) = chosen(&[], Some(&json!({ "compactDefinitionSummaryDictionary": "Other", "revision": 4 })));
        assert_eq!((applied, options), (vec!["jitendex"], None));
        // A failed install selects nothing.
        assert_eq!(first_install_selections(&[], ("jitendex", "failed"), state, None).unwrap(), (Vec::new(), None));
    }
}
