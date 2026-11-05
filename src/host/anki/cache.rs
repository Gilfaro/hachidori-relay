// SPDX-License-Identifier: GPL-3.0-or-later
//! The Anki duplicate index (`anki-index-cache.js`): a snapshot of which notes hold
//! each word, kept in `storage.json` under `ankiDuplicateIndex`, refreshed every 30
//! minutes and repaired one word at a time. It is derived state the host owns: it is
//! never mirrored, but its row revision is announced to linked browsers.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::gateway::{Failure, Result};
use super::index::{self, Found, Row, Source};
use super::{Anki, Invoke, positive_id};
use crate::host::js;

/// `ANKI_INDEX_KEY`.
pub(crate) const KEY: &str = "ankiDuplicateIndex";
/// `ANKI_INDEX_REFRESH_MS`.
const REFRESH_MS: f64 = 30.0 * 60.0 * 1000.0;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `Number.isInteger`.
fn integer(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(integer) = value.as_i64() {
        return Some(integer);
    }
    let number = value.as_f64()?;
    #[allow(clippy::cast_possible_truncation, reason = "a whole number within i64's range")]
    (number.fract() == 0.0 && number.abs() < 9.0e18).then_some(number as i64)
}

/// `normalizedRow`.
fn normalized_row(value: &Value) -> Option<Row> {
    let row = value.as_array().filter(|row| row.len() == 3)?;
    let word = row[0].as_str().filter(|word| !word.is_empty())?;
    let mature = row[1].as_bool()?;
    let ids = row[2].as_array()?.iter().map(positive_id).collect::<Option<BTreeSet<i64>>>()?;
    (!ids.is_empty()).then(|| (word.to_owned(), mature, ids.into_iter().collect()))
}

/// `normalizedRows`: sorted, each word once.
fn normalized_rows(value: Option<&Value>) -> Option<Vec<Row>> {
    let mut rows = value?.as_array()?.iter().map(normalized_row).collect::<Option<Vec<Row>>>()?;
    rows.sort_by(|left, right| js::compare(&left.0, &right.0));
    if rows.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return None;
    }
    Some(rows)
}

#[derive(Debug, Clone, PartialEq)]
struct Snapshot {
    source_key: String,
    refreshed_at: f64,
    rows: Vec<Row>,
}

#[derive(Debug, Clone, PartialEq)]
struct Attempt {
    source_key: String,
    started_at: f64,
    finished_at: Option<f64>,
}

/// `cacheState`: the stored index, its damaged parts dropped.
#[derive(Debug, Clone, PartialEq)]
struct State {
    configuration_revision: i64,
    row_revision: i64,
    snapshot: Option<Snapshot>,
    attempt: Option<Attempt>,
}

impl State {
    fn read(value: Option<&Value>) -> Self {
        let state = value.filter(|value| value.get("version").and_then(Value::as_f64) == Some(1.0));
        let get = |key: &str| state.and_then(|state| state.get(key));
        let snapshot = get("snapshot");
        let snapshot = match (
            snapshot.and_then(|snapshot| snapshot.get("sourceKey")).and_then(Value::as_str),
            snapshot.and_then(|snapshot| snapshot.get("refreshedAt")).and_then(Value::as_f64),
            normalized_rows(snapshot.and_then(|snapshot| snapshot.get("rows"))),
        ) {
            (Some(source_key), Some(refreshed_at), Some(rows)) => Some(Snapshot { source_key: source_key.to_owned(), refreshed_at, rows }),
            _ => None,
        };
        let attempt = get("attempt");
        let attempt = match (
            attempt.and_then(|attempt| attempt.get("sourceKey")).and_then(Value::as_str),
            attempt.and_then(|attempt| attempt.get("startedAt")).and_then(Value::as_f64),
        ) {
            (Some(source_key), Some(started_at)) => Some(Attempt {
                source_key: source_key.to_owned(),
                started_at,
                finished_at: attempt.and_then(|attempt| attempt.get("finishedAt")).and_then(Value::as_f64),
            }),
            _ => None,
        };
        Self {
            configuration_revision: integer(get("configurationRevision")).unwrap_or(0),
            row_revision: integer(get("rowRevision")).unwrap_or(0),
            snapshot,
            attempt,
        }
    }

    fn to_json(&self) -> Value {
        let snapshot = self.snapshot.as_ref().map(|snapshot| {
            let rows: Vec<Value> = snapshot.rows.iter().map(|(word, mature, ids)| json!([word, mature, ids])).collect();
            owned_json!({ "sourceKey": snapshot.source_key.clone(), "refreshedAt": js::number(snapshot.refreshed_at), "rows": rows })
        });
        let attempt = self.attempt.as_ref().map(|attempt| {
            let mut value = json!({ "sourceKey": attempt.source_key, "startedAt": js::number(attempt.started_at) });
            if let Some(finished_at) = attempt.finished_at {
                value["finishedAt"] = js::number(finished_at);
            }
            value
        });
        json!({
            "version": 1,
            "configurationRevision": self.configuration_revision,
            "rowRevision": self.row_revision,
            "snapshot": snapshot,
            "attempt": attempt,
        })
    }

    /// `ownsAttempt`.
    fn owns(&self, source: &Source, token: &Token) -> bool {
        self.attempt
            .as_ref()
            .is_some_and(|attempt| attempt.source_key == source.key && attempt.started_at.total_cmp(&token.started_at).is_eq())
    }
}

/// `sourceFor`: the index source of the options' first Template.
pub(crate) fn source_for(options: &Map<String, Value>) -> Option<Source> {
    options.get("anki").and_then(Value::as_object).and_then(index::source)
}

/// `ankiIndexConfigurationChange`: the index after the options change from
/// `previous` to `next`, if the change moves it to another source.
pub(crate) fn configuration_change(previous: &Map<String, Value>, next: &Map<String, Value>, value: Option<&Value>) -> Option<Value> {
    let (before, after) = (source_for(previous), source_for(next));
    if before.map(|source| source.key) == after.map(|source| source.key) {
        return None;
    }
    let mut state = State::read(value);
    state.configuration_revision += 1;
    state.attempt = None;
    Some(state.to_json())
}

/// `indexRevision`.
pub(crate) fn revision(value: Option<&Value>, key: &str) -> i64 {
    value.filter(|value| value.get("version").and_then(Value::as_f64) == Some(1.0)).and_then(|value| integer(value.get(key))).unwrap_or(0)
}

/// What an index change announces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Announced {
    /// Another source: earlier revisions belong to other rows (`revision: null`).
    Source,
    /// Changed rows, at this revision.
    Rows(i64),
}

/// The `word-status` revision an index change announces, if any.
pub(crate) fn announced(previous: Option<&Value>, next: Option<&Value>) -> Option<Announced> {
    if revision(next, "configurationRevision") != revision(previous, "configurationRevision") {
        Some(Announced::Source)
    } else if revision(next, "rowRevision") != revision(previous, "rowRevision") {
        Some(Announced::Rows(revision(next, "rowRevision")))
    } else {
        None
    }
}

/// One index row in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    mature: bool,
    note_ids: Vec<i64>,
}

fn row_map(snapshot: Option<&Snapshot>) -> HashMap<String, Entry> {
    snapshot
        .map(|snapshot| {
            snapshot.rows.iter().map(|(word, mature, ids)| (word.clone(), Entry { mature: *mature, note_ids: ids.clone() })).collect()
        })
        .unwrap_or_default()
}

fn rows_from_map(rows: HashMap<String, Entry>) -> Vec<Row> {
    let mut rows: Vec<Row> = rows.into_iter().map(|(word, entry)| (word, entry.mature, entry.note_ids)).collect();
    rows.sort_by(|left, right| js::compare(&left.0, &right.0));
    rows
}

/// A pull's reservation.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Token {
    started_at: f64,
    configuration_revision: i64,
    row_revision: i64,
}

/// What this process holds of the stored index.
#[derive(Default)]
struct Memory {
    source_key: Option<String>,
    rows: HashMap<String, Entry>,
    row_revision: i64,
}

/// When the next refresh is due, for the timer thread.
#[derive(Default)]
struct Timer {
    when: Option<f64>,
    running: bool,
}

/// The duplicate index of one relay.
#[derive(Default)]
pub(crate) struct Index {
    memory: Mutex<Memory>,
    /// The control queue: one change of the stored index at a time.
    control: Mutex<()>,
    /// The pull in flight.
    active: Mutex<Option<Token>>,
    /// The reservation this process made most recently (`ownStartedAt`).
    own_started_at: Mutex<Option<f64>>,
    timer: Mutex<Timer>,
    timer_changed: Condvar,
}

/// What a lookup knows of one word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lookup {
    pub(crate) word_key: Option<String>,
    pub(crate) mature: bool,
    pub(crate) note_ids: Vec<i64>,
    pub(crate) cached: bool,
}

impl Index {
    /// `install`.
    pub(crate) fn install(&self, value: Option<&Value>) {
        let state = State::read(value);
        let mut memory = lock(&self.memory);
        memory.source_key = state.snapshot.as_ref().map(|snapshot| snapshot.source_key.clone());
        memory.rows = row_map(state.snapshot.as_ref());
        memory.row_revision = state.row_revision;
    }

    /// `local`: the cached row of `expression` under `config`'s source.
    fn local(&self, config: &Map<String, Value>, expression: Option<&Value>) -> (Option<Source>, Lookup) {
        let source = index::source(config);
        let word_key = index::word_key(expression);
        let miss = |word_key| Lookup { word_key, mature: false, note_ids: Vec::new(), cached: false };
        let (Some(source), Some(key)) = (source, word_key.clone()) else { return (None, miss(word_key)) };
        let memory = lock(&self.memory);
        let cached = (memory.source_key.as_deref() == Some(source.key.as_str())).then(|| memory.rows.get(&key)).flatten().cloned();
        drop(memory);
        let lookup = match cached {
            Some(entry) => Lookup { word_key, mature: entry.mature, note_ids: entry.note_ids, cached: true },
            None => miss(word_key),
        };
        (Some(source), lookup)
    }

    /// `peek`.
    pub(crate) fn peek(&self, config: &Map<String, Value>, expression: Option<&Value>) -> Lookup {
        self.local(config, expression).1
    }

    /// `has`: whether the cached row of `expression` is mature.
    pub(crate) fn has(&self, config: &Map<String, Value>, expression: Option<&Value>) -> bool {
        let (source, lookup) = self.local(config, expression);
        source.is_some() && lookup.cached && lookup.mature
    }

    /// `statuses`: each headword's status from the cached rows, and their revision.
    pub(crate) fn statuses(&self, config: &Map<String, Value>, headwords: &[&str]) -> Value {
        let source = index::source(config);
        let keys: Vec<Option<String>> = headwords.iter().map(|headword| index::word_key(Some(&json!(headword)))).collect();
        let memory = lock(&self.memory);
        let revision = memory.row_revision;
        if source.is_none_or(|source| memory.source_key.as_deref() != Some(source.key.as_str())) {
            return json!({ "revision": revision, "statuses": null });
        }
        let statuses: Vec<&str> = keys
            .iter()
            .map(|key| match key.as_ref().and_then(|key| memory.rows.get(key)) {
                None => "unknown",
                Some(entry) if entry.mature => "known",
                Some(_) => "learning",
            })
            .collect();
        drop(memory);
        json!({ "revision": revision, "statuses": statuses })
    }

    /// `find`: the cached row, or (always, when `force`) Anki's own answer, recorded.
    pub(crate) fn find(
        &self,
        anki: &Anki,
        config: &Map<String, Value>,
        expression: Option<&Value>,
        invoke: &Invoke<'_>,
        force: bool,
    ) -> Result<Lookup> {
        let (source, cached) = self.local(config, expression);
        let Some(source) = source else { return Ok(cached) };
        let Some(word_key) = cached.word_key.clone() else { return Ok(cached) };
        if !force && cached.cached {
            return Ok(cached);
        }
        let found = index::lookup_many(invoke, &source, &[expression])?;
        let found = normalized_lookup(found.into_iter().next(), &word_key)?;
        if !found.note_ids.is_empty() {
            let entry = Entry { mature: found.mature, note_ids: found.note_ids.clone() };
            self.update_row(anki, &source, &word_key, move |_| Some(entry));
        } else if force {
            self.update_row(anki, &source, &word_key, |_| None);
        }
        Ok(Lookup { word_key: found.word_key, mature: found.mature, note_ids: found.note_ids, cached: false })
    }

    /// `recordWrite`: the note just written, under its word's row.
    pub(crate) fn record_write(&self, anki: &Anki, config: &Map<String, Value>, expression: Option<&Value>, note_id: i64, mature: bool) {
        let (Some(source), Some(word_key)) = (index::source(config), index::word_key(expression)) else { return };
        self.update_row(anki, &source, &word_key, |previous| {
            let mut note_ids = previous.map(|entry| entry.note_ids.clone()).unwrap_or_default();
            note_ids.push(note_id);
            Some(Entry { mature: previous.map_or(mature, |entry| entry.mature), note_ids })
        });
    }

    /// `updateRow`.
    fn update_row(&self, anki: &Anki, source: &Source, word_key: &str, change: impl FnOnce(Option<&Entry>) -> Option<Entry>) {
        let _control = lock(&self.control);
        let mut change = Some(change);
        let mut changed = false;
        let saved = anki.update_index(|options, value| {
            let mut current = State::read(value);
            if source_for(options).map(|source| source.key) != Some(source.key.clone()) {
                return None;
            }
            let same_source = current.snapshot.as_ref().is_some_and(|snapshot| snapshot.source_key == source.key);
            let mut rows = if same_source { row_map(current.snapshot.as_ref()) } else { HashMap::new() };
            let previous = rows.get(word_key).cloned();
            let next = (change.take()?)(previous.as_ref());
            match &next {
                None => {
                    rows.remove(word_key);
                }
                Some(entry) => {
                    let ids: BTreeSet<i64> = entry.note_ids.iter().copied().collect();
                    rows.insert(word_key.to_owned(), Entry { mature: entry.mature, note_ids: ids.into_iter().collect() });
                }
            }
            // `sameRow`, of the row as it was and as the change returned it.
            if previous == next {
                return None;
            }
            changed = true;
            current.row_revision += 1;
            let refreshed_at = current.snapshot.as_ref().filter(|_| same_source).map_or_else(js::now, |snapshot| snapshot.refreshed_at);
            current.snapshot = Some(Snapshot { source_key: source.key.clone(), refreshed_at, rows: rows_from_map(rows) });
            Some(current.to_json())
        });
        if changed {
            self.install(saved.as_ref());
        }
    }

    /// `reconcile`: starts the refresh that is due, or arms the timer for it.
    pub(crate) fn reconcile(anki: &Arc<Anki>) {
        let anki = Arc::clone(anki);
        let spawned = std::thread::Builder::new().name("anki-index".to_owned()).spawn(move || {
            let job = {
                let _control = lock(&anki.index.control);
                anki.index.start_due_refresh(&anki)
            };
            if let Some((source, token)) = job {
                anki.index.pull(&anki, &source, token);
            }
        });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start the Anki index refresh");
        }
    }

    /// `startDueRefresh`: the pull to run now, reserved, if one is due.
    fn start_due_refresh(&self, anki: &Arc<Anki>) -> Option<(Source, Token)> {
        let Some(source) = source_for(&anki.options()) else {
            self.schedule(anki, None);
            return None;
        };
        if lock(&self.active).is_some() {
            return None;
        }
        let state = State::read(anki.index_state().as_ref());
        let due = self.refresh_due(state.attempt.as_ref(), &source);
        if due > js::now() {
            self.schedule(anki, Some(due));
            return None;
        }
        let started_at = js::now();
        let mut token = None;
        anki.update_index(|options, value| {
            let mut current = State::read(value);
            if source_for(options).map(|source| source.key) != Some(source.key.clone())
                || self.refresh_due(current.attempt.as_ref(), &source) > js::now()
            {
                return None;
            }
            token = Some(Token { started_at, configuration_revision: current.configuration_revision, row_revision: current.row_revision });
            current.attempt = Some(Attempt { source_key: source.key.clone(), started_at, finished_at: None });
            Some(current.to_json())
        });
        let token = token?;
        *lock(&self.own_started_at) = Some(started_at);
        self.schedule(anki, Some(started_at + REFRESH_MS));
        *lock(&self.active) = Some(token);
        Some((source, token))
    }

    /// `refreshDue`.
    fn refresh_due(&self, attempt: Option<&Attempt>, source: &Source) -> f64 {
        let Some(attempt) = attempt.filter(|attempt| attempt.source_key == source.key) else { return 0.0 };
        if attempt.finished_at.is_none() && Some(attempt.started_at) != *lock(&self.own_started_at) {
            return 0.0;
        }
        attempt.started_at + REFRESH_MS
    }

    /// `pull`: the whole index of `source`, committed if nothing moved meanwhile.
    fn pull(&self, anki: &Arc<Anki>, source: &Source, token: Token) {
        let url = json!(source.url);
        let invoke = anki.invoker(Some(&url), &source.api_key);
        match index::fetch(&invoke, source) {
            Ok(rows) => {
                let _control = lock(&self.control);
                let mut committed = false;
                let saved = anki.update_index(|options, value| {
                    let mut current = State::read(value);
                    if current.configuration_revision != token.configuration_revision
                        || source_for(options).map(|source| source.key) != Some(source.key.clone())
                        || !current.owns(source, &token)
                    {
                        return None;
                    }
                    // A repaired miss or confirmed write after this pull began must
                    // not be erased by a snapshot taken before that change.
                    if current.row_revision != token.row_revision {
                        current.attempt = None;
                        return Some(current.to_json());
                    }
                    committed = true;
                    let now = js::now();
                    current.row_revision += 1;
                    current.snapshot = Some(Snapshot { source_key: source.key.clone(), refreshed_at: now, rows });
                    if let Some(attempt) = current.attempt.as_mut() {
                        attempt.finished_at = Some(now);
                    }
                    Some(current.to_json())
                });
                if committed {
                    self.install(saved.as_ref());
                }
            }
            Err(error) => {
                tracing::warn!(error = error.message, "the Anki index refresh failed");
                // A pull that ran to failure keeps its 30-minute backoff.
                anki.update_index(|_, value| {
                    let mut current = State::read(value);
                    if !current.owns(source, &token) || current.attempt.as_ref().is_some_and(|attempt| attempt.finished_at.is_some()) {
                        return None;
                    }
                    if let Some(attempt) = current.attempt.as_mut() {
                        attempt.finished_at = Some(js::now());
                    }
                    Some(current.to_json())
                });
            }
        }
        {
            let _control = lock(&self.control);
            let mut active = lock(&self.active);
            if *active == Some(token) {
                *active = None;
            }
        }
        // A configuration or row change during the pull may need a replacement now.
        Self::reconcile(anki);
    }

    /// `schedule`: the timer that reconciles when the next refresh is due.
    fn schedule(&self, anki: &Arc<Anki>, when: Option<f64>) {
        let mut timer = lock(&self.timer);
        timer.when = when;
        self.timer_changed.notify_all();
        if timer.running || when.is_none() {
            return;
        }
        timer.running = true;
        drop(timer);
        let anki = Arc::downgrade(anki);
        let spawned = std::thread::Builder::new().name("anki-index-timer".to_owned()).spawn(move || run_timer(&anki));
        if let Err(error) = spawned {
            lock(&self.timer).running = false;
            tracing::warn!(%error, "could not start the Anki index timer");
        }
    }
}

/// The timer thread: waits for the scheduled time, reconciles, and ends when no
/// refresh is scheduled or the relay is gone.
fn run_timer(anki: &Weak<Anki>) {
    loop {
        let Some(strong) = anki.upgrade() else { return };
        let index = &strong.index;
        let mut timer = lock(&index.timer);
        let Some(when) = timer.when else {
            timer.running = false;
            return;
        };
        let wait = when - js::now();
        if wait > 0.0 {
            // Wake at least every minute, so a dropped relay ends this thread.
            let wait = Duration::from_secs_f64((wait / 1000.0).min(60.0));
            drop(index.timer_changed.wait_timeout(timer, wait).unwrap_or_else(PoisonError::into_inner));
            continue;
        }
        timer.when = None;
        drop(timer);
        Index::reconcile(&strong);
    }
}

/// `normalizedLookup`.
fn normalized_lookup(found: Option<Found>, word_key: &str) -> Result<Found> {
    let found = found.filter(|found| found.word_key.as_deref() == Some(word_key));
    let mut found = found.ok_or_else(|| Failure::new("Anki returned an invalid duplicate lookup result."))?;
    found.note_ids = found.note_ids.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_damaged_state_keeps_what_is_valid() {
        let state = State::read(Some(&json!({ "version": 1, "configurationRevision": 2, "rowRevision": 5.0,
            "snapshot": { "sourceKey": "k", "refreshedAt": 1, "rows": [["b", false, [3, 2, 3]], ["a", true, [1]]] },
            "attempt": { "sourceKey": "k" } })));
        assert_eq!((state.configuration_revision, state.row_revision), (2, 5));
        assert_eq!(state.snapshot.unwrap().rows, [("a".to_owned(), true, vec![1]), ("b".to_owned(), false, vec![2, 3])]);
        assert_eq!(state.attempt, None);
        let state = State::read(Some(
            &json!({ "version": 1, "snapshot": { "sourceKey": "k", "refreshedAt": 1, "rows": [["a", true, [1]], ["a", true, [2]]] } }),
        ));
        assert_eq!(state.snapshot, None);
        assert_eq!(State::read(Some(&json!({ "version": 2, "rowRevision": 3 }))).row_revision, 0);
    }

    #[test]
    fn revisions_announce_what_changed() {
        let at = |configuration: i64, rows: i64| json!({ "version": 1, "configurationRevision": configuration, "rowRevision": rows });
        assert_eq!(announced(Some(&at(0, 1)), Some(&at(1, 1))), Some(Announced::Source));
        assert_eq!(announced(Some(&at(0, 1)), Some(&at(0, 2))), Some(Announced::Rows(2)));
        assert_eq!(announced(None, Some(&at(0, 0))), None);
    }
}
