// SPDX-License-Identifier: GPL-3.0-or-later
//! The Anki duplicate index (`anki-index-cache.js`): a snapshot of which notes hold
//! each word, kept in `storage.json` under `ankiDuplicateIndex`, refreshed every 30
//! minutes and repaired one word at a time. It is derived state the host owns: it is
//! never mirrored, but its row revision is announced to linked browsers.
//!
//! Each language profile's first Template names a source (upstream
//! docs/languages.md 4.3 and 12): the record holds one snapshot and attempt
//! while they name at most one, and one of each per source once they name two.
//! Its two revisions cover every snapshot; each source has its own schedule.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::gateway::{Failure, Result};
use super::index::{self, Found, Row, Source};
use super::{Anki, Invoke, OPTIONS_KEY, positive_id, profile};
use crate::host::js;
use crate::host::settings::profiles;

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

impl Snapshot {
    /// `normalizedSnapshot`.
    fn read(value: &Value) -> Option<Self> {
        let source_key = value.get("sourceKey")?.as_str()?.to_owned();
        let refreshed_at = value.get("refreshedAt")?.as_f64()?;
        let rows = normalized_rows(value.get("rows"))?;
        Some(Self { source_key, refreshed_at, rows })
    }

    fn to_json(&self) -> Value {
        let rows: Vec<Value> = self.rows.iter().map(|(word, mature, ids)| json!([word, mature, ids])).collect();
        owned_json!({ "sourceKey": self.source_key.clone(), "refreshedAt": js::number(self.refreshed_at), "rows": rows })
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Attempt {
    source_key: String,
    started_at: f64,
    finished_at: Option<f64>,
}

impl Attempt {
    /// `normalizedAttempt`.
    fn read(value: &Value) -> Option<Self> {
        Some(Self {
            source_key: value.get("sourceKey")?.as_str()?.to_owned(),
            started_at: value.get("startedAt")?.as_f64()?,
            finished_at: value.get("finishedAt").and_then(Value::as_f64),
        })
    }

    fn to_json(&self) -> Value {
        let mut value = json!({ "sourceKey": self.source_key, "startedAt": js::number(self.started_at) });
        if let Some(finished_at) = self.finished_at {
            value["finishedAt"] = js::number(finished_at);
        }
        value
    }
}

/// An entry of one source.
trait Keyed {
    fn source_key(&self) -> &str;
}

impl Keyed for Snapshot {
    fn source_key(&self) -> &str {
        &self.source_key
    }
}

impl Keyed for Attempt {
    fn source_key(&self) -> &str {
        &self.source_key
    }
}

/// `bySource`: the valid entries, one per source key (the first one stays).
fn by_source<T: Keyed>(values: &[Value], read: impl Fn(&Value) -> Option<T>) -> Vec<T> {
    let mut entries: Vec<T> = Vec::new();
    for entry in values.iter().filter_map(read) {
        if !entries.iter().any(|kept| kept.source_key() == entry.source_key()) {
            entries.push(entry);
        }
    }
    entries
}

/// `entryFor`.
fn entry_for<'a, T: Keyed>(entries: &'a [T], key: &str) -> Option<&'a T> {
    entries.iter().find(|entry| entry.source_key() == key)
}

/// `withEntry`: the entries with `key`'s replaced by `entry` (last), or removed.
fn with_entry<T: Keyed>(entries: &mut Vec<T>, key: &str, entry: Option<T>) {
    entries.retain(|item| item.source_key() != key);
    entries.extend(entry);
}

/// `storedState`'s one entry: the current source's, else, in the single
/// record, whichever it held.
fn only<T: Keyed + Clone>(entries: &[T], key: Option<&String>, multi: bool) -> Option<T> {
    key.and_then(|key| entry_for(entries, key)).or_else(|| if multi { None } else { entries.first() }).cloned()
}

/// `cacheState`: the stored index, its damaged parts dropped.
#[derive(Debug, Clone, PartialEq)]
struct State {
    configuration_revision: i64,
    row_revision: i64,
    /// Whether the record holds one entry per source (`snapshots`, `attempts`).
    multi: bool,
    snapshots: Vec<Snapshot>,
    attempts: Vec<Attempt>,
}

impl State {
    fn read(value: Option<&Value>) -> Self {
        let state = value.filter(|value| value.get("version").and_then(Value::as_f64) == Some(1.0));
        let get = |key: &str| state.and_then(|state| state.get(key));
        let multi = get("snapshots").is_some_and(Value::is_array) || get("attempts").is_some_and(Value::is_array);
        let listed = |many: &str, one: &str| -> Vec<Value> {
            if multi {
                get(many).and_then(Value::as_array).cloned().unwrap_or_default()
            } else {
                vec![get(one).cloned().unwrap_or(Value::Null)]
            }
        };
        Self {
            configuration_revision: integer(get("configurationRevision")).unwrap_or(0),
            row_revision: integer(get("rowRevision")).unwrap_or(0),
            multi,
            snapshots: by_source(&listed("snapshots", "snapshot"), Snapshot::read),
            attempts: by_source(&listed("attempts", "attempt"), Attempt::read),
        }
    }

    /// `storedState`: the record to store for the current sources, `keys` in
    /// profile order. One source keeps the single record, which also keeps a
    /// snapshot no Template uses any more until the next pull replaces it;
    /// several keep exactly the current sources' entries.
    fn stored(&self, keys: &[String]) -> Value {
        if keys.len() > 1 {
            let snapshots: Vec<Value> = keys.iter().filter_map(|key| entry_for(&self.snapshots, key)).map(Snapshot::to_json).collect();
            let attempts: Vec<Value> = keys.iter().filter_map(|key| entry_for(&self.attempts, key)).map(Attempt::to_json).collect();
            return json!({
                "version": 1,
                "configurationRevision": self.configuration_revision,
                "rowRevision": self.row_revision,
                "snapshots": snapshots,
                "attempts": attempts,
            });
        }
        json!({
            "version": 1,
            "configurationRevision": self.configuration_revision,
            "rowRevision": self.row_revision,
            "snapshot": only(&self.snapshots, keys.first(), self.multi).map(|snapshot| snapshot.to_json()),
            "attempt": only(&self.attempts, keys.first(), self.multi).map(|attempt| attempt.to_json()),
        })
    }

    /// `ownsAttempt`.
    fn owns(&self, source: &Source, token: &Token) -> bool {
        entry_for(&self.attempts, &source.key).is_some_and(|attempt| attempt.started_at.total_cmp(&token.started_at).is_eq())
    }
}

/// `wordStatusSource`: highlighting's source, the config across All of Anki.
fn word_status_source(config: &Map<String, Value>) -> Option<Source> {
    let mut config = config.clone();
    config.insert("duplicateScope".to_owned(), Value::from("all"));
    index::source(&config)
}

/// `profileSources`: each profile's id and its sources, keyed with its
/// language's note-type families: its first Template's, and with highlighting
/// on the All of Anki one too, without widening mining's duplicate checks.
/// Options without profiles are the Japanese profile.
fn profile_sources(options: Option<&Value>) -> Vec<(String, Vec<Source>)> {
    let list: Vec<(String, String)> = if profiles::has_profiles(options) {
        profiles::profiles(options)
            .iter()
            .filter_map(|profile| Some((profile["id"].as_str()?.to_owned(), profile["language"].as_str()?.to_owned())))
            .collect()
    } else {
        vec![(super::super::profile::JAPANESE.to_owned(), super::super::profile::JAPANESE.to_owned())]
    };
    list.into_iter()
        .map(|(id, language)| {
            let projected = profiles::project_options(options, &id, true);
            let anki = projected.get("anki").and_then(Value::as_object).cloned().unwrap_or_default();
            let config = profile::language_config(anki, Some(&language));
            let mut sources = vec![index::source(&config)];
            let highlighting = js::truthy(projected.get("wordHighlightEnabled"));
            if highlighting && config.get("duplicateScope").and_then(Value::as_str) != Some("all") {
                sources.push(word_status_source(&config));
            }
            (id, sources.into_iter().flatten().collect())
        })
        .collect()
}

/// `ankiIndexSources`: every profile's mining and highlighting sources, once
/// per key, in profile order.
fn sources(options: Option<&Value>) -> Vec<Source> {
    let mut sources: Vec<Source> = Vec::new();
    for source in profile_sources(options).into_iter().flat_map(|(_, sources)| sources) {
        if !sources.iter().any(|kept| kept.key == source.key) {
            sources.push(source);
        }
    }
    sources
}

fn source_keys(options: Option<&Value>) -> Vec<String> {
    sources(options).into_iter().map(|source| source.key).collect()
}

/// `ankiIndexConfigurationChange`: the index after the options record changes
/// from `previous` to `next`, if any profile's sources changed, compared
/// profile by profile. The configuration revision moves, and the
/// snapshot of a source no profile uses any more goes. A single source's
/// record also drops its attempt so the new source is pulled at once.
pub(crate) fn configuration_change(previous: Option<&Value>, next: Option<&Value>, value: Option<&Value>) -> Option<Value> {
    let key_of = |sources: Vec<(String, Vec<Source>)>| -> Vec<(String, Vec<String>)> {
        sources.into_iter().map(|(id, sources)| (id, sources.into_iter().map(|source| source.key).collect())).collect()
    };
    let (before, after) = (key_of(profile_sources(previous)), key_of(profile_sources(next)));
    // The last entry for an id, as `new Map(...)` keeps.
    let lookup = |list: &[(String, Vec<String>)], id: &str| -> Vec<String> {
        list.iter().rev().find(|(profile, _)| profile == id).map(|(_, keys)| keys.clone()).unwrap_or_default()
    };
    if before.iter().chain(&after).all(|(id, _)| lookup(&before, id) == lookup(&after, id)) {
        return None;
    }
    let mut state = State::read(value);
    let mut keys: Vec<String> = Vec::new();
    for key in after.into_iter().flat_map(|(_, keys)| keys) {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    state.configuration_revision += 1;
    if !state.multi && keys.len() <= 1 {
        state.attempts.clear();
    }
    Some(state.stored(&keys))
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
    row_revision: i64,
}

/// What this process holds of the stored index: each source's rows, and the
/// one row revision that covers them.
#[derive(Default)]
struct Memory {
    snapshots: HashMap<String, HashMap<String, Entry>>,
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
    /// The pull in flight: one at a time, the next due source follows it.
    active: Mutex<Option<Token>>,
    /// The reservation this process made most recently for each source
    /// (`ownStarted`).
    own_started: Mutex<HashMap<String, f64>>,
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
        memory.snapshots = state.snapshots.iter().map(|snapshot| (snapshot.source_key.clone(), row_map(Some(snapshot)))).collect();
        memory.row_revision = state.row_revision;
    }

    /// `local`: the cached row of `expression` under `config`'s source.
    fn local(&self, config: &Map<String, Value>, expression: Option<&Value>) -> (Option<Source>, Lookup) {
        let source = index::source(config);
        let word_key = index::word_key(expression);
        let miss = |word_key| Lookup { word_key, mature: false, note_ids: Vec::new(), cached: false };
        let (Some(source), Some(key)) = (source, word_key.clone()) else { return (None, miss(word_key)) };
        let memory = lock(&self.memory);
        let cached = memory.snapshots.get(&source.key).and_then(|rows| rows.get(&key)).cloned();
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
    /// The requested profile's first Template across all decks and recognised
    /// note types keys them.
    pub(crate) fn statuses(&self, config: &Map<String, Value>, headwords: &[&str]) -> Value {
        let source = word_status_source(config);
        let keys: Vec<Option<String>> = headwords.iter().map(|headword| index::word_key(Some(&json!(headword)))).collect();
        let memory = lock(&self.memory);
        let revision = memory.row_revision;
        // A source with no cached snapshot is unavailable, not a page of unknown words.
        let Some(rows) = source.and_then(|source| memory.snapshots.get(&source.key)) else {
            return json!({ "revision": revision, "statuses": null });
        };
        let statuses: Vec<&str> = keys
            .iter()
            .map(|key| match key.as_ref().and_then(|key| rows.get(key)) {
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
            self.update_row(anki, &[source], &word_key, move |_| Some(entry.clone()));
        } else if force {
            self.update_row(anki, &[source], &word_key, |_| None);
        }
        Ok(Lookup { word_key: found.word_key, mature: found.mature, note_ids: found.note_ids, cached: false })
    }

    /// `recordWrite`: the note just written, under its word's row, in the
    /// mining source and highlighting's All of Anki one, at one revision.
    pub(crate) fn record_write(&self, anki: &Anki, config: &Map<String, Value>, expression: Option<&Value>, note_id: i64, mature: bool) {
        let (Some(source), Some(word_key)) = (index::source(config), index::word_key(expression)) else { return };
        let mut sources = vec![source];
        if config.get("duplicateScope").and_then(Value::as_str) != Some("all") {
            sources.extend(word_status_source(config));
        }
        self.update_row(anki, &sources, &word_key, |previous| {
            let mut note_ids = previous.map(|entry| entry.note_ids.clone()).unwrap_or_default();
            note_ids.push(note_id);
            Some(Entry { mature: previous.map_or(mature, |entry| entry.mature), note_ids })
        });
    }

    /// `updateRow`: one change of a word's row in each of `sources` the options
    /// still use, published at one row revision.
    fn update_row(&self, anki: &Anki, sources: &[Source], word_key: &str, change: impl Fn(Option<&Entry>) -> Option<Entry>) {
        let _control = lock(&self.control);
        let mut changed = false;
        let saved = anki.update_index(|options, value| {
            changed = false;
            let mut current = State::read(value);
            let keys = source_keys(options);
            for source in sources.iter().filter(|source| keys.contains(&source.key)) {
                let snapshot = entry_for(&current.snapshots, &source.key);
                let refreshed_at = snapshot.map_or_else(js::now, |snapshot| snapshot.refreshed_at);
                let mut rows = row_map(snapshot);
                let previous = rows.get(word_key).cloned();
                let next = change(previous.as_ref());
                // `sameRow`, of the row as it was and as the change returned it.
                if previous == next {
                    continue;
                }
                match &next {
                    None => {
                        rows.remove(word_key);
                    }
                    Some(entry) => {
                        let ids: BTreeSet<i64> = entry.note_ids.iter().copied().collect();
                        rows.insert(word_key.to_owned(), Entry { mature: entry.mature, note_ids: ids.into_iter().collect() });
                    }
                }
                changed = true;
                let snapshot = Snapshot { source_key: source.key.clone(), refreshed_at, rows: rows_from_map(rows) };
                with_entry(&mut current.snapshots, &source.key, Some(snapshot));
            }
            if !changed {
                return None;
            }
            current.row_revision += 1;
            Some(current.stored(&keys))
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

    /// `startDueRefresh`: the pull to run now, reserved, if one is due: the
    /// first due source in profile order, else the timer for the soonest.
    fn start_due_refresh(&self, anki: &Arc<Anki>) -> Option<(Source, Token)> {
        let sources = sources(anki.read(OPTIONS_KEY).as_ref());
        if sources.is_empty() {
            self.schedule(anki, None);
            return None;
        }
        if lock(&self.active).is_some() {
            return None;
        }
        let state = State::read(anki.index_state().as_ref());
        let due_at = |source: &Source| self.refresh_due(entry_for(&state.attempts, &source.key), source);
        let Some(source) = sources.iter().find(|source| due_at(source) <= js::now()).cloned() else {
            let soonest = sources.iter().map(due_at).fold(f64::INFINITY, f64::min);
            self.schedule(anki, Some(soonest));
            return None;
        };
        let started_at = js::now();
        let mut token = None;
        anki.update_index(|options, value| {
            let mut current = State::read(value);
            let keys = source_keys(options);
            if !keys.contains(&source.key) || self.refresh_due(entry_for(&current.attempts, &source.key), &source) > js::now() {
                return None;
            }
            token = Some(Token { started_at, row_revision: current.row_revision });
            let attempt = Attempt { source_key: source.key.clone(), started_at, finished_at: None };
            with_entry(&mut current.attempts, &source.key, Some(attempt));
            Some(current.stored(&keys))
        });
        let token = token?;
        lock(&self.own_started).insert(source.key.clone(), started_at);
        // Another source due sooner is started when this pull ends.
        self.schedule(anki, Some(started_at + REFRESH_MS));
        *lock(&self.active) = Some(token);
        Some((source, token))
    }

    /// `refreshDue`.
    fn refresh_due(&self, attempt: Option<&Attempt>, source: &Source) -> f64 {
        let Some(attempt) = attempt.filter(|attempt| attempt.source_key == source.key) else { return 0.0 };
        if attempt.finished_at.is_none() && Some(&attempt.started_at) != lock(&self.own_started).get(&source.key) {
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
                    let keys = source_keys(options);
                    // A source that left and came back has a new reservation, so a
                    // pull from before either change cannot own it.
                    if !keys.contains(&source.key) || !current.owns(source, &token) {
                        return None;
                    }
                    // A repaired miss or confirmed write after this pull began must
                    // not be erased by a snapshot taken before that change.
                    if current.row_revision != token.row_revision {
                        with_entry(&mut current.attempts, &source.key, None);
                        return Some(current.stored(&keys));
                    }
                    committed = true;
                    let now = js::now();
                    current.row_revision += 1;
                    with_entry(
                        &mut current.snapshots,
                        &source.key,
                        Some(Snapshot { source_key: source.key.clone(), refreshed_at: now, rows }),
                    );
                    let attempt =
                        entry_for(&current.attempts, &source.key).map(|attempt| Attempt { finished_at: Some(now), ..attempt.clone() });
                    with_entry(&mut current.attempts, &source.key, attempt);
                    Some(current.stored(&keys))
                });
                if committed {
                    self.install(saved.as_ref());
                }
            }
            Err(error) => {
                tracing::warn!(error = error.message, "the Anki index refresh failed");
                // A pull that ran to failure keeps its 30-minute backoff.
                anki.update_index(|options, value| {
                    let mut current = State::read(value);
                    let attempt = entry_for(&current.attempts, &source.key).filter(|_| current.owns(source, &token))?;
                    if attempt.finished_at.is_some() {
                        return None;
                    }
                    let attempt = Attempt { finished_at: Some(js::now()), ..attempt.clone() };
                    with_entry(&mut current.attempts, &source.key, Some(attempt));
                    Some(current.stored(&source_keys(options)))
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
        // A configuration or row change during the pull may need a replacement
        // now, and another source may be due.
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
        assert_eq!(state.snapshots[0].rows, [("a".to_owned(), true, vec![1]), ("b".to_owned(), false, vec![2, 3])]);
        assert_eq!(state.attempts, []);
        let state = State::read(Some(
            &json!({ "version": 1, "snapshot": { "sourceKey": "k", "refreshedAt": 1, "rows": [["a", true, [1]], ["a", true, [2]]] } }),
        ));
        assert_eq!(state.snapshots, []);
        assert_eq!(State::read(Some(&json!({ "version": 2, "rowRevision": 3 }))).row_revision, 0);
    }

    /// Upstream's anki-index-languages.test.mjs "with one source, a record
    /// with profiles changes exactly as the single-profile record does".
    #[test]
    fn one_source_on_a_record_with_profiles_changes_as_the_single_record() {
        let template = |id: &str, model: &str, scope: &str| {
            json!({ "id": id, "name": id, "deck": "Default", "model": model, "tags": [],
                "fields": { "expression": "Expression", "reading": "", "definition": "", "sentence": "", "frequency": "",
                    "pitch": "", "audio": "", "screenshot": "" },
                "duplicateScope": scope, "duplicateBehavior": "prevent", "captureScreenshot": false, "fieldTemplates": null })
        };
        let anki = |scope: &str| json!({ "url": "http://127.0.0.1:8765", "apiKey": "", "duplicateScope": scope });
        let flat = |scope: &str| {
            let mut anki = anki(scope);
            anki["templates"] = json!([template("mining", "Japanese", scope)]);
            json!({ "revision": 4, "anki": anki })
        };
        let profiled = |scope: &str| {
            let profile = |id: &str, first: Value| json!({ "id": id, "language": id, "settings": { "anki": { "templates": [first] } }, "packSettings": {} });
            let mut ja = profile("ja", template("mining", "Japanese", scope));
            ja["settings"]["anki"]["duplicateScope"] = json!(scope);
            json!({ "revision": 4, "anki": { "url": "http://127.0.0.1:8765", "apiKey": "" },
                "profiles": [ja, profile("en", template("words", "", "model"))] })
        };
        let run = |record: &dyn Fn(&str) -> Value| {
            let (before, after) = (record("model"), record("all"));
            let keys = source_keys(Some(&before));
            assert_eq!(keys.len(), 1, "one source");
            let index = json!({ "version": 1, "configurationRevision": 2, "rowRevision": 7,
                "snapshot": { "sourceKey": keys[0], "refreshedAt": 1, "rows": [["犬", false, [9]]] },
                "attempt": { "sourceKey": keys[0], "startedAt": 1, "finishedAt": 2, "failures": 0 } });
            let changed = configuration_change(Some(&before), Some(&after), Some(&index)).unwrap();
            (source_keys(Some(&after)), changed)
        };
        let (flat_keys, flat_index) = run(&flat);
        let (profiled_keys, profiled_index) = run(&profiled);
        assert_eq!(flat_keys, profiled_keys);
        assert_eq!(flat_index, profiled_index);
        assert_eq!(flat_index["attempt"], Value::Null, "the new source is pulled at once");
    }

    /// Upstream's anki-index-cache.test.mjs "highlighting indexes all decks
    /// independently of mining scope" and anki-index-languages.test.mjs
    /// "highlighting keeps each profile's all-decks source and language
    /// families separate", on the source lists.
    #[test]
    fn highlighting_adds_an_all_decks_source_beside_mining() {
        let record = |scope: &str, highlight: bool| {
            json!({ "revision": 1, "wordHighlightEnabled": highlight,
                "anki": { "model": "Japanese", "fields": { "expression": "Expression" }, "duplicateScope": scope } })
        };
        let scopes = |options: &Value| sources(Some(options)).into_iter().map(|source| source.scope).collect::<Vec<_>>();
        assert_eq!(scopes(&record("model", false)), ["model"], "disabled highlighting adds no source");
        assert_eq!(scopes(&record("model", true)), ["model", "all"]);
        assert_eq!(scopes(&record("deck", true)), ["deck", "all"]);
        assert_eq!(scopes(&record("all", true)), ["all"], "All of Anki mining shares the same snapshot");
        let all = sources(Some(&record("all", true))).remove(0).key;
        assert_eq!(sources(Some(&record("model", true)))[1].key, all);
        let index = json!({ "version": 1, "configurationRevision": 2, "rowRevision": 7 });
        let enabling = configuration_change(Some(&record("model", false)), Some(&record("model", true)), Some(&index));
        assert_eq!(enabling.unwrap()["configurationRevision"], 3, "enabling highlighting is a new source list");
        assert_eq!(configuration_change(Some(&record("all", false)), Some(&record("all", true)), Some(&index)), None);
        let projected = profiles::project_options(Some(&record("model", true)), "ja", true);
        let config = projected["anki"].as_object().cloned().unwrap();
        assert_eq!(word_status_source(&config).unwrap().key, all, "statuses read the all-decks rows");

        // Each profile's own highlighting, its language's families kept apart.
        let template = |model: &str| {
            json!([{ "id": "t", "name": "t", "deck": "Default", "model": model, "tags": [],
            "fields": { "expression": "Expression" }, "duplicateScope": "model" }])
        };
        let profile = |id: &str, model: &str, highlight: bool| {
            json!({ "id": id, "language": id,
            "settings": { "wordHighlightEnabled": highlight, "anki": { "templates": template(model), "duplicateScope": "model" } },
            "packSettings": {} })
        };
        let profiled = |english: bool| {
            json!({ "revision": 1, "anki": { "url": "http://127.0.0.1:8765", "apiKey": "" },
            "profiles": [profile("ja", "Japanese", true), profile("en", "English", english)] })
        };
        let listed = |options: &Value| sources(Some(options)).into_iter().map(|source| (source.model, source.scope)).collect::<Vec<_>>();
        let pair = |model: &str, scope: &str| (model.to_owned(), scope.to_owned());
        assert_eq!(listed(&profiled(false)), [pair("Japanese", "model"), pair("Japanese", "all"), pair("English", "model")]);
        assert_eq!(
            listed(&profiled(true)),
            [pair("Japanese", "model"), pair("Japanese", "all"), pair("English", "model"), pair("English", "all")]
        );
    }

    #[test]
    fn revisions_announce_what_changed() {
        let at = |configuration: i64, rows: i64| json!({ "version": 1, "configurationRevision": configuration, "rowRevision": rows });
        assert_eq!(announced(Some(&at(0, 1)), Some(&at(1, 1))), Some(Announced::Source));
        assert_eq!(announced(Some(&at(0, 1)), Some(&at(0, 2))), Some(Announced::Rows(2)));
        assert_eq!(announced(None, Some(&at(0, 0))), None);
    }
}
