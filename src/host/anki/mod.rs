// SPDX-License-Identifier: GPL-3.0-or-later
//! Anki over AnkiConnect, as the extension's service worker owns it for the browsers
//! linked to it: `hachidori-anki` requests, Settings' discovery and setup check, and
//! the duplicate index behind word status.
//!
//! - [`gateway`]: AnkiConnect requests and the text of its errors.
//! - [`templates`]: the `{marker}` field Templates and the note type presets.
//! - [`setup`]: read-only setup detection.
//! - [`index`]: the word index's AnkiConnect queries.
//! - [`cache`]: the stored duplicate index.
//! - [`duplicates`]: notes, duplicates and Anki's add checks.

pub(crate) mod api;
mod audio;
mod cache;
mod compact;
mod css;
mod dom;
mod duplicates;
mod gateway;
mod glossary;
mod index;
mod language;
mod media;
mod preflight;
mod profile;
mod setup;
mod submit;
mod templates;
mod values;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use self::cache::Index;
use self::gateway::{Discovery, Gateway, Result};
use self::templates::Resolved;
use super::engine::Engine;
use super::engine::mutations::{coded, split_code};
use super::js;
use super::local::Publisher;
use super::settings::{hex, options, profile_worker, profiles, worker};

/// `OPTIONS_KEY`.
const OPTIONS_KEY: &str = "options";
/// `CONFIG_CHANGED`.
const CONFIG_CHANGED: &str = "Anki configuration changed. Refresh this result before adding a note.";
const TEMPLATE_GONE: &str = "The selected Anki Template is no longer available.";
/// `ANKI_DECK_UNSET`.
const DECK_UNSET: &str = "Choose an available deck.";
/// `LINKED_ANKI_REQUESTS`.
pub(crate) const REQUESTS: [&str; 7] =
    ["hd_anki_status", "hd_anki_view", "hd_anki_preflight", "hd_anki_submit", "hd_anki_browse", "hd_anki_maturity", "hd_anki_word_status"];
/// `guiBrowse`'s deadline.
const BROWSE_TIMEOUT_MS: u64 = 30_000;

/// One AnkiConnect action with its params and, optionally, its own deadline.
pub(crate) type Invoke<'a> = dyn Fn(&str, Value, Option<u64>) -> Result<Value> + 'a;

/// `positiveId`: a safe integer above zero.
pub(crate) fn positive_id(value: &Value) -> Option<i64> {
    const MAX_SAFE: i64 = (1 << 53) - 1;
    let id = if let Some(id) = value.as_i64() {
        id
    } else {
        let number = value.as_f64().filter(|number| number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0)?;
        #[allow(clippy::cast_possible_truncation, reason = "a safe integer")]
        let id = number as i64;
        id
    };
    (id > 0 && id <= MAX_SAFE).then_some(id)
}

/// A Template's configuration, checked against Anki.
#[derive(Clone)]
struct Configured {
    config: Map<String, Value>,
    config_key: String,
    discovery: Option<Discovery>,
    resolved: Option<Resolved>,
    errors: Vec<String>,
}

/// The relay's Anki service.
pub(crate) struct Anki {
    publisher: Arc<Publisher>,
    /// The dictionaries, for the fields a note's Templates make.
    engine: Arc<Engine>,
    gateway: Gateway,
    index: Index,
    /// `configuration`'s cache: per Template, its JSON and result.
    configured: Mutex<HashMap<String, (String, Configured)>>,
    /// `linkedAnkiConfigPrefix`: a host start's prefix of the keys linked browsers see.
    linked_prefix: String,
    /// `mutations`: one note write at a time.
    mutations: Mutex<()>,
    this: Weak<Anki>,
}

/// `selectedFields`.
fn selected_fields(value: Option<&Value>, fields: &[&str]) -> std::result::Result<Map<String, Value>, String> {
    let value = value.and_then(Value::as_object).ok_or("malformed linked Anki request")?;
    Ok(fields.iter().filter_map(|field| value.get(*field).map(|found| ((*field).to_owned(), found.clone()))).collect())
}

/// `selectedTemplateId`.
fn selected_template_id(value: &Map<String, Value>) -> std::result::Result<Option<Value>, String> {
    let Some(id) = value.get("templateId") else { return Ok(None) };
    match id.as_str() {
        Some(text) if !text.is_empty() && js::length(text) <= 256 && !text.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}') => {
            Ok(Some(id.clone()))
        }
        _ => Err("malformed linked Anki Template".to_owned()),
    }
}

/// `selectedProfileId`: the profile a linked request is for, when it names one.
pub(crate) fn selected_profile_id(value: &Value) -> std::result::Result<Option<Value>, String> {
    let Some(id) = value.get("profileId") else { return Ok(None) };
    match id.as_str() {
        Some(text) if !text.is_empty() && js::length(text) <= 256 && !text.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}') => {
            Ok(Some(id.clone()))
        }
        _ => Err("malformed linked profile".to_owned()),
    }
}

fn with_template_id(mut request: Map<String, Value>) -> std::result::Result<Map<String, Value>, String> {
    if let Some(id) = selected_template_id(&request)? {
        request.insert("templateId".to_owned(), id);
    }
    Ok(request)
}

/// The `requestId` the sharing filters keep: a string or a finite number.
fn request_id(message: &Value) -> Value {
    match message.get("requestId") {
        Some(id @ (Value::String(_) | Value::Number(_))) => id.clone(),
        _ => Value::Null,
    }
}

/// `allowLinkedAnkiRequest`: the request shape each operation needs, nothing else.
fn allow(message: &Value) -> std::result::Result<Value, String> {
    let kind = message.get("type").and_then(Value::as_str).unwrap_or_default();
    if message.get("target").and_then(Value::as_str) != Some("hachidori-anki") || !REQUESTS.contains(&kind) {
        return Err("unsupported linked Anki request".to_owned());
    }
    let mut base = json!({ "target": "hachidori-anki", "type": kind, "requestId": request_id(message) });
    if let Some(id) = selected_profile_id(message)? {
        base["profileId"] = id;
    }
    let request = message.get("request");
    let term = |request: &mut Map<String, Value>| -> std::result::Result<(), String> {
        let term = selected_fields(request.get("term"), &["expression", "reading"])?;
        request.insert("term".to_owned(), Value::Object(term));
        Ok(())
    };
    match kind {
        "hd_anki_status" => {
            if let Some(id) = message.as_object().map(selected_template_id).transpose()?.flatten() {
                base["templateId"] = id;
            }
            return Ok(base);
        }
        "hd_anki_view" => {
            let mut request = with_template_id(selected_fields(request, &["term", "templateId"])?)?;
            term(&mut request)?;
            base["request"] = Value::Object(request);
        }
        "hd_anki_maturity" => {
            let mut request = selected_fields(request, &["term"])?;
            term(&mut request)?;
            base["request"] = Value::Object(request);
        }
        "hd_anki_word_status" => base["request"] = Value::Object(selected_fields(request, &["headwords"])?),
        "hd_anki_browse" => {
            base["request"] =
                Value::Object(with_template_id(selected_fields(request, &["noteIds", "expression", "configKey", "templateId"])?)?);
        }
        _ => {
            const MINING_REQUEST_FIELDS: [&str; 22] = [
                "term",
                "trace",
                "generation",
                "sentence",
                "matchOffset",
                "matched",
                "popupSelectionText",
                "searchQuery",
                "documentTitle",
                "pageUrl",
                "audioSelection",
                "dictionaryAliases",
                "dictionaryIds",
                // A profile's notes follow each dictionary's direction and target language.
                "dictionaryDirections",
                "dictionaryTargetLanguages",
                "frequencyDictionaries",
                "configKey",
                "screenshot",
                "captureUnavailable",
                "clientSpeech",
                "templateId",
                "allowDuplicate",
            ];
            base["request"] = Value::Object(with_template_id(selected_fields(request, &MINING_REQUEST_FIELDS)?)?);
            if kind == "hd_anki_submit" {
                base["clientMedia"] = message.get("clientMedia").cloned().unwrap_or(Value::Null);
            }
        }
    }
    Ok(base)
}

/// `workerReply`: the result's own `ok` and `error`, or success.
fn reply(message: &Value, mut result: Map<String, Value>) -> Value {
    let ok = result.remove("ok").unwrap_or(Value::Bool(true));
    let error = result.remove("error").unwrap_or(Value::Null);
    let mut reply = Map::new();
    reply.insert("type".to_owned(), json!(format!("{}_result", message["type"].as_str().unwrap_or_default())));
    reply.insert("requestId".to_owned(), message.get("requestId").cloned().unwrap_or(Value::Null));
    reply.insert("ok".to_owned(), ok);
    reply.insert("error".to_owned(), error);
    reply.extend(result);
    Value::Object(reply)
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

impl Anki {
    pub(crate) fn new(publisher: Arc<Publisher>, engine: Arc<Engine>) -> Arc<Self> {
        let anki = Arc::new_cyclic(|this| Self {
            publisher,
            engine,
            gateway: Gateway::default(),
            index: Index::default(),
            configured: Mutex::default(),
            linked_prefix: format!("linked:{}:", worker::random_uuid().unwrap_or_else(|_| format!("{:x}", js::now().to_bits()))),
            mutations: Mutex::default(),
            this: this.clone(),
        });
        anki.index.install(anki.index_state().as_ref());
        anki
    }

    fn arc(&self) -> Option<Arc<Self>> {
        self.this.upgrade()
    }

    /// Stores `changes` and tells every linked client; an options change also
    /// reconciles the duplicate index.
    pub(crate) fn publish(&self, changes: Map<String, Value>) -> std::io::Result<Map<String, Value>> {
        let stored = self.publisher.publish(changes)?;
        if stored.contains_key(OPTIONS_KEY) {
            self.reconcile();
        }
        Ok(stored)
    }

    /// One stored value.
    pub(crate) fn read(&self, key: &str) -> Option<Value> {
        self.publisher.read(key)
    }

    /// Every stored key with `prefix`.
    pub(crate) fn keys_with(&self, prefix: &str) -> Vec<String> {
        self.publisher.keys_with(prefix)
    }

    /// `reconcileAnkiIndex`.
    pub(crate) fn reconcile(&self) {
        if let Some(anki) = self.arc() {
            Index::reconcile(&anki);
        }
    }

    /// `readAnkiOptions`: the options, without the browser speech this host
    /// cannot put on a note.
    fn options(&self) -> Map<String, Value> {
        options_from(self.publisher.read(OPTIONS_KEY).as_ref())
    }

    /// `readAnkiOptionsAndProfile`: the options of the profile an Anki request is
    /// for (`ankiOptionsOf`; today's for one that names none, and a missing
    /// profile refused with its `errorCode`), and that profile's `{language,
    /// packSettings}` once the records have profiles (`ankiProfileOf`).
    fn options_and_profile(&self, profile_id: Option<&str>) -> std::result::Result<(Map<String, Value>, Option<Value>), String> {
        let stored = self.publisher.read(OPTIONS_KEY);
        let options = match profile_id {
            None => options_from(stored.as_ref()),
            Some(id) => {
                profile_worker::request_profile_id(&json!({ "profileId": id }), stored.as_ref()).map_err(|refused| match refused.code {
                    Some(code) => coded(&refused.error, code),
                    None => refused.error,
                })?;
                without_speech(profiles::project_options(stored.as_ref(), id, true))
            }
        };
        let profile = profiles::has_profiles(stored.as_ref())
            .then(|| profiles::profile_descriptor(stored.as_ref(), profile_id.unwrap_or(super::profile::JAPANESE)))
            .flatten()
            .map(|profile| json!({ "language": profile["language"], "packSettings": profile["packSettings"] }));
        Ok((options, profile))
    }

    fn index_state(&self) -> Option<Value> {
        self.publisher.read(cache::KEY)
    }

    /// `updateState`: one change of the stored index, given the options with it;
    /// returns what is stored afterwards.
    fn update_index(&self, change: impl FnOnce(Option<&Value>, Option<&Value>) -> Option<Value>) -> Option<Value> {
        let mut saved = None;
        let result = self.publisher.publish_with(|storage| {
            let state = storage.get(cache::KEY);
            let next = change(storage.get(OPTIONS_KEY), state);
            let write = next.as_ref().filter(|next| !state.is_some_and(|state| js::same_json(state, next)));
            saved = next.clone().or_else(|| state.cloned());
            write.map(|next| Map::from_iter([(cache::KEY.to_owned(), next.clone())])).unwrap_or_default()
        });
        if let Err(error) = result {
            tracing::warn!(%error, "could not save the Anki duplicate index");
            return self.index_state();
        }
        saved
    }

    /// `invokeFor`: AnkiConnect at `url` with `api_key`.
    fn invoker<'a>(&'a self, url: Option<&'a Value>, api_key: &'a str) -> impl Fn(&str, Value, Option<u64>) -> Result<Value> + 'a {
        move |action, params, timeout| self.gateway.invoke(action, &params, api_key, timeout.unwrap_or(gateway::TIMEOUT_MS), url)
    }

    fn config_invoker<'a>(&'a self, config: &'a Map<String, Value>) -> impl Fn(&str, Value, Option<u64>) -> Result<Value> + 'a {
        self.invoker(config.get("url"), config.get("apiKey").and_then(Value::as_str).unwrap_or_default())
    }

    /// `readConfig`: the configuration of the Template `key` names (a Template
    /// id, or a profile's Template key), in its profile's options and language.
    fn read_config(&self, key: Option<&Value>) -> std::result::Result<Option<Map<String, Value>>, String> {
        let (profile_id, template_id) = profile::template_target(key);
        let (options, profile) = self.options_and_profile(profile_id.as_deref())?;
        Ok(Self::config_from(&options, profile.as_ref(), template_id.as_ref()))
    }

    /// `readConfig`'s configuration from the profile's options.
    fn config_from(options: &Map<String, Value>, profile: Option<&Value>, template_id: Option<&Value>) -> Option<Map<String, Value>> {
        let template = options::anki_template_config(options.get("anki"), template_id)?;
        let mut config = profile::in_language(template, profile);
        let sources: Vec<Value> = options
            .get("audioSources")
            .and_then(Value::as_array)
            .map(|sources| sources.iter().filter(|source| source.get("enabled") == Some(&Value::Bool(true))).cloned().collect())
            .unwrap_or_default();
        config.insert("audioSources".to_owned(), Value::Array(sources));
        config.insert("compactGlossary".to_owned(), json!(options.get("smallerAnkiCards") == Some(&Value::Bool(true))));
        // Part of the checked configuration, so toggling Compact popup and Anki
        // between a preflight and Add refuses the stale Add.
        config.insert("compactGlossaries".to_owned(), json!(options.get("glossaryLayoutMode") == Some(&json!("compact-popup-anki"))));
        let experimental =
            |flag: &str| options.get("experimental").and_then(|experimental| experimental.get(flag)) == Some(&Value::Bool(true));
        config.insert("netflixMining".to_owned(), json!(experimental("netflixMining")));
        config.insert("youtubeMining".to_owned(), json!(options.get("youtubeMining") == Some(&Value::Bool(true))));
        config.insert("crunchyrollMining".to_owned(), json!(experimental("crunchyrollMining")));
        // And the profile's language, which decides how the note is written.
        if let Some(profile) = profile {
            config.insert("profile".to_owned(), profile.clone());
        }
        Some(config)
    }

    /// `ankiDigest(JSON.stringify({templateId, config}))`.
    fn config_key(template_id: Option<&Value>, config: &Map<String, Value>) -> String {
        let identity = json!({ "templateId": template_id.cloned().unwrap_or(Value::Null), "config": config });
        hex(&Sha256::digest(identity.to_string().as_bytes()))
    }

    /// `identity`.
    fn identity(&self, template_id: Option<&Value>) -> std::result::Result<(Map<String, Value>, String), String> {
        let config = self.read_config(template_id)?.ok_or(TEMPLATE_GONE)?;
        let key = Self::config_key(template_id, &config);
        Ok((config, key))
    }

    /// `configuration`: the Template checked against Anki. The result is kept while
    /// the configuration is unchanged; Anki validates the destination again when
    /// adding, and a configuration with errors, or a failure, retires it for a retry.
    fn configuration(&self, template_id: Option<&Value>) -> std::result::Result<Configured, String> {
        let (config, config_key) = self.identity(template_id)?;
        let json = Value::Object(config.clone()).to_string();
        let cache_key = Self::cache_key(template_id);
        if let Some((key, configured)) = self.configured.lock().unwrap_or_else(PoisonError::into_inner).get(&cache_key)
            && *key == json
        {
            return Ok(configured.clone());
        }
        let configured = if config.get("model").and_then(Value::as_str).unwrap_or_default().is_empty() {
            Configured {
                config,
                config_key,
                discovery: None,
                resolved: None,
                errors: vec!["Choose an Anki note type in Settings.".to_owned()],
            }
        } else {
            let discovery = self.gateway.discover(&config);
            let resolved = templates::resolve(&config, &discovery.fields);
            let errors = gateway::availability(&config, Some(&discovery), Some(&resolved));
            Configured { config, config_key, discovery: Some(discovery), resolved: Some(resolved), errors }
        };
        if configured.errors.is_empty() {
            self.configured.lock().unwrap_or_else(PoisonError::into_inner).insert(cache_key, (json, configured.clone()));
        } else {
            self.retire(template_id);
        }
        Ok(configured)
    }

    /// `cached`'s key: the Template id, or "".
    fn cache_key(template_id: Option<&Value>) -> String {
        template_id.and_then(Value::as_str).unwrap_or_default().to_owned()
    }

    /// `cached.delete`: the next request discovers the Template again.
    fn retire(&self, template_id: Option<&Value>) {
        self.configured.lock().unwrap_or_else(PoisonError::into_inner).remove(&Self::cache_key(template_id));
    }

    /// `hostLinkedAnkiRequest`: the request with the host's own configuration key.
    fn host_request(&self, request: &Value) -> std::result::Result<Map<String, Value>, String> {
        let mut request = request.as_object().cloned().unwrap_or_default();
        let key =
            request.get("configKey").and_then(Value::as_str).and_then(|key| key.strip_prefix(&self.linked_prefix)).ok_or(CONFIG_CHANGED)?;
        let key = key.to_owned();
        request.insert("configKey".to_owned(), json!(key));
        Ok(request)
    }

    fn linked_key(&self, key: &str) -> String {
        format!("{}{key}", self.linked_prefix)
    }

    /// `status`, as `singleProfileStatus` answers it: only a status that names
    /// a profile says when nothing but a deck is missing (`emptyDeck`), which
    /// opens Settings → Anki for a new profile's Templates.
    fn status(&self, message: &Value) -> std::result::Result<Map<String, Value>, String> {
        let current = self.configuration(message.get("templateId"))?;
        let mut status = object(json!({
            "available": current.errors.is_empty(),
            "configKey": self.linked_key(&current.config_key),
            "error": current.errors.join("\n"),
        }));
        if message.get("profileId").is_some() && current.errors == [DECK_UNSET] {
            status.insert("emptyDeck".to_owned(), Value::Bool(true));
        }
        Ok(status)
    }

    /// `view`: what the index already knows of the term, without asking Anki.
    fn view(&self, request: &Value) -> std::result::Result<Map<String, Value>, String> {
        let (config, config_key) = self.identity(request.get("templateId"))?;
        let config_key = self.linked_key(&config_key);
        let expression = request.get("term").and_then(|term| term.get("expression")).or_else(|| request.get("expression"));
        let unknown = || object(json!({ "state": "unknown", "canAdd": false, "noteIds": [], "configKey": config_key, "cached": false }));
        if config.get("duplicateBehavior").and_then(Value::as_str) != Some("prevent") {
            return Ok(unknown());
        }
        let duplicate = self.index.peek(&config, expression.filter(|value| !value.is_null()));
        if duplicate.note_ids.is_empty() {
            return Ok(unknown());
        }
        Ok(object(json!({
            "state": "duplicate",
            "canAdd": false,
            "noteIds": duplicate.note_ids,
            "mature": duplicate.mature,
            "configKey": config_key,
            "cached": true,
        })))
    }

    /// `browse`: Anki's browser on the term's notes, repaired from Anki first.
    fn browse(&self, request: &Map<String, Value>) -> std::result::Result<Map<String, Value>, String> {
        let template_id = request.get("templateId");
        let config = self.read_config(template_id)?.ok_or(TEMPLATE_GONE)?;
        if let Some(key) = request.get("configKey").and_then(Value::as_str)
            && key != Self::config_key(template_id, &config)
        {
            return Err(CONFIG_CHANGED.to_owned());
        }
        let invoke = self.config_invoker(&config);
        let supplied = request.get("noteIds").and_then(Value::as_array).filter(|ids| !ids.is_empty());
        let mut note_ids: Vec<Value> = supplied.cloned().unwrap_or_default();
        let mut repaired = false;
        let expression = request.get("expression").filter(|expression| expression.as_str().is_some_and(|text| !text.is_empty()));
        // Any term the index can answer for is repaired, so View finds the notes of
        // a duplicate the add check named without ids (Hachidori #683).
        if let Some(expression) = expression
            && index::source(&config).is_some()
        {
            let found = self.index.find(self, &config, Some(expression), &invoke, true).map_err(|error| error.message)?;
            if found.note_ids.is_empty() {
                return Ok(object(json!({ "opened": false, "noteIds": [], "repaired": true })));
            }
            note_ids = found.note_ids.iter().map(|id| json!(id)).collect();
            repaired = true;
        }
        let query = if note_ids.is_empty() {
            duplicates::browse_query(&js::to_string(request.get("expression").filter(|value| !value.is_null()).or(Some(&json!("")))))
        } else {
            let ids: Option<Vec<i64>> = note_ids.iter().map(positive_id).collect();
            duplicates::note_ids_query(&ids.unwrap_or_default()).map_err(|error| error.message)?
        };
        invoke("guiBrowse", json!({ "query": query }), Some(BROWSE_TIMEOUT_MS)).map_err(|error| error.message)?;
        Ok(object(json!({ "opened": true, "noteIds": note_ids, "repaired": repaired })))
    }

    /// `maturity`: whether the term's cached row is mature, when definitions blur for it.
    fn maturity(&self, request: &Value) -> std::result::Result<Map<String, Value>, String> {
        let (options, profile) = self.options_and_profile(profile::request_profile(request).as_deref())?;
        let blur = options.get("definitionBlurAnkiMature") == Some(&Value::Bool(true));
        let anki = profile::in_language(options.get("anki").and_then(Value::as_object).cloned().unwrap_or_default(), profile.as_ref());
        let expression = request.get("term").and_then(|term| term.get("expression"));
        Ok(object(json!({ "mature": blur && self.index.has(&anki, expression) })))
    }

    /// `wordStatus`: the page's headwords from the first Template's cached rows.
    fn word_status(&self, request: &Value) -> std::result::Result<Map<String, Value>, String> {
        let headwords = request
            .get("headwords")
            .and_then(Value::as_array)
            .and_then(|headwords| headwords.iter().map(Value::as_str).collect::<Option<Vec<&str>>>())
            .ok_or("Word status requires an array of headwords.")?;
        let (options, profile) = self.options_and_profile(profile::request_profile(request).as_deref())?;
        let anki = profile::in_language(options.get("anki").and_then(Value::as_object).cloned().unwrap_or_default(), profile.as_ref());
        Ok(object(self.index.statuses(&anki, &headwords)))
    }

    /// `answerAnkiRequest` for a linked client: one `hachidori-anki` request.
    pub(crate) fn handle(&self, message: &Value) -> Value {
        let allowed = match allow(message).and_then(profile::with_anki_profile) {
            Ok(allowed) => allowed,
            Err(error) => return super::local::failure(message, &error),
        };
        let request = allowed.get("request").cloned().unwrap_or(Value::Null);
        let result = match allowed["type"].as_str().unwrap_or_default() {
            "hd_anki_status" => self.status(&allowed),
            "hd_anki_view" => self.view(&request),
            "hd_anki_browse" => self.host_request(&request).and_then(|request| self.browse(&request)),
            "hd_anki_maturity" => self.maturity(&request),
            "hd_anki_word_status" => self.word_status(&request),
            "hd_anki_preflight" => self.host_request(&request).and_then(|request| self.preflight(&Value::Object(request))),
            "hd_anki_submit" => self
                .host_request(&request)
                .and_then(|request| self.submit(&Value::Object(request), allowed.get("clientMedia").unwrap_or(&Value::Null))),
            _ => self.host_request(&request).and(Err(super::local::UNSUPPORTED.to_owned())),
        };
        match result {
            Ok(result) => reply(&allowed, result),
            Err(error) => coded_failure(&allowed, &error),
        }
    }

    /// `hd_anki_discover` from a linked Settings page: the host's Anki, for the note
    /// type the page is choosing.
    pub(crate) fn discover(&self, message: &Value) -> Value {
        let model = message.get("model").and_then(Value::as_str).filter(|model| js::length(model) <= 4096);
        let Some(model) = model else { return super::local::failure(message, "unsupported linked Anki discovery request") };
        let allowed =
            json!({ "target": "hoshidicts-worker", "type": "hd_anki_discover", "requestId": request_id(message), "model": model });
        let mut config = self.options().get("anki").and_then(Value::as_object).cloned().unwrap_or_default();
        config.insert("model".to_owned(), json!(model));
        reply(&allowed, self.gateway.discover(&config).to_json())
    }

    /// `hd_anki_setup` from a linked Settings page: the selected Template of the
    /// profile it names checked.
    pub(crate) fn setup(&self, message: &Value) -> Value {
        let selected =
            message.as_object().map(selected_template_id).transpose().and_then(|id| Ok((id.flatten(), selected_profile_id(message)?)));
        let (template_id, profile_id) = match selected {
            Ok(selected) => selected,
            Err(error) => return super::local::failure(message, &error),
        };
        let mut allowed = json!({ "target": "hoshidicts-worker", "type": "hd_anki_setup", "requestId": request_id(message) });
        if let Some(id) = &template_id {
            allowed["templateId"] = id.clone();
        }
        let options = match self.options_and_profile(profile_id.as_ref().and_then(Value::as_str)) {
            Ok((options, _)) => options,
            Err(error) => return coded_failure(message, &error),
        };
        let Some(config) = options::anki_template_config(options.get("anki"), template_id.as_ref()) else {
            return super::local::failure(message, TEMPLATE_GONE);
        };
        let invoke = self.config_invoker(&config);
        reply(&allowed, object(setup::check(&invoke, &config)))
    }
}

/// `ankiIndexConfigurationChange`, as `writeLocalState` adds it: an options change
/// that moves the duplicate index to another source also resets the stored index.
pub(crate) fn derive_index(storage: &BTreeMap<String, Value>, changes: &mut Map<String, Value>) {
    let Some(next) = changes.get(OPTIONS_KEY) else { return };
    let after = Some(next).filter(|next| !next.is_null());
    if let Some(index) = cache::configuration_change(storage.get(OPTIONS_KEY), after, storage.get(cache::KEY)) {
        changes.insert(cache::KEY.to_owned(), index);
    }
}

/// The stored duplicate index, as `derive_index` and [`word_status`] read it.
pub(crate) fn stored_index(storage: &BTreeMap<String, Value>) -> Option<Value> {
    storage.get(cache::KEY).cloned()
}

/// `wordStatusChanged`: the frame that tells linked browsers to re-read word status
/// after the stored index changed from `previous` in `changes`, if it did.
pub(crate) fn word_status(previous: Option<&Value>, changes: &Map<String, Value>) -> Option<Value> {
    let next = changes.get(cache::KEY)?;
    let revision = match cache::announced(previous, Some(next).filter(|next| !next.is_null()))? {
        cache::Announced::Source => Value::Null,
        cache::Announced::Rows(revision) => json!(revision),
    };
    Some(json!({ "kind": "word-status", "revision": revision }))
}

/// `normaliseOptions`, with `capabilityAnkiOptions`' browser speech filter.
fn options_from(stored: Option<&Value>) -> Map<String, Value> {
    without_speech(options::normalise(stored))
}

/// `capabilityAnkiOptions`: no browser speech, which this host cannot put on a note.
fn without_speech(mut options: Map<String, Value>) -> Map<String, Value> {
    if let Some(Value::Array(sources)) = options.get_mut("audioSources") {
        sources.retain(|source| !source.get("type").and_then(Value::as_str).is_some_and(|kind| kind.starts_with("text-to-speech")));
    }
    options
}

/// `failureReply`, with the `errorCode` an error was given ([`coded`]).
fn coded_failure(message: &Value, error: &str) -> Value {
    let (error, code) = split_code(error);
    super::local::failure_with_code(message, error, code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_ids_are_safe_integers() {
        assert_eq!(positive_id(&json!(3)), Some(3));
        assert_eq!(positive_id(&json!(3.0)), Some(3));
        assert_eq!(positive_id(&json!(0)), None);
        assert_eq!(positive_id(&json!(1.5)), None);
        assert_eq!(positive_id(&json!(9_007_199_254_740_992_i64)), None);
        assert_eq!(positive_id(&json!("3")), None);
    }

    #[test]
    fn linked_requests_are_rebuilt() {
        let allowed = allow(&json!({ "target": "hachidori-anki", "type": "hd_anki_view", "requestId": 4,
            "request": { "term": { "expression": "猫", "glossary": [] }, "templateId": "t", "url": "x" } }))
        .unwrap();
        assert_eq!(
            allowed,
            json!({ "target": "hachidori-anki", "type": "hd_anki_view", "requestId": 4,
            "request": { "term": { "expression": "猫" }, "templateId": "t" } })
        );
        assert_eq!(
            allow(&json!({ "target": "hachidori-anki", "type": "hd_anki_view", "request": {} })).unwrap_err(),
            "malformed linked Anki request"
        );
        assert_eq!(
            allow(&json!({ "target": "hachidori-anki", "type": "hd_anki_status", "templateId": "" })).unwrap_err(),
            "malformed linked Anki Template"
        );
        assert_eq!(
            allow(&json!({ "target": "hachidori-anki", "type": "hd_anki_screenshot" })).unwrap_err(),
            "unsupported linked Anki request"
        );
    }
}
