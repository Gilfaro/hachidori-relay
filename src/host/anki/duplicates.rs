// SPDX-License-Identifier: GPL-3.0-or-later
//! Anki notes, their duplicates and Anki's add checks (`anki-duplicates.js`).

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::gateway::{self, Failure, Result};
use super::{Invoke, positive_id, templates};
use crate::host::js;

/// A note's fields, in the note type's order.
pub(crate) type Fields = Vec<(String, String)>;

/// The value of `field` in `fields`, if it is there.
pub(crate) fn field<'a>(fields: &'a Fields, name: &str) -> Option<&'a str> {
    fields.iter().find(|(known, _)| known == name).map(|(_, value)| value.as_str())
}

/// Sets `name` to `value`, keeping its place (or adding it last).
pub(crate) fn set_field(fields: &mut Fields, name: &str, value: String) {
    match fields.iter_mut().find(|(known, _)| known == name) {
        Some(slot) => slot.1 = value,
        None => fields.push((name.to_owned(), value)),
    }
}

pub(crate) fn fields_json(fields: &Fields) -> Value {
    Value::Object(fields.iter().map(|(name, value)| (name.clone(), json!(value))).collect())
}

/// A note as AnkiConnect's `addNote` takes it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Note {
    pub(crate) deck: String,
    pub(crate) model: String,
    pub(crate) fields: Fields,
    pub(crate) options: Map<String, Value>,
    pub(crate) tags: Vec<String>,
}

impl Note {
    pub(crate) fn to_json(&self) -> Value {
        json!({ "deckName": self.deck, "modelName": self.model, "fields": fields_json(&self.fields), "options": self.options, "tags": self.tags })
    }

    /// The note with its options' `allowDuplicate` set.
    fn checked(&self, allow_duplicate: bool) -> Value {
        let mut options = self.options.clone();
        options.insert("allowDuplicate".to_owned(), json!(allow_duplicate));
        json!({ "deckName": self.deck, "modelName": self.model, "fields": fields_json(&self.fields), "tags": self.tags, "options": options })
    }
}

/// The Templates a note's fields come from, in field order.
pub(crate) type Templates = Vec<(String, Map<String, Value>)>;

fn template_value(template: &Map<String, Value>) -> &str {
    template.get("value").and_then(Value::as_str).unwrap_or_default()
}

/// `isAnkiDuplicateError`.
pub(crate) fn is_duplicate_error(error: &str) -> bool {
    error.to_lowercase().contains("cannot create note because it is a duplicate")
}

/// `ankiBrowseQuery`.
pub(crate) fn browse_query(expression: &str) -> String {
    let escaped = expression.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!("\"{}\"", super::index::escape_query(&escaped))
}

/// `ankiNoteIdsQuery`.
pub(crate) fn note_ids_query(note_ids: &[i64]) -> Result<String> {
    if note_ids.is_empty() || note_ids.iter().any(|id| *id <= 0) {
        return Err(Failure::new("Anki browse requires valid note IDs."));
    }
    let mut distinct: Vec<i64> = Vec::new();
    for id in note_ids {
        if !distinct.contains(id) {
            distinct.push(*id);
        }
    }
    Ok(format!("nid:{}", distinct.iter().map(i64::to_string).collect::<Vec<_>>().join(",")))
}

/// `ankiNoteOptions`.
pub(crate) fn note_options(config: &Map<String, Value>) -> Map<String, Value> {
    let deck = config.get("duplicateScope").and_then(Value::as_str) == Some("deck");
    let value = json!({
        "allowDuplicate": config.get("duplicateBehavior").and_then(Value::as_str) == Some("new"),
        "duplicateScope": if deck { "deck" } else { "collection" },
        "duplicateScopeOptions": {
            "deckName": if deck { config.get("deck").cloned().unwrap_or(Value::Null) } else { Value::Null },
            "checkChildren": deck,
            "checkAllModels": false,
        },
    });
    value.as_object().cloned().unwrap_or_default()
}

/// `overwriteValue`.
fn overwrite_value(existing: &str, incoming: &str, mode: &str) -> String {
    match mode {
        "overwrite" => incoming.to_owned(),
        "skip" => existing.to_owned(),
        "append" => format!("{existing}{incoming}"),
        "prepend" => format!("{incoming}{existing}"),
        "coalesce-new" => if incoming.is_empty() { existing } else { incoming }.to_owned(),
        _ => if existing.is_empty() { incoming } else { existing }.to_owned(),
    }
}

const SENTENCE_MARKERS: [&str; 6] =
    ["sentence", "sentence-furigana", "sentence-furigana-plain", "cloze-prefix", "cloze-body", "cloze-suffix"];
const SCENE_MEDIA_MARKERS: [&str; 3] = ["screenshot", "gif", "sentence-audio"];

/// `ankiUpdateKeepsField`: whether updating a note from `request` leaves the field
/// `template` fills alone, because the lookup's sentence is only the matched word.
pub(crate) fn update_keeps_field(template: &Map<String, Value>, request: &Value) -> bool {
    let markers = templates::marker_names(template_value(template));
    let scene = markers.iter().any(|name| SENTENCE_MARKERS.contains(&name.as_str()))
        || (!markers.is_empty() && markers.iter().all(|name| SCENE_MEDIA_MARKERS.contains(&name.as_str())));
    if !scene || js::truthy(request.get("netflix").and_then(|netflix| netflix.get("cue"))) {
        return false;
    }
    let (Some(sentence), Some(matched)) = (request.get("sentence").and_then(Value::as_str), request.get("matched").and_then(Value::as_str))
    else {
        return false;
    };
    let offset = js::to_number(request.get("matchOffset"));
    js::slice(sentence, 0.0, Some(offset)).is_empty()
        && js::slice(sentence, offset + f64::from(u32::try_from(js::length(matched)).unwrap_or(u32::MAX)), None).is_empty()
}

/// `canonicalAnkiFields`: `templates` and `fields` under the existing note's own
/// field names.
pub(crate) fn canonical_fields(fields: &Fields, templates: &Templates, existing: &Fields) -> Result<(Templates, Fields)> {
    let names: HashMap<String, &String> = existing.iter().map(|(name, _)| (name.to_lowercase(), name)).collect();
    let mut canonical_templates = Vec::new();
    let mut incoming = Vec::new();
    for (name, template) in templates {
        let found = if existing.iter().any(|(known, _)| known == name) { Some(name) } else { names.get(&name.to_lowercase()).copied() };
        let Some(found) = found else {
            let known = existing.iter().map(|(field, _)| format!("“{field}”")).collect::<Vec<_>>().join(", ");
            return Err(Failure::new(format!(
                "Anki's note has no field “{name}” to overwrite; its fields are {}. The note type's fields changed. Refresh fields in \
                 Anki Settings before overwriting this note.",
                if known.is_empty() { "unknown".to_owned() } else { known }
            )));
        };
        set_template(&mut canonical_templates, found, template.clone());
        set_field(&mut incoming, found, field(fields, name).unwrap_or_default().to_owned());
    }
    Ok((canonical_templates, incoming))
}

fn set_template(templates: &mut Templates, name: &str, template: Map<String, Value>) {
    match templates.iter_mut().find(|(known, _)| known == name) {
        Some(slot) => slot.1 = template,
        None => templates.push((name.to_owned(), template)),
    }
}

/// `overwriteAnkiFields`: each Template's field, its existing and incoming values
/// combined by the Template's overwrite mode.
pub(crate) fn overwrite_fields(incoming: &Fields, existing: &Fields, templates: &Templates, include_audio: bool) -> Fields {
    templates
        .iter()
        .filter(|(_, template)| include_audio || !templates::is_audio_only(template_value(template)))
        .map(|(name, template)| {
            let mode = template.get("overwriteMode").and_then(Value::as_str).unwrap_or_default();
            (name.clone(), overwrite_value(field(existing, name).unwrap_or_default(), field(incoming, name).unwrap_or_default(), mode))
        })
        .collect()
}

const INVALID_CHECKS: &str = "AnkiConnect returned invalid duplicate check results.";

fn check_results(result: &Value, count: usize, detailed: bool) -> Result<Vec<Value>> {
    let valid = result.as_array().filter(|items| {
        items.len() == count
            && items.iter().all(|item| if detailed { item.get("canAdd").is_some_and(Value::is_boolean) } else { item.is_boolean() })
    });
    valid.cloned().ok_or_else(|| Failure::new(INVALID_CHECKS))
}

fn unsupported(error: &Failure) -> bool {
    error.message.to_lowercase().contains("unsupported action")
}

fn detail_error(result: &Value) -> Option<String> {
    result.get("error").and_then(Value::as_str).filter(|error| !error.is_empty()).map(str::to_owned)
}

/// `checkAnkiDuplicate`: Anki's add check with its duplicate rule.
pub(crate) struct Checked {
    pub(crate) duplicate: bool,
    pub(crate) addable: bool,
    pub(crate) error: Option<String>,
}

pub(crate) fn check_duplicate(invoke: &Invoke<'_>, note: &Note) -> Result<Checked> {
    let result = match invoke("canAddNotesWithErrorDetail", json!({ "notes": [note.checked(false)] }), None) {
        Ok(result) => check_results(&result, 1, true)?.remove(0),
        Err(error) if unsupported(&error) => {
            let allowed = check_results(&invoke("canAddNotes", json!({ "notes": [note.checked(true)] }), None)?, 1, false)?[0] == true;
            let prevented = check_results(&invoke("canAddNotes", json!({ "notes": [note.checked(false)] }), None)?, 1, false)?[0] == true;
            return Ok(Checked { duplicate: allowed && !prevented, addable: allowed && prevented, error: None });
        }
        Err(error) => return Err(error),
    };
    let error = detail_error(&result);
    Ok(Checked { duplicate: error.as_deref().is_some_and(is_duplicate_error), addable: result["canAdd"] == true && error.is_none(), error })
}

/// `validateAnkiNotes`: Anki's add check for each note, without its duplicate rule.
pub(crate) fn validate_notes(invoke: &Invoke<'_>, notes: &[&Note]) -> Result<Vec<(bool, Option<String>)>> {
    if notes.is_empty() {
        return Ok(Vec::new());
    }
    let checks: Vec<Value> = notes.iter().map(|note| note.checked(true)).collect();
    match invoke("canAddNotesWithErrorDetail", json!({ "notes": checks }), None) {
        Ok(result) => Ok(check_results(&result, checks.len(), true)?
            .iter()
            .map(|result| {
                let error = detail_error(result);
                (result["canAdd"] == true && error.is_none(), error)
            })
            .collect()),
        Err(error) if unsupported(&error) => {
            Ok(check_results(&invoke("canAddNotes", json!({ "notes": checks }), None)?, checks.len(), false)?
                .iter()
                .map(|addable| if *addable == true { (true, None) } else { (false, Some("Anki rejected this note.".to_owned())) })
                .collect())
        }
        Err(error) => Err(error),
    }
}

/// `validateAnkiNote`.
pub(crate) fn validate_note(invoke: &Invoke<'_>, note: &Note) -> Result<(bool, Option<String>)> {
    Ok(validate_notes(invoke, &[note])?.remove(0))
}

/// pylib/anki/consts.py's `MODEL_CLOZE`.
const MODEL_CLOZE: i64 = 1;

/// `clozeDeletion`: the first complete top-level deletion with a nonzero number.
fn cloze_deletion(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    // Each opener's start and its cloze numbers.
    let mut open: Vec<(usize, &str)> = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if text[at..].starts_with("{{c") {
            // `(,*\d[\d,]*)::`
            let rest = &bytes[at + 3..];
            let commas = rest.iter().take_while(|byte| **byte == b',').count();
            if rest.get(commas).is_some_and(u8::is_ascii_digit) {
                let digits = commas + rest[commas..].iter().take_while(|byte| byte.is_ascii_digit() || **byte == b',').count();
                if rest[digits..].starts_with(b"::") {
                    open.push((at, &text[at + 3..at + 3 + digits]));
                    at += 3 + digits + 2;
                    continue;
                }
            }
        }
        if text[at..].starts_with("}}") {
            if open.len() == 1 && open[0].1.bytes().any(|byte| (b'1'..=b'9').contains(&byte)) {
                return Some(&text[open[0].0..at + 2]);
            }
            open.pop();
            at += 2;
            continue;
        }
        at += text[at..].chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// Text with every `<!-- ... -->` comment removed.
fn without_comments(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        let Some(end) = rest[start + 4..].find("-->") else { break };
        out.push_str(&rest[..start]);
        rest = &rest[start + 4 + end + 3..];
    }
    out.push_str(rest);
    out
}

/// `clozeFields`: the fields the first card's front renders through `cloze:`.
fn cloze_fields(model: &Value) -> Vec<String> {
    let template = without_comments(model["tmpls"][0]["qfmt"].as_str().unwrap_or_default());
    let mut referenced: Vec<String> = Vec::new();
    let mut rest = template.as_str();
    while let Some(start) = rest.find("{{") {
        let inner = &rest[start + 2..];
        match inner.find(['{', '}']) {
            Some(end) if inner[end..].starts_with("}}") => {
                let tag = js::trim(&inner[..end]);
                let mut parts: Vec<&str> = tag.split(':').collect();
                parts.reverse();
                if parts[1..].contains(&"cloze") {
                    referenced.push(parts[0].to_lowercase());
                }
                rest = &inner[end + 2..];
            }
            _ => rest = &rest[start + 1..],
        }
    }
    model["flds"]
        .as_array()
        .map(|fields| {
            fields
                .iter()
                .filter_map(|field| field["name"].as_str())
                .filter(|name| referenced.contains(&name.to_lowercase()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// `clozeRule`: why Anki refused the note's cloze deletions, in Anki's order.
fn cloze_rule(model: &Value, note: &Note, refused: &str, templates: &Templates) -> Option<String> {
    let values: HashMap<String, &str> = note.fields.iter().map(|(name, value)| (name.to_lowercase(), value.as_str())).collect();
    let deletions: Vec<(&str, &str)> = model["flds"]
        .as_array()
        .map(|fields| {
            fields
                .iter()
                .filter_map(|field| field["name"].as_str())
                .filter_map(|name| {
                    cloze_deletion(values.get(&name.to_lowercase()).copied().unwrap_or_default()).map(|deletion| (name, deletion))
                })
                .collect()
        })
        .unwrap_or_default();
    let model_name = format!("“{}”", note.model);
    if model["type"].as_i64() != Some(MODEL_CLOZE) {
        let &(field, deletion) = deletions.first()?;
        let refusal =
            format!("{refused}: field “{field}” contains the cloze deletion “{deletion}”, but {model_name} is not a Cloze note type.");
        let template =
            templates.iter().find(|(name, _)| name.to_lowercase() == field.to_lowercase()).map(|(_, template)| template_value(template));
        if let Some(template) = template
            && cloze_deletion(template).is_none()
        {
            let mut names: Vec<String> = Vec::new();
            for name in templates::marker_names(template) {
                let marker = format!("{{{name}}}");
                if !names.contains(&marker) {
                    names.push(marker);
                }
            }
            let markers = if names.is_empty() { "its markers".to_owned() } else { names.join(", ") };
            return Some(format!(
                "{refusal} The deletion comes from the content of {markers}, not from the field's template. Map that field to other content in \
                 Anki Settings, or choose a Cloze note type."
            ));
        }
        return Some(format!("{refusal} Remove the deletion from that field's template in Anki Settings, or choose a Cloze note type."));
    }
    let clozable = cloze_fields(model);
    let first = clozable.first()?;
    if let Some((field, deletion)) = deletions.iter().find(|(field, _)| !clozable.iter().any(|name| name == field)) {
        let names = clozable.iter().map(|name| format!("“{name}”")).collect::<Vec<_>>().join(", ");
        return Some(format!(
            "{refused}: field “{field}” contains the cloze deletion “{deletion}”, but {model_name} makes cloze cards only from {names}. Move \
             the deletion to the template of “{first}” in Anki Settings."
        ));
    }
    if !deletions.is_empty() {
        return None;
    }
    Some(format!(
        "{refused}: it is a Cloze note type, but its cloze field “{first}” has no cloze deletion such as {{{{c1::…}}}}. Map “{first}” to a \
         template that makes one, for example {{cloze-prefix}}{{{{c1::{{cloze-body}}}}}}{{cloze-suffix}}, or choose a non-Cloze note type \
         in Anki Settings."
    ))
}

/// `explainAnkiRefusal`: the reader-facing text of a per-note refusal.
pub(crate) fn explain_refusal(invoke: &Invoke<'_>, note: &Note, error: &str, templates: &Templates) -> String {
    if !gateway::is_cloze_refusal(error) {
        return gateway::explain(error).unwrap_or_else(|| error.to_owned());
    }
    let refused = format!("Anki refused the note for deck “{}”, note type “{}”", note.deck, note.model);
    let explained = invoke("findModelsByName", json!({ "modelNames": [note.model] }), None).ok().and_then(|models| {
        let model = models.get(0)?.clone();
        // A note type AnkiConnect could not describe falls back to the shared rules.
        model.get("tmpls").and_then(|templates| templates.get(0))?;
        Some(cloze_rule(&model, note, &refused, templates))
    });
    explained.flatten().unwrap_or_else(|| gateway::cloze_refusal(&refused))
}

/// `duplicateQuery`: Anki's `dupe:` search for the note's first field.
fn duplicate_query(note: &Note, first_field: &str, model_id: i64) -> String {
    let text = field(&note.fields, first_field).unwrap_or_default().replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"dupe:{model_id},{text}\"")
}

/// `scopedNoteIds`: in deck scope, the notes with a card inside the deck.
fn scoped_note_ids(invoke: &Invoke<'_>, infos: &[Value], config: &Map<String, Value>) -> Result<Option<Vec<i64>>> {
    if config.get("duplicateScope").and_then(Value::as_str) != Some("deck") {
        return Ok(None);
    }
    let ids: Vec<i64> =
        infos.iter().filter_map(|info| info.get("cards").and_then(Value::as_array)).flatten().filter_map(positive_id).collect();
    if ids.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let cards = invoke("cardsInfo", json!({ "cards": ids }), None)?;
    let cards = cards.as_array().ok_or_else(|| Failure::new("AnkiConnect returned invalid duplicate card details."))?;
    let exact = config.get("deck").and_then(Value::as_str).unwrap_or_default().to_lowercase();
    Ok(Some(
        cards
            .iter()
            .filter_map(|card| {
                let deck = card.get("deckName")?.as_str()?.to_lowercase();
                let note = card.get("note").and_then(positive_id)?;
                (deck == exact || deck.starts_with(&format!("{exact}::"))).then_some(note)
            })
            .collect(),
    ))
}

/// A duplicate Anki found.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Duplicate {
    pub(crate) note_id: i64,
    pub(crate) model_name: String,
    pub(crate) fields: Fields,
}

/// `findAnkiDuplicateNotes`: Anki's exact first-field duplicates of the configured
/// note type, in scope.
pub(crate) fn find_duplicates(invoke: &Invoke<'_>, note: &Note, first_field: &str, config: &Map<String, Value>) -> Result<Vec<Duplicate>> {
    let models = invoke("modelNamesAndIds", Value::Null, None)?;
    let model = config.get("model").and_then(Value::as_str).unwrap_or_default();
    let model_id = models
        .as_object()
        .and_then(|models| models.get(model))
        .and_then(positive_id)
        .ok_or_else(|| Failure::new("AnkiConnect returned no valid ID for the selected note type."))?;
    let found = invoke("findNotes", json!({ "query": duplicate_query(note, first_field, model_id) }), None)?;
    let found = found
        .as_array()
        .and_then(|ids| ids.iter().map(positive_id).collect::<Option<Vec<i64>>>())
        .ok_or_else(|| Failure::new("AnkiConnect returned invalid duplicate note IDs."))?;
    let mut ids: Vec<i64> = Vec::new();
    for id in found {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let infos = invoke("notesInfo", json!({ "notes": ids }), None)?;
    let infos = infos.as_array().ok_or_else(|| Failure::new("AnkiConnect returned invalid duplicate note details."))?;
    let scoped = scoped_note_ids(invoke, infos, config)?;
    let mut by_id: HashMap<i64, &Value> = HashMap::new();
    for info in infos {
        if let Some(id) = info.get("noteId").and_then(positive_id) {
            by_id.insert(id, info);
        }
    }
    let mut matches = Vec::new();
    for id in ids {
        if scoped.as_ref().is_some_and(|scoped| !scoped.contains(&id)) {
            continue;
        }
        let Some(info) = by_id.get(&id) else { continue };
        let Some(model_name) = info.get("modelName").and_then(Value::as_str).filter(|name| name.to_lowercase() == model.to_lowercase())
        else {
            continue;
        };
        let Some(fields) = info.get("fields").and_then(Value::as_object) else { continue };
        let fields = fields
            .iter()
            .map(|(name, value)| {
                value
                    .as_str()
                    .or_else(|| value.get("value").and_then(Value::as_str))
                    .map(|value| (name.clone(), value.to_owned()))
                    .ok_or_else(|| Failure::new("AnkiConnect returned invalid duplicate note fields."))
            })
            .collect::<Result<Fields>>()?;
        matches.push(Duplicate { note_id: id, model_name: model_name.to_owned(), fields });
    }
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloze_deletions_are_found_as_anki_tokenizes_them() {
        assert_eq!(cloze_deletion("a {{c1::猫}} b"), Some("{{c1::猫}}"));
        assert_eq!(cloze_deletion("{{c0::x}} {{c2::y}}"), Some("{{c2::y}}"));
        assert_eq!(cloze_deletion("{{c1::{{c2::x}}}}"), Some("{{c1::{{c2::x}}}}"));
        assert_eq!(cloze_deletion("}} {{c::x}}"), None);
        assert_eq!(cloze_deletion("{{c,1::x}}"), Some("{{c,1::x}}"));
    }

    #[test]
    fn cloze_fields_follow_the_front_template() {
        let model = json!({ "type": 1, "tmpls": [{ "qfmt": "<!-- {{cloze:Back}} -->{{cloze:Text}}{{furigana:cloze:Extra}}{{Front}}" }],
            "flds": [{ "name": "Text" }, { "name": "Back" }, { "name": "extra" }] });
        assert_eq!(cloze_fields(&model), ["Text", "extra"]);
    }

    #[test]
    fn overwrite_modes_combine_values() {
        let templates: Templates = ["overwrite", "skip", "append", "prepend", "coalesce-new", "coalesce"]
            .iter()
            .map(|mode| ((*mode).to_owned(), json!({ "value": "{expression}", "overwriteMode": mode }).as_object().unwrap().clone()))
            .collect();
        let existing: Fields = templates.iter().map(|(name, _)| (name.clone(), "old".to_owned())).collect();
        let incoming: Fields = templates
            .iter()
            .map(|(name, _)| (name.clone(), if name == "coalesce-new" { String::new() } else { "new".to_owned() }))
            .collect();
        let values: Vec<String> = overwrite_fields(&incoming, &existing, &templates, false).into_iter().map(|(_, value)| value).collect();
        assert_eq!(values, ["new", "old", "oldnew", "newold", "old", "old"]);
    }

    #[test]
    fn an_update_keeps_sentence_fields_for_a_word_alone() {
        let template = json!({ "value": "{sentence}" }).as_object().unwrap().clone();
        assert!(update_keeps_field(&template, &json!({ "sentence": "猫", "matched": "猫", "matchOffset": 0 })));
        assert!(!update_keeps_field(&template, &json!({ "sentence": "黒い猫", "matched": "猫", "matchOffset": 2 })));
        assert!(!update_keeps_field(&template, &json!({ "sentence": "猫", "matched": "猫", "matchOffset": 0, "netflix": { "cue": 1 } })));
        assert_eq!(browse_query("a<b>\"c\""), "\"a&lt;b&gt;\\\"c\\\"\"");
        assert_eq!(note_ids_query(&[3, 1, 3]).unwrap(), "nid:3,1");
    }
}
