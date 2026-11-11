// SPDX-License-Identifier: GPL-3.0-or-later
//! The Anki word index's queries (`anki-index.js`): which notes hold a word in a
//! field mapped to `{expression}` alone, which of them are mature, and the whole
//! index for one Template's source.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};

use icu_normalizer::ComposingNormalizerBorrowed;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::gateway::{self, Failure, Result};
use super::{Invoke, positive_id, setup, templates};
use crate::host::js;
use crate::host::settings::{hex, options};

/// Anki parses these field names as search operators.
const SEARCH_OPERATORS: [&str; 23] = [
    "deck",
    "note",
    "tag",
    "card",
    "flag",
    "resched",
    "prop",
    "added",
    "edited",
    "introduced",
    "rated",
    "is",
    "did",
    "mid",
    "nid",
    "cid",
    "re",
    "nc",
    "sc",
    "w",
    "dupe",
    "has-cd",
    "preset",
];
/// `fetchAnkiIndex`' deadline for each of its requests.
const INDEX_TIMEOUT_MS: u64 = 25_000;

fn fold_ascii(value: &str) -> String {
    value.to_ascii_lowercase()
}

pub(crate) fn nfc(value: &str) -> String {
    ComposingNormalizerBorrowed::new_nfc().normalize(value).into_owned()
}

/// `nameKey`: NFC, lower-cased.
pub(crate) fn name_key(value: &str) -> String {
    nfc(value).to_lowercase()
}

/// `escapeQuery`.
pub(crate) fn escape_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '"' | '*' | '_' | ':') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn search_token(operator: &str, value: &str) -> String {
    format!("\"{}:{}\"", escape_query(operator), escape_query(value))
}

/// The note fields whose Templates hold just `{expression}`, as named in the
/// Templates, one per name key.
fn direct_expression_names(config: &Map<String, Value>, names: Option<&[String]>) -> Vec<String> {
    let default_names: Vec<String>;
    let names = if let Some(names) = names {
        names
    } else {
        default_names = match config.get("fieldTemplates").and_then(Value::as_object) {
            Some(saved) => saved.keys().cloned().collect(),
            None => config
                .get("fields")
                .and_then(Value::as_object)
                .map(|fields| fields.values().filter_map(Value::as_str).filter(|name| !name.is_empty()).map(str::to_owned).collect())
                .unwrap_or_default(),
        };
        &default_names
    };
    let resolved = templates::resolve(config, names);
    let mut fields: Vec<String> = Vec::new();
    for (field, template) in &resolved.templates {
        let value = template.get("value").and_then(Value::as_str).unwrap_or_default();
        if value.eq_ignore_ascii_case("{expression}")
            && !SEARCH_OPERATORS.contains(&field.to_lowercase().as_str())
            && !fields.iter().any(|known| name_key(known) == name_key(field))
        {
            fields.push(field.clone());
        }
    }
    fields
}

/// `directExpressionFields`: name keys, sorted.
fn name_keys(names: &[String]) -> Vec<String> {
    let mut fields: Vec<String> = names.iter().map(|name| name_key(name)).collect();
    fields.sort_by(|left, right| js::compare(left, right));
    fields
}

/// `ankiWordKey`: how Anki's ordinary field search sees `expression`.
pub(crate) fn word_key(expression: Option<&Value>) -> Option<String> {
    let expression = expression?.as_str().filter(|expression| !expression.is_empty())?;
    Some(fold_ascii(&nfc(&templates::encode_cloze_braces(&templates::escape_html(expression)))))
}

/// `storedWordKey`.
fn stored_word_key(value: Option<&str>) -> Option<String> {
    value.filter(|value| !value.is_empty()).map(fold_ascii)
}

/// `ankiIndexSource`: what the index of one Template is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Source {
    pub(crate) key: String,
    pub(crate) url: String,
    pub(crate) api_key: String,
    pub(crate) scope: String,
    pub(crate) model: String,
    pub(crate) fields: Vec<String>,
    /// `fields` as the Templates name them, for field-limited note reads.
    pub(crate) field_names: Vec<String>,
    pub(crate) deck: Option<String>,
    /// The note-type families recognised beside the configured note type, for a
    /// language without them (`[]`); `None` recognises every family.
    pub(crate) families: Option<Vec<Value>>,
}

/// `ankiIndexSource(config)`, or `None` for a Template the index cannot key.
pub(crate) fn source(config: &Map<String, Value>) -> Option<Source> {
    let text = |key: &str| config.get(key).and_then(Value::as_str).unwrap_or_default();
    if text("model").is_empty() {
        return None;
    }
    let default_url = options::defaults()["anki"]["url"].clone();
    let url = options::normalise_anki_connect_url(Some(config.get("url").unwrap_or(&default_url)))?;
    let field_names = direct_expression_names(config, None);
    let fields = name_keys(&field_names);
    if fields.is_empty() {
        return None;
    }
    let scope = text("duplicateScope").to_owned();
    let deck = (scope == "deck").then(|| text("deck").to_owned());
    // JSON.stringify(source), in its key order.
    let mut serialized = format!(
        "{{\"url\":{},\"apiKey\":{},\"scope\":{},\"model\":{},\"fields\":{}",
        Value::from(url.as_str()),
        config.get("apiKey").cloned().unwrap_or(Value::Null),
        Value::from(scope.as_str()),
        Value::from(text("model")),
        json!(fields)
    );
    if let Some(deck) = &deck {
        serialized.push_str(",\"deck\":");
        serialized.push_str(&Value::from(deck.as_str()).to_string());
    }
    // A language without note-type families recognises none beside the
    // configured note type (docs/languages.md 12).
    let families = config.get("noteTypeFamilies").and_then(Value::as_array).filter(|_| scope != "model").cloned();
    if let Some(families) = &families {
        serialized.push_str(",\"families\":");
        serialized.push_str(&Value::Array(families.clone()).to_string());
    }
    serialized.push('}');
    Some(Source {
        key: hex(&Sha256::digest(serialized.as_bytes())),
        url,
        api_key: text("apiKey").to_owned(),
        scope,
        model: text("model").to_owned(),
        fields,
        field_names,
        deck,
        families,
    })
}

/// A note type the index searches, and its direct expression fields' name keys.
struct Model {
    name: String,
    fields: Vec<String>,
    /// `fields` as named, for field-limited note reads.
    names: Vec<String>,
}

/// `recognizedModels`: the configured note type and, beyond model scope, every
/// recognised Senren, Lapis or Kiku note type.
fn recognized_models(invoke: &Invoke<'_>, source: &Source) -> Result<Vec<Model>> {
    if source.scope == "model" {
        return Ok(vec![Model { name: source.model.clone(), fields: source.fields.clone(), names: source.field_names.clone() }]);
    }
    let available = invoke("modelNamesAndIds", json!({}), None)?;
    let available = setup::model_map(&available)?;
    let mut models = Vec::new();
    if available.get(&source.model).and_then(positive_id).is_some() {
        models.push(Model { name: source.model.clone(), fields: source.fields.clone(), names: source.field_names.clone() });
    }
    let candidates: Vec<(&String, &str)> = available
        .iter()
        .filter(|(model, id)| **model != source.model && positive_id(id).is_some())
        .filter_map(|(model, _)| listed_family(model, source.families.as_deref()).map(|family| (model, family)))
        .collect();
    if candidates.is_empty() {
        return Ok(models);
    }
    let actions: Vec<Value> =
        candidates.iter().map(|(model, _)| json!({ "action": "modelFieldNames", "params": { "modelName": model } })).collect();
    let replies = gateway::multi_results(&invoke("multi", json!({ "actions": actions }), None)?)?;
    let base = options::defaults()["anki"].as_object().cloned().unwrap_or_default();
    for ((model, family), reply) in candidates.into_iter().zip(&replies) {
        let fields = setup::field_list(reply)?;
        let Some(field_templates) = setup::setup_templates(family, model, source.deck.as_deref().unwrap_or("Default"), &fields, &base)
        else {
            continue;
        };
        let mut config = base.clone();
        config.insert("model".to_owned(), json!(model));
        config.insert("fieldTemplates".to_owned(), Value::Object(field_templates));
        let names = direct_expression_names(&config, Some(&fields));
        if !names.is_empty() {
            models.push(Model { name: model.clone(), fields: name_keys(&names), names });
        }
    }
    Ok(models)
}

/// `ankiListedFamily`: a model's family when the language lists it (`None`:
/// every family).
fn listed_family(model: &str, families: Option<&[Value]>) -> Option<&'static str> {
    let family = setup::family(model)?;
    families.is_none_or(|families| families.iter().any(|listed| listed == family)).then_some(family)
}

fn scoped_query(source: &Source, query: String) -> String {
    match &source.deck {
        Some(deck) if source.scope == "deck" => format!("{query} {}", search_token("deck", deck)),
        _ => query,
    }
}

fn either(clauses: Vec<String>) -> Option<String> {
    match clauses.len() {
        0 => None,
        1 => clauses.into_iter().next(),
        _ => Some(format!("({})", clauses.join(" or "))),
    }
}

/// `completeQuery`: every note of the recognised note types, in scope.
fn complete_query(source: &Source, models: &[Model]) -> Option<String> {
    either(models.iter().map(|model| search_token("note", &model.name)).collect()).map(|query| scoped_query(source, query))
}

/// `lookupQuery`: one search for every word, grouped by note type and field.
fn lookup_query(source: &Source, models: &[Model], expressions: &[&str]) -> Option<String> {
    let values: Vec<String> =
        expressions.iter().map(|expression| nfc(&templates::encode_cloze_braces(&templates::escape_html(expression)))).collect();
    let clauses: Vec<String> = models
        .iter()
        .flat_map(|model| {
            model.fields.iter().map(|field| {
                let words = either(values.iter().map(|value| search_token(field, value)).collect()).unwrap_or_default();
                format!("({} {words})", search_token("note", &model.name))
            })
        })
        .collect();
    either(clauses).map(|query| scoped_query(source, query))
}

/// `noteFields`: a `notesInfo` entry's field values.
pub(crate) fn note_fields(info: &Value) -> Result<Map<String, Value>> {
    let invalid = || Failure::new("AnkiConnect returned invalid note details.");
    let fields = info.get("fields").and_then(Value::as_object).ok_or_else(invalid)?;
    fields
        .iter()
        .map(|(field, value)| {
            let value = value.as_str().or_else(|| value.get("value").and_then(Value::as_str)).ok_or_else(invalid)?;
            Ok((field.clone(), json!(value)))
        })
        .collect()
}

struct Note<'a> {
    id: i64,
    model: &'a Model,
    fields: Map<String, Value>,
    names: HashMap<String, String>,
}

impl Note<'_> {
    /// The note's value of the field whose name key is `key`.
    fn value(&self, key: &str) -> Option<&str> {
        self.names.get(key).and_then(|name| self.fields.get(name)).and_then(Value::as_str)
    }
}

/// `indexedNotes`.
fn indexed_notes<'a>(value: &Value, models: &'a [Model], expected: &[i64]) -> Result<Vec<Note<'a>>> {
    let invalid = || Failure::new("AnkiConnect returned invalid note details.");
    let infos = value.as_array().ok_or_else(invalid)?;
    let mut seen: BTreeSet<i64> = BTreeSet::new();
    let mut notes = Vec::new();
    for info in infos {
        let id = info.get("noteId").and_then(positive_id).ok_or_else(invalid)?;
        let model_name = info.get("modelName").and_then(Value::as_str).ok_or_else(invalid)?;
        if !expected.contains(&id) || !seen.insert(id) {
            return Err(invalid());
        }
        let key = name_key(model_name);
        let model = models
            .iter()
            .rev()
            .find(|model| name_key(&model.name) == key)
            .ok_or_else(|| Failure::new("AnkiConnect returned notes outside the requested note types."))?;
        let fields = note_fields(info)?;
        let names: HashMap<String, String> = fields.keys().map(|field| (name_key(field), field.clone())).collect();
        if model.fields.iter().any(|field| !names.contains_key(field)) {
            return Err(invalid());
        }
        notes.push(Note { id, model, fields, names });
    }
    if seen.len() != expected.len() {
        return Err(invalid());
    }
    Ok(notes)
}

/// `indexedNotes` for `candidates`, reading only the models' expression fields
/// where AnkiConnect supports it (`notesInfo`'s `noteFields`). Upstream
/// AnkiConnect rejects the parameter and earlier versions ignore it; any reply
/// the index cannot use as asked for falls back to full note records. A
/// rejection is remembered until the relay restarts, so an AnkiConnect without
/// it costs one extra request in all, not one per read (each is a poll
/// interval on a popup's cache miss).
static PROJECTION_REJECTED: AtomicBool = AtomicBool::new(false);

fn read_indexed_notes<'a>(invoke: &Invoke<'_>, models: &'a [Model], candidates: &[i64], timeout: Option<u64>) -> Result<Vec<Note<'a>>> {
    let mut names: Vec<&String> = Vec::new();
    for name in models.iter().flat_map(|model| &model.names) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if !names.is_empty() && !PROJECTION_REJECTED.load(Ordering::Relaxed) {
        match invoke("notesInfo", json!({ "notes": candidates, "noteFields": names }), timeout) {
            Ok(infos) => {
                if let Ok(notes) = indexed_notes(&infos, models, candidates) {
                    return Ok(notes);
                }
            }
            Err(error) if error.message.contains("unexpected keyword argument") => PROJECTION_REJECTED.store(true, Ordering::Relaxed),
            Err(error) => return Err(error),
        }
    }
    let infos = invoke("notesInfo", json!({ "notes": candidates }), timeout)?;
    indexed_notes(&infos, models, candidates)
}

/// `returnedNoteIds`: distinct and sorted.
fn returned_note_ids(value: &Value, message: &str) -> Result<Vec<i64>> {
    let ids =
        value.as_array().and_then(|ids| ids.iter().map(positive_id).collect::<Option<Vec<i64>>>()).ok_or_else(|| Failure::new(message))?;
    Ok(ids.into_iter().collect::<BTreeSet<_>>().into_iter().collect())
}

const INVALID_IDS: &str = "AnkiConnect returned invalid note IDs.";
const INVALID_MATURE: &str = "AnkiConnect returned invalid mature note IDs.";

fn mature_query(query: &str) -> String {
    format!("{query} is:review -is:learn prop:ivl>=21")
}

fn mature_note_ids(value: &Value, candidates: &[i64]) -> Result<BTreeSet<i64>> {
    let mature: BTreeSet<i64> = returned_note_ids(value, INVALID_MATURE)?.into_iter().collect();
    if mature.iter().any(|id| !candidates.contains(id)) {
        return Err(Failure::new(INVALID_MATURE));
    }
    Ok(mature)
}

/// One index row: a word key, whether a note holding it is mature, its notes.
pub(crate) type Row = (String, bool, Vec<i64>);

/// `compactRows`.
fn compact_rows(notes: &[Note<'_>], mature: &BTreeSet<i64>) -> Vec<Row> {
    let mut rows: HashMap<String, (bool, BTreeSet<i64>)> = HashMap::new();
    for note in notes {
        for field in &note.model.fields {
            let Some(key) = stored_word_key(note.value(field)) else { continue };
            let row = rows.entry(key).or_default();
            row.1.insert(note.id);
            row.0 |= mature.contains(&note.id);
        }
    }
    let mut rows: Vec<Row> = rows.into_iter().map(|(word, (mature, ids))| (word, mature, ids.into_iter().collect())).collect();
    rows.sort_by(|left, right| js::compare(&left.0, &right.0));
    rows
}

/// `fetchAnkiIndex`: the complete index of `source`.
pub(crate) fn fetch(invoke: &Invoke<'_>, source: &Source) -> Result<Vec<Row>> {
    let models = recognized_models(invoke, source)?;
    let Some(query) = complete_query(source, &models) else { return Ok(Vec::new()) };
    let candidates = invoke("findNotes", json!({ "query": query }), Some(INDEX_TIMEOUT_MS))?;
    let mature = invoke("findNotes", json!({ "query": mature_query(&query) }), Some(INDEX_TIMEOUT_MS))?;
    let candidates = returned_note_ids(&candidates, INVALID_IDS)?;
    let mature = mature_note_ids(&mature, &candidates)?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    Ok(compact_rows(&read_indexed_notes(invoke, &models, &candidates, Some(INDEX_TIMEOUT_MS))?, &mature))
}

/// What a live lookup found for one word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) word_key: Option<String>,
    pub(crate) mature: bool,
    pub(crate) note_ids: Vec<i64>,
}

/// `lookupAnkiIndexMany`: each expression's notes, through one search.
pub(crate) fn lookup_many(invoke: &Invoke<'_>, source: &Source, expressions: &[Option<&Value>]) -> Result<Vec<Found>> {
    let keys: Vec<Option<String>> = expressions.iter().map(|expression| word_key(*expression)).collect();
    // `words`: each distinct key, and the first expression with it.
    let mut words: Vec<(String, &str)> = Vec::new();
    for (key, expression) in keys.iter().zip(expressions) {
        if let (Some(key), Some(text)) = (key, expression.and_then(Value::as_str))
            && !words.iter().any(|(known, _)| known == key)
        {
            words.push((key.clone(), text));
        }
    }
    let mut matches: HashMap<String, BTreeSet<i64>> = words.iter().map(|(key, _)| (key.clone(), BTreeSet::new())).collect();
    let mut mature = BTreeSet::new();
    if !words.is_empty() {
        let models = recognized_models(invoke, source)?;
        let texts: Vec<&str> = words.iter().map(|(_, text)| *text).collect();
        if let Some(query) = lookup_query(source, &models, &texts) {
            let actions = json!({ "actions": [
                { "action": "findNotes", "params": { "query": query } },
                { "action": "findNotes", "params": { "query": mature_query(&query) } },
            ] });
            let replies = gateway::multi_results(&invoke("multi", actions, None)?)?;
            let candidates = returned_note_ids(&replies[0], INVALID_IDS)?;
            mature = mature_note_ids(&replies[1], &candidates)?;
            if !candidates.is_empty() {
                for note in read_indexed_notes(invoke, &models, &candidates, None)? {
                    for field in &note.model.fields {
                        if let Some(found) = stored_word_key(note.value(field)).and_then(|key| matches.get_mut(&key)) {
                            found.insert(note.id);
                        }
                    }
                }
            }
        }
    }
    Ok(keys
        .into_iter()
        .map(|key| {
            let note_ids: Vec<i64> =
                key.as_ref().and_then(|key| matches.get(key)).map(|ids| ids.iter().copied().collect()).unwrap_or_default();
            Found { mature: note_ids.iter().any(|id| mature.contains(id)), note_ids, word_key: key }
        })
        .collect())
}

/// `inspectAnkiNoteIds`: whether `note_ids` still hold the word, and the first of
/// them an overwrite may target.
pub(crate) struct Inspected {
    pub(crate) stale: bool,
    pub(crate) target: Option<(i64, Map<String, Value>)>,
}

pub(crate) fn inspect(invoke: &Invoke<'_>, source: &Source, expression: Option<&Value>, note_ids: &[i64]) -> Result<Inspected> {
    if note_ids.iter().any(|id| *id <= 0) {
        return Err(Failure::new("Anki duplicate inspection requires valid note IDs."));
    }
    let ids: Vec<i64> = note_ids.iter().copied().collect::<BTreeSet<_>>().into_iter().collect();
    let infos = invoke("notesInfo", json!({ "notes": ids }), None)?;
    let invalid = || Failure::new("AnkiConnect returned invalid duplicate note details.");
    let infos = infos.as_array().ok_or_else(invalid)?;
    let mut by_id: HashMap<i64, &Value> = HashMap::new();
    for info in infos {
        let id = info.get("noteId").and_then(positive_id).ok_or_else(invalid)?;
        if !ids.contains(&id) || by_id.contains_key(&id) || !info.get("modelName").is_some_and(Value::is_string) {
            return Err(invalid());
        }
        by_id.insert(id, info);
    }
    let key = word_key(expression);
    let mut stale = by_id.len() != ids.len();
    let mut target = None;
    for id in ids {
        let Some(info) = by_id.get(&id) else { continue };
        if name_key(info["modelName"].as_str().unwrap_or_default()) != name_key(&source.model) {
            continue;
        }
        let fields = note_fields(info)?;
        let names: HashMap<String, &String> = fields.keys().map(|field| (name_key(field), field)).collect();
        let holds = key.as_ref().is_some_and(|key| {
            source.fields.iter().any(|field| {
                stored_word_key(names.get(field).and_then(|name| fields.get(*name)).and_then(Value::as_str)).as_ref() == Some(key)
            })
        });
        if !holds {
            stale = true;
            continue;
        }
        if target.is_none() {
            target = Some((id, fields));
        }
    }
    Ok(Inspected { stale, target })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_keys_follow_anki_field_search() {
        assert_eq!(word_key(Some(&json!("ＡBc<i>{x}"))).as_deref(), Some("Ａbc&lt;i&gt;&#123;x&#125;"));
        assert_eq!(word_key(Some(&json!("ABC"))), Some("abc".to_owned()));
        assert_eq!(word_key(Some(&json!(""))), None);
        assert_eq!(word_key(None), None);
        assert_eq!(escape_query("a\"b*c_d:e\\"), "a\\\"b\\*c\\_d\\:e\\\\");
    }

    #[test]
    fn a_source_keys_the_direct_expression_fields() {
        let config: Map<String, Value> = serde_json::from_value(json!({ "url": "http://127.0.0.1:8765", "apiKey": "", "model": "Kiku",
            "deck": "Mining", "duplicateScope": "deck", "fields": {}, "fieldTemplates": {
                "Expression": { "value": "{Expression}", "overwriteMode": "coalesce" },
                "Tag": { "value": "{expression}", "overwriteMode": "coalesce" },
                "Word": { "value": "{expression} ", "overwriteMode": "coalesce" } } }))
        .unwrap();
        let source = source(&config).unwrap();
        assert_eq!(source.fields, ["expression"]);
        assert_eq!(source.deck.as_deref(), Some("Mining"));
        assert_eq!(source.key.len(), 64);
        assert_eq!(source.field_names, ["Expression"]);
        let models = [Model { name: "Kiku".to_owned(), fields: source.fields.clone(), names: source.field_names.clone() }];
        assert_eq!(complete_query(&source, &models).unwrap(), "\"note:Kiku\" \"deck:Mining\"");
        assert_eq!(
            lookup_query(&source, &models, &["猫", "犬"]).unwrap(),
            "(\"note:Kiku\" (\"expression:猫\" or \"expression:犬\")) \"deck:Mining\""
        );
    }

    // anki-index-languages.test.mjs: "a language without note-type families keys
    // its source with them and recognises no family note type beside its own".
    #[test]
    fn a_language_without_families_keys_them_and_recognises_none_beside_its_own() {
        let mut config = options::normalise(Some(&json!({ "anki": { "model": "English", "duplicateScope": "all",
            "fields": { "expression": "Expression" } } })))["anki"]
            .as_object()
            .cloned()
            .unwrap();
        let japanese = source(&config).unwrap();
        let mut english_config = config.clone();
        english_config.insert("noteTypeFamilies".to_owned(), json!([]));
        let english = source(&english_config).unwrap();
        assert_eq!(japanese.families, None);
        assert_eq!(english.families, Some(Vec::new()));
        assert_ne!(english.key, japanese.key);
        config.insert("duplicateScope".to_owned(), json!("model"));
        let by_model = source(&config).unwrap().key;
        config.insert("noteTypeFamilies".to_owned(), json!([]));
        assert_eq!(source(&config).unwrap().key, by_model, "a note-type scope recognises nothing else");
        let asked = std::cell::RefCell::new(Vec::new());
        let invoke = |action: &str, params: Value, _: Option<u64>| -> Result<Value> {
            asked.borrow_mut().push(action.to_owned());
            match action {
                "modelNamesAndIds" => Ok(json!({ "English": 1, "Kiku v2": 2 })),
                "findNotes" => {
                    assert!(!params["query"].as_str().unwrap().contains("Kiku"), "a Kiku note type is not an English duplicate");
                    Ok(json!([]))
                }
                other => panic!("unexpected {other}"),
            }
        };
        assert_eq!(fetch(&invoke, &english).unwrap(), Vec::<Row>::new());
        assert_eq!(*asked.borrow(), ["modelNamesAndIds", "findNotes", "findNotes"]);
    }

    #[test]
    fn index_reads_project_fields_and_fall_back_to_full_records() {
        use std::cell::RefCell;
        let models = [Model { name: "Lapis".to_owned(), fields: vec!["expression".to_owned()], names: vec!["Expression".to_owned()] }];
        let full = json!([{ "noteId": 7, "modelName": "Lapis", "fields": {
            "Expression": { "value": "猫", "order": 0 }, "Glossary": { "value": "<ol>…</ol>", "order": 1 } } }]);
        let projected = json!([{ "noteId": 7, "modelName": "Lapis", "fields": { "Expression": { "value": "猫", "order": 0 } } }]);
        let missing = json!([{ "noteId": 7, "modelName": "Lapis", "fields": {} }]);
        let stock = "notesInfo() got an unexpected keyword argument 'noteFields'";
        // What each server answers to a projected read; plain reads get full records.
        // A rejection is last: after it, reads are plain (1 call).
        for (answer, calls) in [(Ok(projected), 1), (Ok(full.clone()), 1), (Ok(missing), 2), (Err(stock), 2), (Ok(json!(null)), 1)] {
            let sent = RefCell::new(Vec::new());
            let invoke = |action: &str, params: Value, _: Option<u64>| -> Result<Value> {
                assert_eq!(action, "notesInfo");
                sent.borrow_mut().push(params.clone());
                if params.get("noteFields").is_none() {
                    return Ok(full.clone());
                }
                assert_eq!(params["noteFields"], json!(["Expression"]));
                answer.clone().map_err(|error| Failure::new(gateway::describe(error)))
            };
            let notes = read_indexed_notes(&invoke, &models, &[7], None).unwrap();
            assert_eq!(notes.len(), 1);
            assert_eq!(notes[0].value("expression"), Some("猫"));
            assert_eq!(sent.borrow().len(), calls);
        }
        // Other failures are not retried.
        let invoke = |_: &str, _: Value, _: Option<u64>| -> Result<Value> { Err(Failure::new("AnkiConnect: collection is not available")) };
        assert!(read_indexed_notes(&invoke, &models, &[7], None).is_err());
    }
}
