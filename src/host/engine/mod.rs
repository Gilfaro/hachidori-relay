// SPDX-License-Identifier: GPL-3.0-or-later
//! The dictionary engine: native Hoshidicts ([`ffi`]) loaded with the mirrored
//! dictionaries, answering the extension's `hoshidicts-offscreen` requests as its
//! `engine-service.js` does (`handleEngineMessage` and its `HANDLERS`).
//!
//! The set loaded is always the stored `dictionaryState`: before every request the
//! engine checks it against what it loaded and reloads if it changed, which is
//! when the generation counter moves on, as the extension's does on publish.

mod ffi;
mod frequency;
mod import;
pub(crate) mod languages;
pub(crate) mod mutations;
pub(crate) mod package;
mod remote;
pub(crate) use remote::{IMPORT_IN_PROGRESS, Progress};

pub(crate) use mutations::MUTATIONS;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use base64::Engine as _;
use serde_json::{Map, Value, json};

use self::ffi::{Kind, Native};
use super::lookup::{furigana, tags};
use super::profile::JAPANESE;
use super::registry;
use super::settings::{custom, profile_worker, profiles, worker};
use super::store::{self, Store};

/// The extension's `CUSTOM_DICTIONARY_TITLE`.
pub(crate) const CUSTOM_DICTIONARY_TITLE: &str = "Hachidori Custom Dictionary";
/// The request types the engine answers (`FORWARDED_REQUESTS`' read-only engine rows).
pub(crate) const REQUESTS: [&str; 9] =
    ["hd_lookup", "hd_lookup_dictionary", "hd_segment", "hd_kanji", "hd_styles", "hd_media", "hd_status", "hd_memory", "hd_memory_total"];

const FREQUENCY_ORDERS: [&str; 4] = ["auto", "ascending", "descending", "disabled"];
const DEFAULT_MAX_RESULTS: i32 = 32;
const DEFAULT_SCAN_LENGTH: i32 = 16;
const MAX_LOOKUP_TEXT_BYTES: usize = 4 * 1024;
const MAX_MEDIA_DICTIONARY_BYTES: usize = 1024;
const MAX_MEDIA_PATH_BYTES: usize = 4 * 1024;
const MAX_LOOKUP_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_MEDIA_RESPONSE_BYTES: usize = 6 * 1024 * 1024;
/// `response-limits.js`' `BOUNDED_REQUESTS`, as far as the engine answers them.
const BOUNDED: [&str; 5] = ["hd_lookup", "hd_lookup_dictionary", "hd_segment", "hd_kanji", "hd_media"];
/// The files a loaded package keeps resident (`INDEX_FILES` and the rest of `residentFiles`).
const INDEX_FILES: [&str; 3] = ["bloom.filter", "media.idx", "dict.zstd"];

/// A handler's failure: its message and, where the extension sets one, its code.
struct Failure(String);

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self(message.to_owned())
    }
}

type Reply = Result<Map<String, Value>, Failure>;

/// A loaded set: its packages, how many kinds were added, and what would not load.
type Candidate = (Vec<Package>, usize, Vec<Value>);

/// One loaded package (`loadedPackages`' entries).
struct Package {
    id: Value,
    title: String,
    /// The browser's generation path, which the dictionary state names it by.
    path: String,
    /// Where it is installed here, which the native engine knows it by.
    dir: String,
    kinds: Vec<&'static str>,
    index_paged: bool,
}

/// What is loaded.
#[derive(Default)]
struct Loaded {
    generation: u64,
    /// The `dictionaryState` this set was loaded from (`None` before the first load).
    state: Option<Value>,
    packages: Vec<Package>,
    dictionary_count: usize,
    failures: Vec<Value>,
    last_load_path: Option<&'static str>,
    error: Option<String>,
    banks: Option<(u64, HashMap<String, tags::Bank>)>,
    /// Whether the records loaded have language profiles (`profileRecords`).
    profile_records: bool,
    /// The packs that did not load (`languageFailures`); their lookups fail.
    language_failures: Vec<Value>,
}

/// The relay's engine service.
pub(crate) struct Engine {
    id: u64,
    store: Arc<Store>,
    loaded: Mutex<Loaded>,
    /// One remote import at a time (`stagingImports`).
    importing: Mutex<()>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl Engine {
    pub(crate) fn new(store: Arc<Store>) -> Self {
        Self { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), store, loaded: Mutex::default(), importing: Mutex::default() }
    }

    fn lock(&self) -> MutexGuard<'_, Loaded> {
        self.loaded.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `handleEngineMessage`: the reply envelope for one request. Blocks while the
    /// engine works, so callers run it off the async runtime.
    pub(crate) fn handle(&self, message: &Value) -> Value {
        let kind = text(message.get("type")).to_owned();
        let mut request_id = message.get("requestId").cloned().unwrap_or(Value::Null);
        if kind == "hd_segment" && valid_request_id(&request_id) {
            // Locks the engine for one chunk at a time.
            let result = self.segment(message);
            return finish(&kind, &request_id, result, &self.lock());
        }
        let mut loaded = self.lock();
        if !REQUESTS.contains(&kind.as_str()) {
            let shown = if kind.is_empty() { "hd_unknown" } else { &kind };
            return json!({"type": format!("{shown}_result"), "requestId": request_id, "ok": false,
                "error": format!("unknown request type {}", Value::from(kind.as_str())), "generation": loaded.generation});
        }
        if kind == "hd_memory_total" {
            drop(loaded);
            return self.memory_total(&request_id);
        }
        let bounded = BOUNDED.contains(&kind.as_str());
        let result = if bounded && !valid_request_id(&request_id) {
            request_id = Value::Null;
            Err(Failure::from("request ID must be a string, finite number, or null"))
        } else {
            let mut native = Native::lock();
            self.answer(&kind, message, &mut native, &mut loaded)
        };
        finish(&kind, &request_id, result, &loaded)
    }

    fn answer(&self, kind: &str, message: &Value, native: &mut Native, loaded: &mut Loaded) -> Reply {
        if kind == "hd_status" {
            // The extension retries a failed reload on a status poll; so does this.
            let _ = self.ensure_loaded(native, loaded);
            return Ok(status(loaded));
        }
        self.ensure_loaded(native, loaded)?;
        match kind {
            "hd_lookup" => {
                let (text, max, scan, options) = lookup_arguments(message, loaded)?;
                let json = native.lookup(text, max, scan, &options).map_err(|error| format!("hdw_lookup: {error}"))?;
                post_process(&json, "hdw_lookup", message, native, loaded)
            }
            "hd_lookup_dictionary" => {
                let (text, max, scan, options) = lookup_arguments(message, loaded)?;
                let title = self::text(message.get("dictionary"));
                let Some(package) = loaded.packages.iter().find(|entry| entry.title == title && entry.kinds.contains(&"term")) else {
                    return Ok(object(json!({"results": [], "dictionaryCount": loaded.dictionary_count})));
                };
                let scan = usize::try_from(scan).unwrap_or(0);
                let json = native
                    .lookup_dictionary(text, &package.dir, max, scan, &options)
                    .map_err(|error| format!("hdw_lookup_dictionary: {error}"))?;
                post_process(&json, "hdw_lookup_dictionary", message, native, loaded)
            }
            "hd_kanji" => {
                let character = bounded_text(message.get("character"), "kanji text", MAX_LOOKUP_TEXT_BYTES, true)?;
                if character.is_empty() {
                    return Ok(object(json!({"kanji": null})));
                }
                // A character lookup of a profile reads only its dictionaries.
                let json = match language_lookup_options(message, loaded)? {
                    None => native.kanji(character).map_err(|error| format!("hdw_kanji: {error}"))?,
                    Some(options) => native
                        .kanji_with_options(character, &Value::Object(options).to_string())
                        .map_err(|error| format!("hdw_kanji_with_options: {error}"))?,
                };
                let kanji = parse_json(&json, "hdw_kanji")?;
                let character = kanji.get("character").and_then(Value::as_str);
                if character.is_none() || !kanji.get("entries").is_some_and(Value::is_array) {
                    return Err("hdw_kanji returned a malformed lookup response".into());
                }
                let kanji = if character == Some("") { Value::Null } else { kanji };
                Ok(object(owned_json!({"kanji": kanji})))
            }
            "hd_styles" => {
                let json = native.styles().map_err(|error| format!("hdw_styles: {error}"))?;
                let styles = parse_json(&json, "hdw_styles")?;
                Ok(object(json!({"styles": if styles.is_array() { styles } else { json!([]) }})))
            }
            "hd_media" => media(message, native, loaded),
            "hd_memory" => Ok(memory(native, loaded)),
            _ => Err(format!("unknown request type {}", Value::from(kind)).into()),
        }
    }

    /// `hd_segment`: each chunk's spans, one engine turn per chunk, so a hover's
    /// lookup that arrives meanwhile runs between a page's chunks rather than after
    /// all of them. Every chunk is checked before the first turn.
    fn segment(&self, message: &Value) -> Reply {
        let scan = clamp_int(message.get("scanLength"), 1, 64, DEFAULT_SCAN_LENGTH);
        let mut extra = Map::new();
        if personal_dictionary_off(message) {
            extra.insert("excludedDictionary".to_owned(), Value::from(personal_title(message)));
        }
        extra.extend(language_lookup_options(message, &self.lock())?.unwrap_or_default());
        let options = lookup_options(message.get("options"), extra)?;
        let chunks = message.get("chunks").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        let mut checked = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let id = chunk.get("id").cloned().unwrap_or(Value::Null);
            checked.push((id, bounded_text(chunk.get("text"), "segment text", MAX_LOOKUP_TEXT_BYTES, true)?));
        }
        let mut segments = Vec::with_capacity(checked.len());
        for (id, text) in checked {
            let spans = self.segment_chunk(text, scan, &options)?;
            segments.push(owned_json!({"id": id, "spans": spans}));
        }
        Ok(object(owned_json!({"segments": segments})))
    }

    /// One chunk's spans, in an engine turn of its own.
    fn segment_chunk(&self, text: &str, scan: i32, options: &str) -> Result<Value, Failure> {
        let mut loaded = self.lock();
        let mut native = Native::lock();
        self.ensure_loaded(&mut native, &mut loaded)?;
        drop(loaded);
        if text.is_empty() {
            return Ok(json!([]));
        }
        let json = native.segment(text, scan, options).map_err(|error| format!("hdw_segment: {error}"))?;
        drop(native);
        match parse_json(&json, "hdw_segment")?.get_mut("spans").map(Value::take) {
            Some(spans @ Value::Array(_)) => Ok(spans),
            _ => Err("hdw_segment returned a malformed response".into()),
        }
    }

    /// Loads the stored `dictionaryState` if it is not what is loaded (`ensureLoaded`
    /// and `reloadFromStorage`).
    fn ensure_loaded(&self, native: &mut Native, loaded: &mut Loaded) -> Result<(), Failure> {
        // Compared where it is stored: a lookup should not copy the dictionary list.
        let current = loaded.state.as_ref().is_some_and(|state| self.store.holds(store::DICTIONARY_STATE, state));
        if current && loaded.error.is_none() && native.owner == Some((self.id, loaded.generation)) {
            return Ok(());
        }
        let state = self.store.get(store::DICTIONARY_STATE).unwrap_or(Value::Null);
        let changed = loaded.state.as_ref() != Some(&state);
        self.reload(native, loaded, state, changed)
    }

    /// Loads `state` and makes it what is loaded, a new generation when `publish`.
    fn reload(&self, native: &mut Native, loaded: &mut Loaded, state: Value, publish: bool) -> Result<(), Failure> {
        // Packs load before the dictionaries publish, so no lookup runs without
        // its language (`reconcile`).
        loaded.profile_records = profiles::has_dictionary_profiles(Some(&state));
        loaded.language_failures = if loaded.profile_records {
            let languages = profile_worker::distinct_languages(self.store.get(worker::OPTIONS_KEY).as_ref());
            load_language_packs(native, &languages)
        } else {
            Vec::new()
        };
        let dictionaries = engine_dictionaries(&state);
        match self.load(native, &dictionaries, None) {
            Ok(candidate) => {
                self.install(native, loaded, candidate, state, publish);
                Ok(())
            }
            Err(error) => {
                loaded.error = Some(error.clone());
                native.owner = None;
                Err(Failure(error))
            }
        }
    }

    /// Makes a loaded `candidate` for `state` what is loaded (`publishLoadedDictionaries`
    /// when `publish`, which starts a new generation).
    fn install(&self, native: &mut Native, loaded: &mut Loaded, candidate: Candidate, state: Value, publish: bool) {
        let (packages, count, failures) = candidate;
        if publish || loaded.state.is_none() {
            loaded.generation += 1;
        }
        loaded.packages = packages;
        loaded.dictionary_count = count;
        loaded.failures = failures;
        loaded.profile_records = profiles::has_dictionary_profiles(Some(&state));
        loaded.state = Some(state);
        loaded.last_load_path = Some("full");
        loaded.error = None;
        loaded.banks = None;
        native.owner = Some((self.id, loaded.generation));
    }

    /// `loadDictionaries`: loads every enabled, installed package in its order. A
    /// package at a `tolerated` path (every path when `None`) that will not load is
    /// left out and reported, so one broken dictionary does not stop the others; any
    /// other package, enabled or not, must load.
    fn load(&self, native: &mut Native, dictionaries: &[Value], tolerated: Option<&[&str]>) -> Result<Candidate, String> {
        let root = self.store.root().join("dictionaries");
        native.init_storage(&root.to_string_lossy()).map_err(|error| format!("hdw_init_storage: {error}"))?;
        let may_fail = |path: &str| tolerated.is_none_or(|paths| paths.contains(&path));
        let add = |native: &mut Native, dictionary: &Value, dir: &str| -> Result<usize, String> {
            let kinds = kinds_for_package(dictionary);
            for &kind in &kinds {
                let added = if Path::new(dir).is_dir() {
                    native.add_dict(dir, kind_of(kind), false, false)
                } else {
                    // What hdw_add_dict says of a missing directory, which the browser names by its path.
                    Err(format!("not an imported dictionary directory: {}", text(dictionary.get("path"))))
                };
                if let Err(error) = added {
                    let error = if error.is_empty() { "unknown engine error" } else { &error };
                    return Err(format!("could not load {} as {kind}: {error}", text(dictionary.get("path"))));
                }
            }
            Ok(kinds.len())
        };
        // A disabled package a mutation introduces must prove it loads too.
        for dictionary in dictionaries.iter().filter(|entry| entry.get("enabled") == Some(&Value::Bool(false))) {
            let path = text(dictionary.get("path"));
            if !may_fail(path) {
                native.reset();
                add(native, dictionary, &self.store.dictionary_dir(path).to_string_lossy())?;
            }
        }
        let enabled: Vec<&Value> = dictionaries.iter().filter(|entry| entry.get("enabled") != Some(&Value::Bool(false))).collect();
        let mut failures = Vec::new();
        let mut skipped = vec![false; enabled.len()];
        loop {
            native.reset();
            let mut packages = Vec::new();
            let mut count = 0;
            let mut failed = None;
            for (index, dictionary) in enabled.iter().enumerate() {
                if skipped[index] {
                    continue;
                }
                let path = text(dictionary.get("path")).to_owned();
                let dir = self.store.dictionary_dir(&path).to_string_lossy().into_owned();
                match add(native, dictionary, &dir) {
                    Ok(kinds) => count += kinds,
                    Err(error) if may_fail(&path) => {
                        failed = Some((index, error));
                        break;
                    }
                    Err(error) => return Err(error),
                }
                let index_paged = native.hash_index_paged(&dir);
                packages.push(Package {
                    id: dictionary.get("id").filter(|id| id.is_string()).cloned().unwrap_or(Value::Null),
                    title: text(dictionary.get("title")).to_owned(),
                    path,
                    dir,
                    kinds: kinds_for_package(dictionary),
                    index_paged,
                });
            }
            let Some((index, error)) = failed else { return Ok((packages, count, failures)) };
            skipped[index] = true;
            let dictionary = enabled[index];
            failures.push(json!({
                "id": dictionary.get("id").filter(|id| id.is_string()).cloned().unwrap_or(Value::Null),
                "title": text(dictionary.get("title")),
                "error": error,
            }));
        }
    }

    /// `measureExtensionMemory` (offscreen.js): the engine's memory, as `hd_memory` reports it.
    fn memory_total(&self, request_id: &Value) -> Value {
        let mut loaded = self.lock();
        let mut native = Native::lock();
        let heap = self.ensure_loaded(&mut native, &mut loaded).ok().map(|()| memory(&native, &loaded)["heapBytes"].clone());
        drop((native, loaded));
        let heap = heap.filter(Value::is_number).unwrap_or(Value::Null);
        json!({"type": "hd_memory_total_result", "requestId": request_id, "ok": true, "bytes": heap, "heapBytes": heap})
    }
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn envelope(kind: &str, request_id: &Value, generation: u64) -> Map<String, Value> {
    object(json!({"type": format!("{kind}_result"), "requestId": request_id, "ok": true, "error": null, "generation": generation}))
}

/// The reply to `result` in its envelope, with the generation of what is loaded
/// and a bounded request's reply limit.
fn finish(kind: &str, request_id: &Value, result: Reply, loaded: &Loaded) -> Value {
    let bounded = BOUNDED.contains(&kind);
    let reply = result.map(|payload| {
        let mut reply = envelope(kind, request_id, loaded.generation);
        reply.extend(payload);
        reply
    });
    let reply = match reply {
        Ok(reply) if bounded && !fits(kind, &reply) => Err(Failure(limit_error(kind).to_owned())),
        other => other,
    };
    match reply {
        Ok(reply) => Value::Object(reply),
        Err(Failure(error)) => Value::Object(failure_reply(kind, request_id, &error, loaded)),
    }
}

/// `engineFailureReply` with `failurePayload`'s defaults.
fn failure_reply(kind: &str, request_id: &Value, error: &str, loaded: &Loaded) -> Map<String, Value> {
    let mut reply = object(
        json!({"type": format!("{kind}_result"), "requestId": request_id, "ok": false, "error": error, "generation": loaded.generation}),
    );
    let payload = match kind {
        "hd_lookup" | "hd_lookup_dictionary" => json!({"results": [], "dictionaryCount": 0}),
        "hd_segment" => json!({"segments": []}),
        "hd_kanji" => json!({"kanji": null}),
        "hd_styles" => json!({"styles": []}),
        "hd_media" => json!({"dataUrl": null}),
        "hd_reload" => json!({"dictionaryCount": 0}),
        "hd_status" => json!({"ready": false, "loading": false, "dictionaryCount": 0, "generation": loaded.generation}),
        _ => json!({}),
    };
    reply.extend(object(payload));
    reply
}

fn valid_request_id(id: &Value) -> bool {
    id.is_null() || id.is_string() || id.as_f64().is_some_and(f64::is_finite)
}

fn limit_error(kind: &str) -> &'static str {
    if kind == "hd_media" {
        "media response exceeds the 6 MiB serialized limit"
    } else {
        "lookup response exceeds the 32 MiB serialized limit"
    }
}

/// `responseFits`: whether the serialized reply is within its frame limit. Its
/// length is counted as it is written, never held, and given up past the limit.
fn fits(kind: &str, reply: &Map<String, Value>) -> bool {
    let limit = if kind == "hd_media" { MAX_MEDIA_RESPONSE_BYTES } else { MAX_LOOKUP_RESPONSE_BYTES };
    serde_json::to_writer(Counter { written: 0, limit }, reply).is_ok()
}

/// A writer that only counts, and fails once past `limit` bytes.
struct Counter {
    written: usize,
    limit: usize,
}

impl std::io::Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.written += bytes.len();
        if self.written > self.limit {
            return Err(std::io::Error::other("over the limit"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The extension's `text`: a string, or "" for anything else.
fn text(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or_default()
}

/// `boundedText`.
fn bounded_text<'a>(value: Option<&'a Value>, label: &str, max_bytes: usize, c_string: bool) -> Result<&'a str, Failure> {
    let result = text(value);
    if c_string && result.contains('\0') {
        return Err(format!("{label} contains NUL").into());
    }
    if result.len() > max_bytes {
        return Err(format!("{label} exceeds the {max_bytes}-byte limit").into());
    }
    Ok(result)
}

/// `clampInt`.
fn clamp_int(value: Option<&Value>, min: i32, max: i32, fallback: i32) -> i32 {
    let number = super::js::to_number(value).trunc();
    if !number.is_finite() {
        return fallback;
    }
    #[allow(clippy::cast_possible_truncation, reason = "clamped into i32 range first")]
    let clamped = number.clamp(f64::from(min), f64::from(max)) as i32;
    clamped
}

/// `engineState`'s dictionaries: with profiles, every package of the record,
/// enabled when a profile enables it (`allPackages`).
fn engine_dictionaries(state: &Value) -> Vec<Value> {
    if profiles::has_dictionary_profiles(Some(state)) {
        return profiles::all_packages(Some(state));
    }
    state.get("dictionaries").and_then(Value::as_array).cloned().unwrap_or_default()
}

/// `loadLanguagePacks(codes, { all: true })`: loads every pack of `languages`
/// that is not loaded; the ones that fail are reported and the others keep working.
fn load_language_packs(native: &mut Native, languages: &[String]) -> Vec<Value> {
    languages
        .iter()
        .filter_map(|code| languages::load(native, code).err().map(|error| json!({ "language": code, "error": error })))
        .collect()
}

/// `languageLookupOptions`: once the records have profiles, the worker names each
/// lookup's language, its profile's enabled package ids and its pack settings'
/// engine effects. The ids map to the directories this engine has loaded. A
/// request without them is today's: no keys are added.
fn language_lookup_options(message: &Value, loaded: &Loaded) -> Result<Option<Map<String, Value>>, Failure> {
    if message.get("language").is_none() {
        return Ok(None);
    }
    let language = text(message.get("language"));
    if loaded.language_failures.iter().any(|entry| entry["language"] == language) {
        return Err(format!("{} could not be loaded.", registry::name(language)).into());
    }
    let ids = message.get("dictionaries").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let dictionaries: Vec<Value> = ids
        .iter()
        .filter_map(|id| loaded.packages.iter().find(|entry| !entry.id.is_null() && &entry.id == id))
        .map(|entry| Value::from(entry.dir.as_str()))
        .collect();
    let options = message.get("languageOptions").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    Ok(Some(object(json!({
        "language": language,
        "dictionaries": dictionaries,
        "languageOptions": options.iter().filter(|option| option.is_string()).collect::<Vec<_>>(),
    }))))
}

/// `personalTitle`: the title of the personal package of the language a request is for.
fn personal_title(message: &Value) -> String {
    let language = message.get("language").and_then(Value::as_str).unwrap_or(JAPANESE);
    custom::identity(language).map_or_else(|| CUSTOM_DICTIONARY_TITLE.to_owned(), |identity| identity.title)
}

/// `kanaRuby`: the KANJIDIC furigana split runs for packs whose ruby is anchored
/// on kana, and for requests that name no language.
fn kana_ruby(message: &Value) -> bool {
    match message.get("language") {
        None => true,
        Some(language) if language == JAPANESE => true,
        Some(language) => language
            .as_str()
            .and_then(registry::get)
            .and_then(|descriptor| descriptor.pointer("/capabilities/ruby/alignment"))
            .is_some_and(|alignment| alignment == "kana"),
    }
}

/// `lookupOptionsJson`.
fn lookup_options(options: Option<&Value>, extra: Map<String, Value>) -> Result<String, Failure> {
    let field = |name: &str| options.and_then(|options| options.get(name));
    let frequency_order = text(field("frequencyOrder"));
    let mut json = json!({
        "frequencyDictionary": bounded_text(field("frequencyDictionary"), "frequency dictionary", MAX_LOOKUP_TEXT_BYTES, false)?,
        "frequencyOrder": if FREQUENCY_ORDERS.contains(&frequency_order) { frequency_order } else { "auto" },
        "primaryReading": bounded_text(field("primaryReading"), "primary reading", MAX_LOOKUP_TEXT_BYTES, false)?,
        "metadataOnlyTerms": field("metadataOnlyTerms") == Some(&Value::Bool(true)),
    });
    if let Some(map) = json.as_object_mut() {
        map.extend(extra);
    }
    Ok(json.to_string())
}

/// `lookupArguments`.
fn lookup_arguments<'a>(message: &'a Value, loaded: &Loaded) -> Result<(&'a str, i32, i32, String), Failure> {
    Ok((
        bounded_text(message.get("text"), "lookup text", MAX_LOOKUP_TEXT_BYTES, true)?,
        clamp_int(message.get("maxResults"), 1, 256, DEFAULT_MAX_RESULTS),
        clamp_int(message.get("scanLength"), 1, 64, DEFAULT_SCAN_LENGTH),
        lookup_options(message.get("options"), language_lookup_options(message, loaded)?.unwrap_or_default())?,
    ))
}

fn parse_json(json: &str, source: &str) -> Result<Value, Failure> {
    serde_json::from_str(json).map_err(|error| format!("{source} returned malformed JSON: {error}").into())
}

fn personal_dictionary_off(message: &Value) -> bool {
    message.get("options").and_then(|options| options.get("personalDictionary")) == Some(&Value::Bool(false))
}

/// The term-bank kanji dictionary whose term-meta ranks are characters', not words'.
const KANJI_RANKS_DICTIONARY: &str = "Bee's Ultimate Kanji Dictionary";

/// `withoutKanjiWordFrequencies` (#724): Bee's rich kanji cards come from term
/// banks, character ranks included. Those ranks belong to the clicked-kanji
/// route, not to a word's frequencies or their averages, so a lookup drops each
/// result's first group from that dictionary (by its canonical title).
fn without_kanji_word_frequencies(results: &mut [Value]) {
    for result in results {
        let Some(groups) = result.pointer_mut("/term/frequencies").and_then(Value::as_array_mut) else { continue };
        if let Some(index) = groups.iter().position(|group| group.get("dictionary").and_then(Value::as_str) == Some(KANJI_RANKS_DICTIONARY))
        {
            groups.remove(index);
        }
    }
}

/// `withFurigana(withDefinitionTags(withoutPersonalDictionary(termLookupReply(json))))`,
/// and for `hdw_lookup` `withoutKanjiWordFrequencies` first.
fn post_process(json: &str, source: &str, message: &Value, native: &Native, loaded: &mut Loaded) -> Reply {
    let mut parsed = parse_json(json, source)?;
    let count_ok = parsed.get("dictionaryCount").and_then(Value::as_u64).is_some_and(|count| count <= (1 << 53));
    if !parsed.get("results").is_some_and(Value::is_array) || !count_ok {
        return Err(format!("{source} returned a malformed lookup response").into());
    }
    let mut reply = object(parsed.take());
    let Some(Value::Array(results)) = reply.get_mut("results") else { return Ok(reply) };
    if source == "hdw_lookup" {
        without_kanji_word_frequencies(results);
    }
    if personal_dictionary_off(message) {
        let title = personal_title(message);
        results.retain_mut(|result| {
            let Some(glossaries) = result.pointer_mut("/term/glossaries").and_then(Value::as_array_mut) else { return true };
            let before = glossaries.len();
            glossaries.retain(|glossary| glossary.get("dictionary").and_then(Value::as_str) != Some(title.as_str()));
            glossaries.len() == before || !glossaries.is_empty()
        });
    }
    if loaded.banks.as_ref().is_none_or(|(generation, _)| *generation != loaded.generation) {
        let banks = tags::banks(&native.tags().map_err(|error| format!("hdw_tags: {error}"))?)?;
        loaded.banks = Some((loaded.generation, banks));
    }
    let banks = loaded.banks.as_ref().map(|(_, banks)| banks);
    for result in results.iter_mut() {
        for glossary in result.pointer_mut("/term/glossaries").and_then(Value::as_array_mut).into_iter().flatten() {
            let bank = banks.and_then(|banks| banks.get(text(glossary.get("dictionary"))));
            let expanded = tags::expand(glossary.get("definitionTags").and_then(Value::as_str), bank);
            if let Some(glossary) = glossary.as_object_mut() {
                glossary.insert("tags".to_owned(), Value::Array(expanded));
            }
        }
        if let Some(term) = result.get_mut("term").and_then(Value::as_object_mut)
            && kana_ruby(message)
        {
            let furigana = furigana::furigana(text(term.get("expression")), text(term.get("reading")));
            if let Some(furigana) = furigana {
                term.insert("furigana".to_owned(), furigana);
            }
        }
    }
    Ok(reply)
}

/// `mediaPathCandidates`.
fn media_path_candidates(path: &str) -> Vec<String> {
    let mut candidates = vec![path.to_owned()];
    let mut decoded = path.to_owned();
    if path.contains('%')
        && let Some(text) = decode_uri_component(path)
    {
        decoded = text;
        candidates.push(decoded.clone());
    }
    let nfc = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
    let nfd = icu_normalizer::DecomposingNormalizerBorrowed::new_nfd();
    for source in [path.to_owned(), decoded] {
        candidates.push(nfc.normalize(&source).into_owned());
        candidates.push(nfd.normalize(&source).into_owned());
    }
    let mut unique = Vec::new();
    for candidate in candidates {
        if !unique.contains(&candidate) {
            unique.push(candidate);
        }
    }
    unique
}

/// JavaScript's `decodeURIComponent`, `None` where it throws.
fn decode_uri_component(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `mediaType`.
fn media_type(path: &str) -> &'static str {
    let extension = path.rfind('.').map_or(String::new(), |dot| path[dot + 1..].to_lowercase());
    match extension.as_str() {
        "avif" => "image/avif",
        "webp" => "image/webp",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// `hd_media`: one media file as a `data:` URL.
fn media(message: &Value, native: &Native, loaded: &Loaded) -> Reply {
    let generation = message.get("generation").and_then(Value::as_f64);
    #[allow(clippy::cast_precision_loss, reason = "generations stay far below 2^53")]
    let current = loaded.generation as f64;
    #[allow(clippy::float_cmp, reason = "both are integers below 2^53, so exact")]
    if !generation
        .is_some_and(|generation| generation.fract() == 0.0 && generation.abs() <= 9_007_199_254_740_991.0 && generation == current)
    {
        return Err("media generation no longer matches the loaded dictionaries".into());
    }
    let dictionary = bounded_text(message.get("dictionary"), "media dictionary", MAX_MEDIA_DICTIONARY_BYTES, true)?;
    let path = bounded_text(message.get("path"), "media path", MAX_MEDIA_PATH_BYTES, true)?;
    if dictionary.is_empty() || path.is_empty() {
        return Ok(object(json!({"dataUrl": null})));
    }
    for candidate in media_path_candidates(path) {
        if let Some(bytes) = native.media(dictionary, &candidate).map_err(|error| format!("hdw_media: {error}"))? {
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            return Ok(object(json!({"dataUrl": format!("data:{};base64,{data}", media_type(path))})));
        }
    }
    Ok(object(json!({"dataUrl": null})))
}

/// `kindsForPackage`.
fn kinds_for_package(dictionary: &Value) -> Vec<&'static str> {
    let kinds: Vec<&'static str> = [("term", "termCount"), ("freq", "frequencyCount"), ("pitch", "pitchCount"), ("kanji", "kanjiCount")]
        .into_iter()
        .filter(|(_, count)| super::js::to_number(dictionary.get(*count)) > 0.0)
        .map(|(kind, _)| kind)
        .collect();
    if kinds.is_empty() { vec!["term"] } else { kinds }
}

fn kind_of(kind: &str) -> Kind {
    match kind {
        "freq" => Kind::Frequency,
        "pitch" => Kind::Pitch,
        "kanji" => Kind::Kanji,
        _ => Kind::Term,
    }
}

/// `hd_status`, for an engine that is native, unthreaded and keeps every package resident.
fn status(loaded: &Loaded) -> Map<String, Value> {
    let mut status = object(json!({
        "ok": loaded.error.is_none(),
        "error": loaded.error,
        "ready": true,
        "loading": false,
        "dictionaryCount": loaded.dictionary_count,
        "registeredKindCount": loaded.dictionary_count,
        "packageCount": loaded.packages.len(),
        "failedDictionaries": loaded.failures,
        "lastLoadPath": loaded.last_load_path,
        "generation": loaded.generation,
        "storageBackend": "native",
        "threaded": false,
        "lowMemory": false,
        "pagedDictionaries": false,
        "dictionaryEntryStorage": "auto",
        "dictionaryIndexStorage": "auto",
        "useLessRamByDefault": true,
        "hashIndexStorage": "resident",
        "residentHashBudgetBytes": null,
    }));
    if loaded.profile_records {
        status.insert("failedLanguages".to_owned(), Value::from(loaded.language_failures.clone()));
    }
    status
}

fn file_bytes(dir: &str, name: &str) -> u64 {
    fs::metadata(Path::new(dir).join(name)).map_or(0, |metadata| metadata.len())
}

/// `hd_memory`. With no wasm heap, `heapBytes` is what the loaded packages keep
/// resident plus the page cache.
fn memory(native: &Native, loaded: &Loaded) -> Map<String, Value> {
    let page_cache = native.page_cache_bytes();
    let mut resident = 0;
    let dictionaries: Vec<Value> = loaded
        .packages
        .iter()
        .map(|package| {
            let mut names = INDEX_FILES.to_vec();
            if package.kinds.contains(&"term") {
                names.push("scan.idx");
            }
            let other: u64 = names.iter().map(|name| file_bytes(&package.dir, name)).sum();
            let hash = file_bytes(&package.dir, "hash.table");
            let resident_hash = if package.index_paged { 0 } else { hash };
            let entries = file_bytes(&package.dir, "blobs.bin");
            let bytes = resident_hash + other + entries;
            resident += bytes;
            json!({
                "id": package.id, "title": package.title, "path": package.path,
                "hashBytes": hash, "residentHashBytes": resident_hash, "otherResidentBytes": other,
                "residentEntryBytes": entries, "bytes": bytes, "paged": false,
                "hashIndexStorage": if package.index_paged { "paged" } else { "resident" },
            })
        })
        .collect();
    #[allow(clippy::cast_precision_loss, reason = "byte counts stay far below 2^53")]
    let heap = resident as f64 + page_cache;
    let mut reply = object(json!({"heapBytes": heap, "pageCacheBytes": page_cache}));
    reply.extend(object(serde_json::from_str(&native.memory_stats()).unwrap_or(Value::Null)));
    reply.extend(object(json!({
        "dictionaryIndexStorage": "auto",
        "useLessRamByDefault": true,
        "hashIndexStorage": "resident",
        "residentHashBudgetBytes": null,
        "packageCount": dictionaries.len(),
        "registeredKindCount": loaded.dictionary_count,
        "dictionaries": dictionaries,
    })));
    reply
}

#[cfg(test)]
mod tests {
    use super::ffi::Native;
    use super::*;

    #[test]
    fn the_native_engine_links_and_answers_with_nothing_loaded() {
        let mut native = Native::lock();
        native.reset();
        let reply = native.lookup("食べる", 8, 16, "");
        drop(native);
        assert_eq!(reply.as_deref(), Ok(r#"{"results":[],"dictionaryCount":0}"#));
    }

    /// Upstream's engine-lookup smoke step "character ranks from Bee's term banks
    /// stay in clicked-kanji lookups" (#724), on a reply's results.
    #[test]
    fn a_word_lookup_drops_the_kanji_dictionarys_character_ranks() {
        let rank = |dictionary: &str, value: u64| json!({ "dictionary": dictionary, "frequencies": [{ "value": value }] });
        let mut results = vec![
            json!({ "term": { "expression": "食", "frequencies": [rank(KANJI_RANKS_DICTIONARY, 3), rank("Word ranks", 200)] } }),
            json!({ "term": { "expression": "食う", "frequencies": [rank("Word ranks", 900)] } }),
            json!({ "term": { "expression": "x" } }),
        ];
        without_kanji_word_frequencies(&mut results);
        assert_eq!(results[0]["term"]["frequencies"], json!([rank("Word ranks", 200)]));
        assert_eq!(results[1]["term"]["frequencies"], json!([rank("Word ranks", 900)]));
        assert_eq!(results[2], json!({ "term": { "expression": "x" } }));
    }

    #[test]
    fn numbers_and_clamps_follow_javascript() {
        let value = |json: Value| clamp_int(Some(&json), 1, 64, 16);
        assert_eq!(clamp_int(None, 1, 64, 16), 16);
        assert_eq!(value(json!(null)), 1);
        assert_eq!(value(json!("20")), 20);
        assert_eq!(value(json!(" 0x10 ")), 16);
        assert_eq!(value(json!(99.9)), 64);
        assert_eq!(value(json!("abc")), 16);
        assert_eq!(value(json!([7])), 7);
        assert_eq!(value(json!({})), 16);
        assert_eq!(value(json!(true)), 1);
    }

    #[test]
    fn media_paths_are_tried_raw_decoded_and_normalized() {
        assert_eq!(media_path_candidates("a.png"), ["a.png"]);
        assert_eq!(media_path_candidates("%E3%81%8C.png"), ["%E3%81%8C.png", "\u{304c}.png", "\u{304b}\u{3099}.png"]);
        assert_eq!(media_path_candidates("%zz"), ["%zz"]);
        assert_eq!(media_type("x/Y.JPG"), "image/jpeg");
        assert_eq!(media_type("noext"), "application/octet-stream");
    }
}
