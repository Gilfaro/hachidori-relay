// SPDX-License-Identifier: GPL-3.0-or-later
//! The host side of the Yomitan-compatible API: the extension's `api-host.js`,
//! answering the `hd_api_*` requests the relay's HTTP API sends to its sharing
//! host from the relay's own engine and store.
//!
//! Like `dispatchSharedRequest`, an answer is the handler's bare result, not an
//! engine envelope, and a failure is the sharing `failureReply`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine as _;
use serde_json::{Map, Value, json};

use super::anki::Anki;
use super::anki::api::escape_html;
use super::archive;
use super::engine::{Engine, package};
use super::js;
use super::lookup::{furigana, tags};
use super::profile::JAPANESE;
use super::settings::{backup, custom, profile_worker, profiles, worker};
use super::store::{self, Store};

/// `API_REQUESTS`.
pub(crate) const REQUESTS: [&str; 10] = [
    "hd_api_version",
    "hd_api_term_entries",
    "hd_api_kanji_entries",
    "hd_api_anki_fields",
    "hd_api_anki_card_formats",
    "hd_api_tokenize",
    "hd_api_dictionaries",
    "hd_api_dictionary_open",
    "hd_api_dictionary_read",
    "hd_api_dictionary_close",
];

/// `UNPROFILED`: downloads stream an archive their open already chose.
const UNPROFILED: [&str; 3] = ["hd_api_version", "hd_api_dictionary_read", "hd_api_dictionary_close"];

const DOWNLOADS_DIR: &str = "downloads";

/// `AUDIO_TYPES`: a pronunciation's media type by its extension.
const AUDIO_TYPES: [(&str, &str); 7] = [
    ("aac", "audio/aac"),
    ("flac", "audio/flac"),
    ("m4a", "audio/mp4"),
    ("mp3", "audio/mpeg"),
    ("ogg", "audio/ogg"),
    ("wav", "audio/wav"),
    ("webm", "audio/webm"),
];

/// The API host.
pub(crate) struct Api {
    engine: Arc<Engine>,
    anki: Arc<Anki>,
    store: Arc<Store>,
    version: String,
    downloads: Mutex<HashMap<String, (PathBuf, u64)>>,
    next_download: AtomicU64,
}

impl Api {
    pub(crate) fn new(engine: Arc<Engine>, anki: Arc<Anki>, store: Arc<Store>, version: &str) -> Self {
        // Archives built for downloads a previous run never closed.
        let _ = fs::remove_dir_all(store.root().join(DOWNLOADS_DIR));
        Self { engine, anki, store, version: version.to_owned(), downloads: Mutex::default(), next_download: AtomicU64::new(0) }
    }

    /// `answerApiRequest`: the result, or the error the sharing host fails it with.
    pub(crate) fn answer(&self, message: &Value) -> Result<Value, String> {
        let kind = message.get("type").and_then(Value::as_str).unwrap_or_default();
        if !REQUESTS.contains(&kind) {
            return Err(format!("unsupported API request {}", Value::from(kind)));
        }
        let profile = if UNPROFILED.contains(&kind) {
            None
        } else {
            // `profileIndex ?? 0`; other requests have always answered from
            // profile 0 whatever index they named.
            let index = message.get("profileIndex").filter(|index| !index.is_null()).cloned().unwrap_or_else(|| json!(0));
            match self.resolve_profile(&index) {
                Some(Selected { profile_id }) => profile_id,
                None if kind == "hd_api_anki_card_formats" => {
                    return Err(format!(
                        "Invalid input for ankiCardFormats, expected \"profileIndex\" to be a valid profile index but got {}",
                        js::to_string(Some(&index))
                    ));
                }
                None => self.resolve_profile(&json!(0)).and_then(|selected| selected.profile_id),
            }
        };
        let profile = profile.as_deref();
        match kind {
            "hd_api_version" => Ok(json!({ "version": self.version })),
            "hd_api_term_entries" => self.term_entries(message, profile),
            "hd_api_kanji_entries" => self.kanji_entries(message, profile),
            "hd_api_tokenize" => self.tokenize(message, profile),
            "hd_api_dictionaries" => Ok(json!({ "dictionaries": self.dictionaries(profile).iter().map(|item| {
                let mut row = Map::new();
                for key in ["id", "title", "revision"] {
                    if let Some(value) = item.get(key) {
                        row.insert(key.to_owned(), value.clone());
                    }
                }
                row.insert("fileName".to_owned(), Value::from(file_name(item.get("title"))));
                Value::Object(row)
            }).collect::<Vec<_>>() })),
            "hd_api_dictionary_open" => self.dictionary_open(message, profile),
            "hd_api_dictionary_read" => self.dictionary_read(message),
            "hd_api_dictionary_close" => {
                let token = message.get("token").and_then(Value::as_str).unwrap_or_default();
                if let Some((path, _)) = self.downloads.lock().unwrap_or_else(PoisonError::into_inner).remove(token) {
                    let _ = fs::remove_file(path);
                }
                Ok(json!({}))
            }
            "hd_api_anki_fields" => self.anki_fields(message, profile),
            // Each profile has its own Templates; Yomitan's profile 0 is Japanese's.
            _ => Ok(json!({ "cardFormats": self.anki.api_card_formats(profile) })),
        }
    }

    /// `apiProfile(profileIndex)`: the profile a Yomitan profile index selects,
    /// or `None` for an index that names none. Index 0 is the Japanese profile,
    /// as every older client of the API expects; later indexes list the other
    /// profiles in order.
    fn resolve_profile(&self, index: &Value) -> Option<Selected> {
        let stored = self.store.get(worker::OPTIONS_KEY);
        // `profileIndex === 0`.
        let zero = index.as_f64() == Some(0.0);
        if !profiles::has_profiles(stored.as_ref()) {
            return zero.then_some(Selected { profile_id: None });
        }
        let listed = profiles::profiles(stored.as_ref());
        let id = |profile: &Value| profile["id"].as_str().map(str::to_owned);
        if zero {
            return Some(Selected { profile_id: listed.iter().find(|profile| profile["id"] == JAPANESE).and_then(id) });
        }
        // `others[profileIndex - 1]`: an array index only for a whole number in range.
        let position = js::to_number(Some(index)) - 1.0;
        let others: Vec<&Value> = listed.iter().filter(|profile| profile["id"] != JAPANESE).collect();
        if position.fract() != 0.0 || position < 0.0 || position >= 4_294_967_295.0 {
            return None;
        }
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a whole array index")]
        others.get(position as usize).map(|profile| Selected { profile_id: id(profile) })
    }

    /// `apiDictionaries`: the profile's dictionary list. The Relay downloads
    /// from the Japanese view (upstream docs/languages.md 17.2), which has no
    /// other language's personal package, so neither does the list.
    fn dictionaries(&self, profile: Option<&str>) -> Vec<Value> {
        let state = self.store.get(store::DICTIONARY_STATE);
        if !profiles::has_dictionary_profiles(state.as_ref()) {
            return state.as_ref().and_then(|state| state.get("dictionaries")).and_then(Value::as_array).cloned().unwrap_or_default();
        }
        let projected = profiles::project_dictionary_state(state.as_ref(), profile.unwrap_or(JAPANESE));
        let rows = projected.as_ref().and_then(|state| state.get("dictionaries")).and_then(Value::as_array).cloned().unwrap_or_default();
        rows.into_iter().filter(|row| custom::personal_language(row).is_none_or(|language| language == JAPANESE)).collect()
    }

    /// One engine request, its envelope or its error (`sendAnkiRequest`).
    fn engine(&self, request: &Value) -> Result<Value, String> {
        let (options, state) = (self.store.get(worker::OPTIONS_KEY), self.store.get(store::DICTIONARY_STATE));
        let request = profile_worker::engine_request(request, options.as_ref(), state.as_ref()).map_err(|refused| refused.error)?;
        let reply = self.engine.handle(&request);
        if reply.get("ok") == Some(&Value::Bool(true)) {
            Ok(reply)
        } else {
            Err(reply
                .get("error")
                .and_then(Value::as_str)
                .filter(|error| !error.is_empty())
                .unwrap_or("Anki preparation did not complete.")
                .to_owned())
        }
    }

    fn lookup(&self, profile: Option<&str>, text: &str, fields: Value) -> Result<Vec<Value>, String> {
        Ok(self.lookup_generation(profile, text, fields)?.0)
    }

    /// `lookup`: the results and the generation they came from.
    fn lookup_generation(&self, profile: Option<&str>, text: &str, fields: Value) -> Result<(Vec<Value>, Value), String> {
        let mut request = named(profile, json!({ "type": "hd_lookup", "text": text, "requestId": "api" }));
        if let (Some(request), Value::Object(fields)) = (request.as_object_mut(), fields) {
            request.extend(fields);
        }
        let mut reply = self.engine(&request)?;
        let generation = reply.get("generation").cloned().unwrap_or(Value::Null);
        let results = reply
            .get_mut("results")
            .map(Value::take)
            .and_then(|results| match results {
                Value::Array(results) => Some(results),
                _ => None,
            })
            .unwrap_or_default();
        Ok((results, generation))
    }

    /// `hd_api_anki_fields`.
    fn anki_fields(&self, message: &Value, profile: Option<&str>) -> Result<Value, String> {
        let text = require_text(message.get("text"), "text")?;
        let markers = require_strings(message.get("markers"), "markers")?;
        let max_entries = message
            .get("maxEntries")
            .and_then(Value::as_f64)
            .filter(|count| count.fract() == 0.0 && *count > 0.0 && *count <= 9_007_199_254_740_991.0)
            .map_or(0, |count| {
                #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a positive safe integer")]
                let count = count as usize;
                count
            });
        let include_media = message.get("includeMedia") == Some(&Value::Bool(true));
        match message.get("entryType") {
            Some(Value::String(kind)) if kind == "kanji" => self.anki_kanji_fields(profile, text, &markers, max_entries),
            Some(Value::String(kind)) if kind == "term" => self.anki_term_fields(profile, text, &markers, max_entries, include_media),
            other => Err(format!("unsupported entry type {}", other.map_or_else(|| "undefined".to_owned(), Value::to_string))),
        }
    }

    /// `ankiTermFields`: each result's fields, as mining would render them.
    fn anki_term_fields(
        &self,
        profile_id: Option<&str>,
        text: &str,
        markers: &[String],
        max_entries: usize,
        include_media: bool,
    ) -> Result<Value, String> {
        let dictionaries = self.dictionaries(profile_id);
        let profile = self.anki.api_profile(profile_id);
        let (mut results, generation) =
            self.lookup_generation(profile_id, text, if max_entries > 0 { json!({ "maxResults": max_entries }) } else { json!({}) })?;
        if max_entries > 0 {
            results.truncate(max_entries);
        }
        let title = |item: &Value| js::to_string(item.get("title"));
        let aliases: Map<String, Value> = dictionaries
            .iter()
            .filter(|item| item.get("displayName").is_some_and(truthy))
            .map(|item| (title(item), item["displayName"].clone()))
            .collect();
        let ids: Map<String, Value> =
            dictionaries.iter().map(|item| (title(item), item.get("id").cloned().unwrap_or(Value::Null))).collect();
        let frequency_dictionaries: Vec<Value> = dictionaries
            .iter()
            .filter(|item| {
                item.get("enabled").is_some_and(truthy)
                    && item.get("frequencyCount").and_then(Value::as_f64).is_some_and(|count| count > 0.0)
            })
            .map(|item| Value::from(title(item)))
            .collect();
        // Each dictionary's target language, which a profile's notes follow
        // (upstream docs/languages.md 12).
        let targets: Option<Map<String, Value>> = profile.as_ref().map(|_| {
            dictionaries
                .iter()
                .filter(|item| item.get("targetLanguage").and_then(Value::as_str).is_some_and(|target| !target.is_empty()))
                .map(|item| (title(item), item["targetLanguage"].clone()))
                .collect()
        });
        let layout = self.anki.api_layout();
        let (mut fields, mut dictionary_media, mut audio_media) = (Vec::new(), Vec::new(), Vec::new());
        let mut seen: Vec<String> = Vec::new();
        for result in &results {
            let mut term = result.get("term").cloned().unwrap_or(Value::Null);
            if let Some(groups) = term.get_mut("frequencies").and_then(Value::as_array_mut) {
                for group in groups {
                    let dictionary = js::to_string(group.get("dictionary"));
                    let mode =
                        dictionaries.iter().find(|item| title(item) == dictionary).and_then(|item| item.get("frequencyMode")).cloned();
                    if let Some(group) = group.as_object_mut() {
                        match mode {
                            Some(mode) => group.insert("frequencyMode".to_owned(), mode),
                            None => group.remove("frequencyMode"),
                        };
                    }
                }
            }
            let mut request = result.clone();
            if let Some(request) = request.as_object_mut() {
                request.insert("term".to_owned(), term.clone());
                request.insert("generation".to_owned(), generation.clone());
                request.insert("sentence".to_owned(), json!(text));
                request.insert("matchOffset".to_owned(), json!(0));
                request.insert("searchQuery".to_owned(), json!(text));
                request.insert("popupSelectionText".to_owned(), json!(""));
                request.insert("documentTitle".to_owned(), json!(""));
                request.insert("dictionaryAliases".to_owned(), Value::Object(aliases.clone()));
                request.insert("dictionaryIds".to_owned(), Value::Object(ids.clone()));
                request.insert("frequencyDictionaries".to_owned(), Value::Array(frequency_dictionaries.clone()));
                if let Some(targets) = &targets {
                    request.insert("dictionaryTargetLanguages".to_owned(), Value::Object(targets.clone()));
                }
            }
            let mut audio = String::new();
            if include_media
                && markers.iter().any(|marker| marker.to_lowercase() == "audio")
                && let Some((filename, data)) =
                    self.anki.api_audio(result.get("term").unwrap_or(&Value::Null), profile_id, profile.as_ref())
            {
                audio = format!("[sound:{filename}]");
                let extension = filename.rsplit('.').next().unwrap_or_default().to_lowercase();
                let media_type =
                    AUDIO_TYPES.iter().find(|(known, _)| *known == extension).map_or("application/octet-stream", |(_, kind)| kind);
                audio_media.push(json!({ "term": term.get("expression"), "reading": term.get("reading"), "mediaType": media_type,
                        "content": data, "ankiFilename": filename }));
            }
            let built = self.anki.api_render(&request, markers, &audio, layout, profile.as_ref())?;
            let shown: Vec<&String> = built.fields.iter().map(|(_, value)| value).collect();
            fields.push(Value::Object(built.fields.iter().map(|(name, value)| (name.clone(), json!(value))).collect()));
            if !include_media {
                continue;
            }
            // Only the images these fields show, as mining uploads only those:
            // Smaller Anki cards leaves hidden content out, with any image in it.
            for (dictionary, path, filename) in &built.media {
                if seen.contains(filename) || !shown.iter().any(|value| value.contains(filename.as_str())) {
                    continue;
                }
                seen.push(filename.clone());
                let Some((media_type, content)) = self.anki.api_media(dictionary, path, Some(&generation)) else { continue };
                dictionary_media.push(owned_json!({ "dictionary": dictionary.as_str(), "path": path.as_str(), "mediaType": media_type,
                    "content": content, "ankiFilename": filename.as_str() }));
            }
        }
        Ok(owned_json!({ "fields": fields, "dictionaryMedia": dictionary_media, "audioMedia": audio_media }))
    }

    /// `ankiKanjiFields`: the first character's entries as kanji markers.
    fn anki_kanji_fields(&self, profile: Option<&str>, text: &str, markers: &[String], max_entries: usize) -> Result<Value, String> {
        let where_ = Where::new(self.dictionaries(profile));
        let character = text.chars().next().map(String::from).unwrap_or_default();
        let reply = self.engine(&named(profile, json!({ "type": "hd_kanji", "character": character, "requestId": "api" })))?;
        let entries = list(reply.pointer("/kanji/entries"));
        let entries = if max_entries > 0 { &entries[..max_entries.min(entries.len())] } else { entries };
        let fields: Vec<Value> = entries.iter().map(|entry| kanji_fields(&character, entry, markers, &where_)).collect();
        Ok(owned_json!({ "fields": fields, "dictionaryMedia": Vec::<Value>::new(), "audioMedia": Vec::<Value>::new() }))
    }

    fn term_entries(&self, message: &Value, profile: Option<&str>) -> Result<Value, String> {
        let terms = require_strings(message.get("terms"), "terms")?;
        let where_ = Where::new(self.dictionaries(profile));
        let mut results = Vec::with_capacity(terms.len());
        for (index, text) in terms.iter().enumerate() {
            let found = if text.is_empty() { Vec::new() } else { self.lookup(profile, text, json!({}))? };
            let original = found.first().map_or(0, |result| utf16_len(text_of(result.get("matched"))));
            let entries: Vec<Value> = found.iter().map(|result| term_entry(result, &where_)).collect();
            results.push(owned_json!({ "index": index, "dictionaryEntries": entries, "originalTextLength": original }));
        }
        Ok(owned_json!({ "results": results }))
    }

    fn kanji_entries(&self, message: &Value, profile: Option<&str>) -> Result<Value, String> {
        let characters = require_strings(message.get("characters"), "characters")?;
        let where_ = Where::new(self.dictionaries(profile));
        let mut results = Vec::with_capacity(characters.len());
        for (index, text) in characters.iter().enumerate() {
            let mut entries = Vec::new();
            for character in text.chars() {
                let character = character.to_string();
                let reply = self.engine(&named(profile, json!({ "type": "hd_kanji", "character": character, "requestId": "api" })))?;
                for entry in reply.pointer("/kanji/entries").and_then(Value::as_array).into_iter().flatten() {
                    entries.push(kanji_entry(&character, entry, &where_));
                }
            }
            results.push(owned_json!({ "index": index, "dictionaryEntries": entries }));
        }
        Ok(owned_json!({ "results": results }))
    }

    fn tokenize(&self, message: &Value, profile: Option<&str>) -> Result<Value, String> {
        let texts = require_strings(message.get("texts"), "texts")?;
        // Number.isSafeInteger(scanLength) && scanLength > 0.
        #[allow(clippy::cast_possible_truncation, reason = "a safe integer")]
        let scan_length = message
            .get("scanLength")
            .and_then(Value::as_f64)
            .filter(|length| length.fract() == 0.0 && *length > 0.0 && *length <= 9_007_199_254_740_991.0)
            .map(|length| length as i64);
        // A language without ruby gives its words no reading (upstream
        // docs/languages.md 12).
        let ruby = self.anki.api_profile(profile).is_none_or(|descriptor| {
            let pack_settings = descriptor["packSettings"].as_object().cloned().unwrap_or_default();
            profiles::option_requirement_met(&Value::from("ruby"), descriptor["language"].as_str(), &pack_settings)
        });
        let mut results = Vec::with_capacity(texts.len());
        for (index, text) in texts.iter().enumerate() {
            let mut lines = Vec::new();
            for line in text.split('\n') {
                lines.push(self.tokenize_line(profile, line, scan_length, ruby)?);
            }
            results.push(
                owned_json!({ "id": "scan", "source": "scanning-parser", "dictionary": Value::Null, "index": index, "content": lines }),
            );
        }
        Ok(owned_json!({ "results": results }))
    }

    /// One line of `tokenize`: the longest match at each position, or one character.
    fn tokenize_line(&self, profile: Option<&str>, line: &str, scan_length: Option<i64>, ruby: bool) -> Result<Vec<Value>, String> {
        let mut segments: Vec<(String, String)> = Vec::new();
        let plain = |segments: &mut Vec<(String, String)>, value: &str| {
            if value.is_empty() {
                return;
            }
            match segments.last_mut() {
                Some((text, reading)) if reading.is_empty() => text.push_str(value),
                _ => segments.push((value.to_owned(), String::new())),
            }
        };
        let mut position = 0;
        while position < line.len() {
            let rest = &line[position..];
            let fields = scan_length.map_or_else(|| json!({ "maxResults": 1 }), |scan| json!({ "maxResults": 1, "scanLength": scan }));
            let results = self.lookup(profile, rest, fields)?;
            let best = results.first();
            let matched = best.map(|best| text_of(best.get("matched"))).filter(|matched| !matched.is_empty() && rest.starts_with(matched));
            let (Some(best), Some(matched)) = (best, matched) else {
                let step = rest.chars().next().map_or(1, char::len_utf8);
                plain(&mut segments, &rest[..step]);
                position += step;
                continue;
            };
            // `matchedSegments`: without ruby, the matched text without a reading.
            let matched_segments = if ruby {
                furigana::matched_furigana(best.get("term").unwrap_or(&Value::Null), matched)
            } else {
                vec![(matched.to_owned(), String::new())]
            };
            for (text, reading) in matched_segments {
                if reading.is_empty() {
                    plain(&mut segments, &text);
                } else {
                    segments.push((text, reading));
                }
            }
            position += matched.len();
        }
        Ok(segments.into_iter().map(|(text, reading)| json!({ "text": text, "reading": reading })).collect())
    }

    /// Builds the dictionary's archive and leases it under a token.
    fn dictionary_open(&self, message: &Value, profile: Option<&str>) -> Result<Value, String> {
        let id = require_text(message.get("id"), "id")?;
        let Some(dictionary) = self.dictionaries(profile).into_iter().find(|item| item.get("id").and_then(Value::as_str) == Some(id))
        else {
            return Ok(json!({ "error": "unknown dictionary", "notFound": true }));
        };
        let path = dictionary.get("path").and_then(Value::as_str).unwrap_or_default();
        if package::root(&dictionary).is_none() {
            return Err("Cannot export an invalid dictionary path.".to_owned());
        }
        let snapshot = backup::dictionary_snapshot(&self.store.snapshot(), &dictionary)?;
        let downloads = self.store.root().join(DOWNLOADS_DIR);
        fs::create_dir_all(&downloads).map_err(|error| error.to_string())?;
        let token = format!("dl-{}", self.next_download.fetch_add(1, Ordering::Relaxed) + 1);
        let file = downloads.join(format!("{token}.zip"));
        let size = archive::pack(&self.store.dictionary_dir(path), &snapshot, &js::now_iso(), &file).map_err(|error| {
            let _ = fs::remove_file(&file);
            error.to_string()
        })?;
        self.downloads.lock().unwrap_or_else(PoisonError::into_inner).insert(token.clone(), (file, size));
        Ok(json!({ "token": token, "size": size, "fileName": file_name(dictionary.get("title")) }))
    }

    fn dictionary_read(&self, message: &Value) -> Result<Value, String> {
        let token = message.get("token").and_then(Value::as_str).unwrap_or_default();
        let Some((path, size)) = self.downloads.lock().unwrap_or_else(PoisonError::into_inner).get(token).cloned() else {
            return Err("unknown download token".to_owned());
        };
        let bound = |name: &str| message.get(name).and_then(Value::as_u64).filter(|value| *value <= 9_007_199_254_740_991);
        let (Some(offset), Some(length)) = (bound("offset"), bound("length")) else {
            return Err("a dictionary read needs a non-negative offset and length".to_owned());
        };
        let end = size.min(offset.saturating_add(length));
        let mut bytes = Vec::new();
        if end > offset {
            let mut file = File::open(&path).map_err(|error| error.to_string())?;
            file.seek(SeekFrom::Start(offset)).map_err(|error| error.to_string())?;
            file.take(end - offset).read_to_end(&mut bytes).map_err(|error| error.to_string())?;
        }
        Ok(json!({ "data": base64::engine::general_purpose::STANDARD.encode(bytes), "eof": end >= size }))
    }
}

/// `{ profileId }`: the profile an API request answers from, `None` while the
/// records have no profiles (or no Japanese one for index 0).
struct Selected {
    profile_id: Option<String>,
}

/// `named`: an engine request made for a profile names it (upstream
/// docs/languages.md 9.3).
fn named(profile: Option<&str>, mut request: Value) -> Value {
    if let (Some(profile), Some(fields)) = (profile, request.as_object_mut()) {
        fields.insert("profileId".to_owned(), Value::from(profile));
    }
    request
}

/// `whereabouts`: where each dictionary is in the list.
struct Where {
    dictionaries: Vec<Value>,
}

impl Where {
    fn new(dictionaries: Vec<Value>) -> Self {
        Self { dictionaries }
    }

    fn find(&self, title: &str) -> Option<&Value> {
        self.dictionaries.iter().find(|item| item.get("title").and_then(Value::as_str) == Some(title))
    }

    fn index(&self, title: &str) -> usize {
        self.dictionaries.iter().position(|item| item.get("title").and_then(Value::as_str) == Some(title)).unwrap_or(0)
    }

    /// The display name, or the title where it has none (`||`).
    fn alias(&self, title: &str) -> Value {
        self.find(title).and_then(|item| item.get("displayName")).filter(|name| truthy(name)).cloned().unwrap_or_else(|| Value::from(title))
    }

    fn frequency_mode(&self, title: &str) -> Value {
        self.find(title).and_then(|item| item.get("frequencyMode")).filter(|mode| truthy(mode)).cloned().unwrap_or(Value::Null)
    }
}

/// JavaScript truthiness of a JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn text_of(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or_default()
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn require_text<'a>(value: Option<&'a Value>, name: &str) -> Result<&'a str, String> {
    value.and_then(Value::as_str).ok_or_else(|| format!("{name} must be a string"))
}

fn require_strings(value: Option<&Value>, name: &str) -> Result<Vec<String>, String> {
    value
        .and_then(Value::as_array)
        .and_then(|items| items.iter().map(|item| item.as_str().map(str::to_owned)).collect())
        .ok_or_else(|| format!("{name} must be an array of strings"))
}

/// `words`: a tag list's names (`parseTagList` keeps repeats).
fn words(value: Option<&Value>) -> Vec<Value> {
    let text = match value {
        Some(Value::String(text)) => text.clone(),
        Some(value) if truthy(value) => match value {
            Value::Number(number) => number.to_string(),
            Value::Bool(_) => "true".to_owned(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    tags::tag_list(Some(&text)).into_iter().map(Value::from).collect()
}

/// `strings`: an array's string elements.
fn strings(value: Option<&Value>) -> Vec<Value> {
    value.and_then(Value::as_array).map(|items| items.iter().filter(|item| item.is_string()).cloned().collect()).unwrap_or_default()
}

/// `tag`: Yomitan's Tag, with a tag-bank tag's fields where the reply has them.
fn tag(name: &Value, dictionary: &Value, entry: Option<&Value>) -> Value {
    let field = |key: &str, fallback: Value| entry.and_then(|entry| entry.get(key)).cloned().unwrap_or(fallback);
    let notes = field("notes", json!(""));
    json!({
        "name": name,
        "category": field("category", json!("")),
        "order": field("order", json!(0)),
        "score": field("score", json!(0)),
        "content": if truthy(&notes) { json!([notes]) } else { json!([]) },
        "dictionaries": [dictionary],
        "redundant": false,
    })
}

/// `parseGlossary`.
fn parse_glossary(glossary: Option<&Value>) -> Value {
    let text = match glossary {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => "undefined".to_owned(),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Array(items)) => Value::Array(items),
        Ok(other) => json!([other]),
        Err(_) => json!([text]),
    }
}

/// An object with the fields of `pairs` whose value is present (JSON drops `undefined`).
fn present(pairs: &[(&str, Option<&Value>)]) -> Value {
    Value::Object(pairs.iter().filter_map(|(key, value)| Some(((*key).to_owned(), (*value)?.clone()))).collect())
}

fn list(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// `termEntry`: Yomitan's `TermDictionaryEntry`, as far as the engine's result carries it.
fn term_entry(result: &Value, where_: &Where) -> Value {
    let term = result.get("term").unwrap_or(&Value::Null);
    let matched = result.get("matched");
    let deinflected = result.get("deinflected");
    let glossaries = list(term.get("glossaries"));
    let first = glossaries.first().and_then(|glossary| glossary.get("dictionary")).cloned().unwrap_or_else(|| json!(""));
    let first_title = first.as_str().unwrap_or_default();
    let mut headword_tags: Vec<Value> = Vec::new();
    for name in glossaries.iter().flat_map(|glossary| words(glossary.get("termTags"))) {
        if !headword_tags.contains(&name) {
            headword_tags.push(name);
        }
    }
    let definitions: Vec<Value> = glossaries
        .iter()
        .enumerate()
        .map(|(index, glossary)| {
            let dictionary = glossary.get("dictionary").cloned().unwrap_or(Value::Null);
            let title = dictionary_of(glossary.get("dictionary"));
            let tag_rows: Vec<Value> = match glossary.get("tags") {
                Some(Value::Array(rows)) => {
                    rows.iter().map(|row| tag(row.get("name").unwrap_or(&Value::Null), &dictionary, Some(row))).collect()
                }
                _ => words(glossary.get("definitionTags")).iter().map(|name| tag(name, &dictionary, None)).collect(),
            };
            let mut definition = present(&[("dictionary", glossary.get("dictionary")), ("score", term.get("score"))]);
            if let Some(fields) = definition.as_object_mut() {
                fields.insert("index".to_owned(), json!(index));
                fields.insert("headwordIndices".to_owned(), json!([0]));
                fields.insert("dictionaryIndex".to_owned(), json!(where_.index(&title)));
                fields.insert("dictionaryAlias".to_owned(), where_.alias(&title));
                fields.insert("id".to_owned(), json!(index));
                fields.insert("frequencyOrder".to_owned(), json!(0));
                fields.insert("sequences".to_owned(), json!([-1]));
                fields.insert("isPrimary".to_owned(), json!(true));
                fields.insert("tags".to_owned(), Value::Array(tag_rows));
                fields.insert("entries".to_owned(), parse_glossary(glossary.get("glossary")));
            }
            definition
        })
        .collect();
    let pronunciations = pronunciations(term, where_);
    let frequencies = frequencies(term, where_);
    let rules: Vec<Value> = list(result.get("trace"))
        .iter()
        .map(|step| present(&[("name", step.get("name")), ("description", step.get("description"))]))
        .collect();
    let source = present(&[("originalText", matched), ("transformedText", deinflected), ("deinflectedText", deinflected)]);
    let mut source = source;
    if let Some(fields) = source.as_object_mut() {
        fields.insert("matchType".to_owned(), json!("exact"));
        fields.insert("matchSource".to_owned(), json!("term"));
        fields.insert("isPrimary".to_owned(), json!(true));
    }
    let mut headword = present(&[("term", term.get("expression")), ("reading", term.get("reading"))]);
    if let Some(fields) = headword.as_object_mut() {
        fields.insert("index".to_owned(), json!(0));
        fields.insert("sources".to_owned(), json!([source]));
        fields.insert("tags".to_owned(), Value::Array(headword_tags.iter().map(|name| tag(name, &first, None)).collect()));
        fields.insert("wordClasses".to_owned(), Value::Array(words(term.get("rules"))));
    }
    let mut entry = present(&[("score", term.get("score"))]);
    if let Some(fields) = entry.as_object_mut() {
        fields.insert("type".to_owned(), json!("term"));
        fields.insert("isPrimary".to_owned(), json!(true));
        fields.insert("textProcessorRuleChainCandidates".to_owned(), json!([[]]));
        fields.insert("inflectionRuleChainCandidates".to_owned(), json!([{ "source": "dictionary", "inflectionRules": rules }]));
        fields.insert("frequencyOrder".to_owned(), json!(0));
        fields.insert("dictionaryIndex".to_owned(), json!(where_.index(first_title)));
        fields.insert("dictionaryAlias".to_owned(), where_.alias(first_title));
        let exact = matched.is_some() && matched == term.get("expression");
        fields.insert("sourceTermExactMatchCount".to_owned(), json!(u8::from(exact)));
        fields.insert("matchPrimaryReading".to_owned(), json!(false));
        fields.insert("maxOriginalTextLength".to_owned(), json!(utf16_len(text_of(matched))));
        fields.insert("headwords".to_owned(), json!([headword]));
        fields.insert("definitions".to_owned(), Value::Array(definitions));
        fields.insert("pronunciations".to_owned(), Value::Array(pronunciations));
        fields.insert("frequencies".to_owned(), Value::Array(frequencies));
    }
    entry
}

fn dictionary_of(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// `termEntry`'s pronunciations: pitch accents, then IPA transcriptions.
fn pronunciations(term: &Value, where_: &Where) -> Vec<Value> {
    list(term.get("pitches"))
        .iter()
        .map(|group| {
            let title = dictionary_of(group.get("dictionary"));
            let mut entries: Vec<Value> = list(group.get("pitches"))
                .iter()
                .map(|pitch| {
                    let mut entry = present(&[
                        ("positions", pitch.get("position")),
                        ("nasalPositions", pitch.get("nasal")),
                        ("devoicePositions", pitch.get("devoice")),
                    ]);
                    if let Some(fields) = entry.as_object_mut() {
                        fields.insert("type".to_owned(), json!("pitch-accent"));
                        fields.insert("tags".to_owned(), json!([]));
                    }
                    entry
                })
                .collect();
            entries.extend(
                list(group.get("transcriptions")).iter().map(|ipa| json!({ "type": "phonetic-transcription", "ipa": ipa, "tags": [] })),
            );
            let mut row = present(&[("dictionary", group.get("dictionary"))]);
            if let Some(fields) = row.as_object_mut() {
                fields.insert("headwordIndex".to_owned(), json!(0));
                fields.insert("dictionaryIndex".to_owned(), json!(where_.index(&title)));
                fields.insert("dictionaryAlias".to_owned(), where_.alias(&title));
                fields.insert("pronunciations".to_owned(), Value::Array(entries));
            }
            row
        })
        .collect()
}

/// `termEntry`'s frequencies, one row per value.
fn frequencies(term: &Value, where_: &Where) -> Vec<Value> {
    let mut frequencies = Vec::new();
    for group in list(term.get("frequencies")) {
        let title = dictionary_of(group.get("dictionary"));
        for value in list(group.get("frequencies")) {
            let display = value.get("displayValue").filter(|display| truthy(display)).cloned().unwrap_or(Value::Null);
            let mut row = present(&[("dictionary", group.get("dictionary")), ("frequency", value.get("value"))]);
            if let Some(fields) = row.as_object_mut() {
                fields.insert("index".to_owned(), json!(frequencies.len()));
                fields.insert("headwordIndex".to_owned(), json!(0));
                fields.insert("frequencyMode".to_owned(), where_.frequency_mode(&title));
                fields.insert("dictionaryIndex".to_owned(), json!(where_.index(&title)));
                fields.insert("dictionaryAlias".to_owned(), where_.alias(&title));
                fields.insert("hasReading".to_owned(), json!(false));
                fields.insert("displayValue".to_owned(), display);
                fields.insert("displayValueParsed".to_owned(), json!(false));
            }
            frequencies.push(row);
        }
    }
    frequencies
}

/// `kanjiFields`: a kanji entry's marker values, escaped for a note.
fn kanji_fields(character: &str, entry: &Value, markers: &[String], where_: &Where) -> Value {
    let dictionary = js::to_string(entry.get("dictionary"));
    let stat = |name: &str| {
        let value = list(entry.get("stats"))
            .iter()
            .find(|item| item.get("name").and_then(Value::as_str) == Some(name))
            .and_then(|item| item.get("value"));
        escape_html(&value.filter(|value| !value.is_null()).map(|value| js::to_string(Some(value))).unwrap_or_default())
    };
    let joined =
        |key: &str| words(entry.get(key)).iter().map(|word| escape_html(word.as_str().unwrap_or_default())).collect::<Vec<_>>().join(", ");
    let value = |marker: &str| -> String {
        match marker {
            "character" => escape_html(character),
            "dictionary" => escape_html(&dictionary),
            "dictionary-alias" => escape_html(&js::to_string(Some(&where_.alias(&dictionary)))),
            "onyomi" | "kunyomi" | "tags" => joined(marker),
            "glossary" => format!(
                "<ul>{}</ul>",
                strings(entry.get("definitions")).iter().fold(String::new(), |mut items, text| {
                    let _ = write!(items, "<li>{}</li>", escape_html(text.as_str().unwrap_or_default()));
                    items
                })
            ),
            "stroke-count" => stat("strokes"),
            "frequencies" => stat("freq"),
            _ => String::new(),
        }
    };
    // Each field keeps the marker as the request spelled it, as Yomitan's do.
    Value::Object(markers.iter().map(|marker| (marker.clone(), json!(value(&marker.to_lowercase())))).collect())
}

/// `kanjiEntry`.
fn kanji_entry(character: &str, entry: &Value, where_: &Where) -> Value {
    let dictionary = entry.get("dictionary").cloned().unwrap_or(Value::Null);
    let title = dictionary.as_str().unwrap_or_default();
    let stats: Vec<Value> = list(entry.get("stats"))
        .iter()
        .map(|stat| {
            let mut row = present(&[("name", stat.get("name")), ("dictionary", entry.get("dictionary")), ("value", stat.get("value"))]);
            if let Some(fields) = row.as_object_mut() {
                fields.insert("category".to_owned(), json!("misc"));
                fields.insert("content".to_owned(), json!(""));
                fields.insert("order".to_owned(), json!(0));
                fields.insert("score".to_owned(), json!(0));
            }
            row
        })
        .collect();
    let mut row = present(&[("dictionary", entry.get("dictionary"))]);
    if let Some(fields) = row.as_object_mut() {
        fields.insert("type".to_owned(), json!("kanji"));
        fields.insert("character".to_owned(), json!(character));
        fields.insert("dictionaryIndex".to_owned(), json!(where_.index(title)));
        fields.insert("dictionaryAlias".to_owned(), where_.alias(title));
        fields.insert("onyomi".to_owned(), Value::Array(words(entry.get("onyomi"))));
        fields.insert("kunyomi".to_owned(), Value::Array(words(entry.get("kunyomi"))));
        fields.insert("tags".to_owned(), Value::Array(words(entry.get("tags")).iter().map(|name| tag(name, &dictionary, None)).collect()));
        fields.insert("stats".to_owned(), if stats.is_empty() { json!({}) } else { json!({ "misc": stats }) });
        fields.insert("definitions".to_owned(), Value::Array(strings(entry.get("definitions"))));
        fields.insert("frequencies".to_owned(), json!([]));
    }
    row
}

/// `fileName`: the archive name a download is offered under.
fn file_name(title: Option<&Value>) -> String {
    let title = match title {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => "undefined".to_owned(),
    };
    let safe: String = title
        .chars()
        .map(|c| if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'..='\u{1f}') { '_' } else { c })
        .collect();
    let safe = safe.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    format!("{}.hachidori.zip", if safe.is_empty() { "dictionary" } else { safe })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_safe_and_never_empty() {
        assert_eq!(file_name(Some(&json!("Nico/Pixiv: 2"))), "Nico_Pixiv_ 2.hachidori.zip");
        assert_eq!(file_name(Some(&json!("  "))), "dictionary.hachidori.zip");
        assert_eq!(file_name(None), "undefined.hachidori.zip");
    }

    #[test]
    fn definition_tags_keep_a_no_break_space_inside_a_name() {
        // Upstream api-host.test.mjs (#426): Jitendex writes U+00A0 inside a tag
        // name, which Yomitan's _splitField keeps.
        let names = words(Some(&json!("rarely\u{a0}used\u{a0}form ateji\u{a0}form")));
        assert_eq!(names, [json!("rarely\u{a0}used\u{a0}form"), json!("ateji\u{a0}form")]);
    }

    #[test]
    fn glossaries_parse_as_the_extension_parses_them() {
        assert_eq!(parse_glossary(Some(&json!("to eat"))), json!(["to eat"]));
        assert_eq!(parse_glossary(Some(&json!(r#"[{"type":"structured-content"}]"#))), json!([{ "type": "structured-content" }]));
        assert_eq!(parse_glossary(Some(&json!("12"))), json!([12]));
    }
}
