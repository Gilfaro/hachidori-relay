// SPDX-License-Identifier: GPL-3.0-or-later
//! `hd_anki_submit`: the note written to Anki. The extension's `write` and
//! `writeAnkiNote` (`anki-mining.js`), the worker's media hooks and
//! `submitClient` (`anki-worker.js`), and pronunciation enrichment
//! (`anki-enrichment.js`).

use std::collections::HashSet;
use std::sync::PoisonError;

use serde_json::{Map, Value, json};

use super::duplicates::{self, Fields, Note, Templates, fields_json};
use super::gateway::{Failure, Result};
use super::glossary::Layout;
use super::media::{self, AddFile, ClientMedia, DictionaryMedia};
use super::preflight::{self, Applied, Decision, Prepared, Resources};
use super::{Anki, CONFIG_CHANGED, Invoke, index, positive_id, templates};
use crate::host::js;

const WRITE_TIMEOUT_MS: u64 = 10_000;
const STORE_TIMEOUT_MS: u64 = 30_000;
const SCREENSHOT_REPLACED: &str = "the captured picture was replaced before this note was saved.";

type Reply = std::result::Result<Map<String, Value>, String>;

/// `readAnkiNoteFields`: the saved note's fields.
fn read_note_fields(invoke: &Invoke<'_>, note_id: i64) -> Result<Map<String, Value>> {
    let infos = invoke("notesInfo", json!({ "notes": [note_id] }), None)?;
    let fields = infos
        .as_array()
        .and_then(|infos| infos.iter().find(|info| info.get("noteId").and_then(positive_id) == Some(note_id)))
        .and_then(|info| info.get("fields"))
        .and_then(Value::as_object)
        .ok_or_else(|| Failure::new("Anki did not return the saved note fields."))?;
    Ok(fields.iter().map(|(name, value)| (name.clone(), value.get("value").cloned().unwrap_or(Value::Null))).collect())
}

/// `ankiFieldDifferences`: the submitted fields Anki lost or changed, and what to say.
fn field_differences(saved: &Map<String, Value>, note_id: i64, expected: &Fields) -> Option<(Vec<String>, String)> {
    let mut missing = Vec::new();
    let mut changed = Vec::new();
    for (field, value) in expected {
        match saved.get(field).and_then(Value::as_str) {
            None => missing.push(field.clone()),
            Some(saved) if index::nfc(saved) != index::nfc(value) => changed.push(field.clone()),
            Some(_) => {}
        }
    }
    if missing.is_empty() && changed.is_empty() {
        return None;
    }
    let list = |names: &[String]| names.iter().map(|name| format!("“{name}”")).collect::<Vec<_>>().join(", ");
    let mut parts = Vec::new();
    if !missing.is_empty() {
        let (noun, verb) = if missing.len() == 1 { ("field", "is") } else { ("fields", "are") };
        parts.push(format!("{noun} {} {verb} missing from note {note_id}", list(&missing)));
    }
    if !changed.is_empty() {
        let (noun, verb) = if changed.len() == 1 { ("field", "was") } else { ("fields", "were") };
        parts.push(format!("{noun} {} {verb} saved with different content", list(&changed)));
    }
    let message = format!("Anki's saved note differs from the submitted values: {}. Inspect note {note_id} in Anki.", parts.join("; "));
    Some(([missing, changed].concat(), message))
}

/// `verifyAnkiFields`.
fn verify_fields(invoke: &Invoke<'_>, note_id: i64, expected: &Fields) -> Result<()> {
    match field_differences(&read_note_fields(invoke, note_id)?, note_id, expected) {
        Some((_, message)) => Err(Failure::new(message)),
        None => Ok(()),
    }
}

/// `addNoteContext`: Anki's "empty" refusal, with the note and field it means.
fn add_note_context(error: Failure, note: &Note) -> Failure {
    if !error.message.to_lowercase().contains("cannot create note because it is empty") {
        return error;
    }
    let (first_field, first_value) = note.fields.first().map(|(name, value)| (name.as_str(), value.as_str())).unwrap_or_default();
    let stripped = if js::trim(first_value).is_empty() { "" } else { " once Anki stripped its formatting" };
    let detail = format!(
        "Anki refused the note for deck “{}”, note type “{}” because its first field “{first_field}” is empty{stripped}.",
        note.deck, note.model
    );
    let message = &error.message;
    // `/\(AnkiConnect: (.+)\)$/u`.
    let raw = message
        .find("(AnkiConnect: ")
        .filter(|_| message.ends_with(')'))
        .map(|start| &message[start + "(AnkiConnect: ".len()..message.len() - 1])
        .filter(|raw| !raw.is_empty())
        .unwrap_or(message);
    let raw = raw.strip_prefix("AnkiConnect: ").unwrap_or(raw);
    Failure::new(format!("{detail} (AnkiConnect: {raw})"))
}

/// `writeAnkiNote`: the note added, or the target's fields updated; its ID.
fn write_note(invoke: &Invoke<'_>, note: &Note, target: Option<i64>, fields: &Fields) -> Result<i64> {
    let note_id = if let Some(id) = target {
        let reply = invoke("updateNoteFields", json!({ "note": { "id": id, "fields": fields_json(fields) } }), Some(WRITE_TIMEOUT_MS))?;
        if !reply.is_null() {
            return Err(Failure::new("Anki returned an invalid field-update acknowledgement."));
        }
        Some(id)
    } else {
        positive_id(
            &invoke("addNote", json!({ "note": note.to_json() }), Some(WRITE_TIMEOUT_MS)).map_err(|error| add_note_context(error, note))?,
        )
    };
    note_id.filter(|id| *id > 0).ok_or_else(|| {
        Failure::new(format!(
            "Anki did not return a valid note ID for the {} note in deck “{}”. Check the note in Anki.",
            if target.is_some() { "updated" } else { "added" },
            note.deck
        ))
    })
}

/// `releaseCapturedMedia`: a note that was definitively not written leaves no
/// captured media of its own behind.
fn release_captured(invoke: &Invoke<'_>, captured: Option<&str>) {
    if let Some(filename) = captured.filter(|filename| !filename.is_empty()) {
        let _ = invoke("deleteMediaFile", json!({ "filename": filename }), Some(WRITE_TIMEOUT_MS));
    }
}

/// What `prepareWrite` or `prepareAdd` did: its warnings, the screenshot it
/// stored or attached, and the note's native attachments.
#[derive(Default)]
struct WriteResources {
    warnings: Vec<String>,
    screenshot: Option<String>,
    attachments: Map<String, Value>,
}

/// `takePendingMedia("screenshot")`'s answer.
enum Taken {
    /// The fields reference no screenshot.
    None,
    /// The screenshot cannot go with the note; its references are gone.
    Without(String),
    /// The linked browser's screenshot for this note: its filename and data.
    Pending(String, String),
}

/// The note goes without the screenshot, and so does a later render.
fn without_screenshot(request: &mut Value, fields: &mut Fields, filename: &str, reason: &str) -> String {
    let reference = format!("<img src=\"{filename}\">");
    let mut unavailable = request.get("captureUnavailable").and_then(Value::as_array).cloned().unwrap_or_default();
    unavailable.push(json!("screenshot"));
    request["captureUnavailable"] = Value::Array(unavailable);
    for (_, value) in fields.iter_mut() {
        *value = value.replace(&reference, "");
    }
    format!("Screenshot: {reason}")
}

/// `takePendingMedia("screenshot")`: the linked browser's screenshot, if the
/// fields still reference it and it is the one this note captured.
fn take_screenshot(request: &mut Value, fields: &mut Fields, media: &ClientMedia) -> Taken {
    let Some(filename) = request.get("screenshot").and_then(|screenshot| screenshot.get("filename")).and_then(Value::as_str) else {
        return Taken::None;
    };
    let filename = filename.to_owned();
    let reference = format!("<img src=\"{filename}\">");
    if filename.is_empty() || !fields.iter().any(|(_, value)| value.contains(&reference)) {
        return Taken::None;
    }
    let token = request["screenshot"].get("token").cloned();
    match media.screenshot.as_ref().filter(|pending| Some(json!(pending.token)) == token && pending.filename == filename) {
        Some(pending) => Taken::Pending(filename, pending.data.clone()),
        None => Taken::Without(without_screenshot(request, fields, &filename, SCREENSHOT_REPLACED)),
    }
}

/// `storePendingMedia("screenshot")`: the linked browser's screenshot, stored if
/// the fields still reference it; otherwise the references go.
fn store_screenshot(invoke: &Invoke<'_>, request: &mut Value, fields: &mut Fields, media: &ClientMedia) -> WriteResources {
    let (filename, data) = match take_screenshot(request, fields, media) {
        Taken::None => return WriteResources::default(),
        Taken::Without(warning) => return WriteResources { warnings: vec![warning], ..WriteResources::default() },
        Taken::Pending(filename, data) => (filename, data),
    };
    let stored = invoke("storeMediaFile", json!({ "filename": filename, "data": data, "deleteExisting": false }), Some(STORE_TIMEOUT_MS))
        .and_then(|stored| {
            if stored.as_str() == Some(filename.as_str()) {
                Ok(())
            } else {
                Err(Failure::new("Anki stored it under a different filename."))
            }
        });
    match stored {
        Ok(()) => WriteResources { screenshot: Some(filename), ..WriteResources::default() },
        Err(error) => {
            // The store may have happened although its answer did not arrive.
            release_captured(invoke, Some(&filename));
            WriteResources { warnings: vec![without_screenshot(request, fields, &filename, &error.message)], ..WriteResources::default() }
        }
    }
}

/// `pronunciationFields`: the enriched values of fields nobody changed meanwhile.
fn pronunciation_fields(
    incoming: &Fields,
    saved: &Map<String, Value>,
    applied: &Fields,
    existing: Option<&Fields>,
    warnings: &mut Vec<String>,
) -> Fields {
    let mut fields = Fields::new();
    for (field, value) in incoming {
        let baseline = duplicates::field(applied, field)
            .or_else(|| existing.and_then(|existing| duplicates::field(existing, field)))
            .unwrap_or_default();
        match saved.get(field).and_then(Value::as_str) {
            Some(current) if index::nfc(current) == index::nfc(baseline) => {
                if current != value {
                    fields.push((field.clone(), value.clone()));
                }
            }
            _ => warnings.push(format!("Field “{field}” changed in Anki; its pronunciation update was skipped.")),
        }
    }
    fields
}

fn template_mode(template: &Map<String, Value>) -> &str {
    template.get("overwriteMode").and_then(Value::as_str).unwrap_or_default()
}

/// The data URL of a dictionary file, as its base64.
fn data_url_base64(value: Option<&Value>) -> Option<String> {
    let rest = value?.as_str()?.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    let body = data.trim_end_matches('=');
    let valid = !mime.is_empty()
        && !mime.contains([';', ','])
        && data.len() - body.len() <= 2
        && body.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/');
    valid.then(|| data.to_owned())
}

impl Anki {
    /// `dictionaryMedia`: a dictionary file's bytes, from the generation the result came from.
    fn dictionary_media(&self, item: &DictionaryMedia, generation: Option<&Value>) -> Result<String> {
        let reply =
            self.engine(&json!({ "type": "hd_media", "dictionary": item.dictionary, "path": item.path, "generation": generation }))?;
        data_url_base64(reply.get("dataUrl")).ok_or_else(|| Failure::new("The dictionary image is no longer available or is malformed."))
    }

    /// `prepareWrite`: the captured screenshot, then the dictionary files and the
    /// first-field pronunciation, in Anki before the note.
    fn prepare_write(
        &self,
        invoke: &Invoke<'_>,
        request: &mut Value,
        fields: &mut Fields,
        resources: &mut Resources,
        media: &ClientMedia,
    ) -> Result<WriteResources> {
        let captured = store_screenshot(invoke, request, fields, media);
        let generation = request.get("generation").cloned();
        let audio = resources.audio.as_ref().map(|file| (file.filename.as_str(), file.data.as_str()));
        let load = |item: &DictionaryMedia| self.dictionary_media(item, generation.as_ref());
        let validate = || self.current_generation(request);
        match media::prepare(invoke, fields, &resources.media, audio, &load, &validate) {
            Ok(confirmed) => resources.confirmed = confirmed,
            Err(error) => {
                release_captured(invoke, captured.screenshot.as_deref());
                return Err(error);
            }
        }
        Ok(captured)
    }

    /// `prepareAdd`: a native add's media. A pronunciation outside the first field
    /// is resolved once and rendered into the note; it, the screenshot and the
    /// dictionary images the note references go with the add.
    fn prepare_add(
        &self,
        invoke: &Invoke<'_>,
        prepared: &Prepared,
        request: &mut Value,
        fields: &mut Fields,
        resources: &mut Resources,
        media: &ClientMedia,
    ) -> Result<WriteResources> {
        let mut written = WriteResources::default();
        let mut files: Vec<AddFile> = Vec::new();
        let config = &prepared.current.config;
        let audio_templates: Templates = prepared
            .templates()
            .iter()
            .filter(|(_, template)| {
                templates::marker_names(template.get("value").and_then(Value::as_str).unwrap_or_default())
                    .iter()
                    .any(|name| name == "audio")
            })
            .cloned()
            .collect();
        let sources = config.get("audioSources").and_then(Value::as_array).is_some_and(|sources| !sources.is_empty());
        if !audio_templates.is_empty() && sources {
            let rendered = Self::audio(request, config).and_then(|file| {
                let (built, planned) = self.render(
                    request,
                    &audio_templates,
                    &format!("[sound:{}]", file.filename),
                    Layout::of(config),
                    config.get("profile"),
                )?;
                Ok((file, built, planned))
            });
            match rendered {
                Ok((file, built, planned)) => {
                    for (field, value) in built {
                        match fields.iter_mut().find(|(name, _)| *name == field) {
                            Some((_, existing)) => *existing = value,
                            None => fields.push((field, value)),
                        }
                    }
                    resources.media.extend(planned);
                    files.push(AddFile {
                        filename: file.filename,
                        data: file.data,
                        picture: false,
                        kind: "pronunciation".to_owned(),
                        required: false,
                    });
                }
                Err(error) => written.warnings.push(format!("Pronunciation: {}", error.message)),
            }
        }
        match take_screenshot(request, fields, media) {
            Taken::None => {}
            Taken::Without(warning) => written.warnings.push(warning),
            Taken::Pending(filename, data) => {
                written.screenshot = Some(filename.clone());
                files.push(AddFile { filename, data, picture: true, kind: "Screenshot".to_owned(), required: false });
            }
        }
        let generation = request.get("generation").cloned();
        let mut attach = || -> Result<Vec<String>> {
            // One file per filename, the last planned, in the order first planned.
            let mut planned: Vec<&DictionaryMedia> = Vec::new();
            for item in &resources.media {
                match planned.iter_mut().find(|known| known.filename == item.filename) {
                    Some(known) => *known = item,
                    None => planned.push(item),
                }
            }
            for item in planned.into_iter().filter(|item| fields.iter().any(|(_, value)| value.contains(&item.filename))) {
                let data = self.dictionary_media(item, generation.as_ref())?;
                files.push(AddFile {
                    filename: item.filename.clone(),
                    data,
                    picture: true,
                    kind: "dictionary image".to_owned(),
                    required: true,
                });
            }
            self.current_generation(request)?;
            media::attach_add_media(fields, &mut written.attachments, &files, invoke)
        };
        match attach() {
            Ok(warnings) => written.warnings.extend(warnings),
            Err(error) => {
                release_captured(invoke, written.screenshot.as_deref());
                return Err(error);
            }
        }
        Ok(written)
    }

    /// The Template's configuration as it is now, as JSON (`null` once it is gone).
    fn config_json(&self, template_id: Option<&Value>) -> std::result::Result<String, String> {
        Ok(self.read_config(template_id)?.map_or_else(|| "null".to_owned(), |config| Value::Object(config).to_string()))
    }

    /// `enrichAnkiNote`: a pronunciation for the fields that want one but were
    /// written without it, added once the note is saved.
    fn enrich(
        &self,
        invoke: &Invoke<'_>,
        prepared: &Prepared,
        request: &Value,
        note_id: i64,
        applied: &Fields,
        existing: Option<&Fields>,
    ) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        let confirmed: HashSet<&String> = prepared.resources.confirmed.iter().collect();
        let canonical = match existing {
            Some(existing) => duplicates::canonical_fields(&Fields::new(), prepared.templates(), existing)?.0,
            None => prepared.templates().clone(),
        };
        let templates: Templates = canonical
            .into_iter()
            .filter(|(field, template)| {
                let value = template.get("value").and_then(Value::as_str).unwrap_or_default();
                if !templates::marker_names(value).iter().any(|name| name == "audio") {
                    return false;
                }
                existing.is_none_or(|existing| {
                    template_mode(template) != "skip"
                        && !duplicates::update_keeps_field(template, request)
                        && !(template_mode(template) == "coalesce"
                            && duplicates::field(existing, field).is_some_and(|value| !value.is_empty()))
                })
            })
            .collect();
        let config = &prepared.current.config;
        let sources = config.get("audioSources").and_then(Value::as_array).is_some_and(|sources| !sources.is_empty());
        if templates.is_empty() || (prepared.resources.audio.is_none() && !sources) {
            return Ok(warnings);
        }
        let mut run = || -> Result<()> {
            let file = match &prepared.resources.audio {
                Some(file) => file.clone(),
                None => Self::audio(request, config)?,
            };
            if !confirmed.contains(&file.filename) {
                media::ensure(invoke, &file.filename, "pronunciation", &mut || Ok(file.data.clone()), &|| Ok(()))?;
            }
            let (rendered, _) =
                self.render(request, &templates, &format!("[sound:{}]", file.filename), Layout::of(config), config.get("profile"))?;
            let incoming = match existing {
                Some(existing) => duplicates::overwrite_fields(&rendered, existing, &templates, true),
                None => rendered,
            };
            // The audio may have taken seconds: re-read just before updating.
            let saved = read_note_fields(invoke, note_id)?;
            let fields = pronunciation_fields(&incoming, &saved, applied, existing, &mut warnings);
            if !fields.is_empty() {
                let reply = invoke(
                    "updateNoteFields",
                    json!({ "note": { "id": note_id, "fields": fields_json(&fields) } }),
                    Some(WRITE_TIMEOUT_MS),
                )?;
                if !reply.is_null() {
                    return Err(Failure::new("Anki returned an invalid pronunciation-update acknowledgement. Inspect the saved note."));
                }
                verify_fields(invoke, note_id, &fields)?;
            }
            Ok(())
        };
        if let Err(error) = run() {
            warnings.push(format!("Pronunciation: {}", error.message));
        }
        Ok(warnings)
    }

    /// The reply to an add Anki refused as a duplicate: the notes it means.
    fn duplicate_reply(
        &self,
        invoke: &Invoke<'_>,
        prepared: &Prepared,
        note: &Note,
        fields: &Fields,
        expression: Option<&Value>,
    ) -> Map<String, Value> {
        let config = &prepared.current.config;
        let note_ids: Vec<i64> = if index::source(config).is_none() {
            duplicates::find_duplicates(invoke, note, &prepared.first_field, config)
                .map(|found| found.iter().map(|found| found.note_id).collect())
                .unwrap_or_default()
        } else {
            self.index.find(self, config, expression, invoke, true).map(|found| found.note_ids).unwrap_or_default()
        };
        let first = &prepared.first_field;
        let value = duplicates::field(fields, first).or_else(|| duplicates::field(&note.fields, first)).unwrap_or_default();
        let value = js::trim(value);
        let shown = if value.is_empty() { "empty".to_owned() } else { format!("“{value}”") };
        let error =
            format!("Anki already has a note in deck “{}” (note type “{}”) whose first field “{first}” is {shown}.", note.deck, note.model);
        super::object(json!({ "state": "duplicate", "noteIds": note_ids, "error": error }))
    }

    /// `write`: the note, decided again against a fresh configuration, written once.
    fn write(&self, request: &mut Value, media: &ClientMedia) -> Reply {
        let template_id = request.get("templateId").cloned();
        let mut prepared = self.prepare(request, true)?;
        let config = prepared.current.config.clone();
        let invoke = self.config_invoker(&config);
        // A native add is itself the authoritative duplicate, empty and cloze
        // check: a second check adds latency without closing a race with Anki.
        let direct = preflight::native_check(&prepared.current);
        let checked = if direct {
            Decision { state: "addable", can_add: true, ..Decision::default() }
        } else {
            self.decision(&invoke, &prepared, request).map_err(|error| error.message)?
        };
        if !checked.can_add {
            let mut reply = Map::new();
            reply.insert("state".to_owned(), json!(checked.state));
            reply.insert("error".to_owned(), json!(checked.error));
            if let Some(action) = checked.action {
                reply.insert("action".to_owned(), json!(action));
            }
            if let Some(note_ids) = checked.note_ids {
                reply.insert("noteIds".to_owned(), json!(note_ids));
            }
            return Ok(reply);
        }
        let Applied { mut fields, target, .. } =
            preflight::fields_for_decision(&prepared, &checked, request).map_err(|error| error.message)?;
        if self.config_json(template_id.as_ref())? != prepared.config_json {
            return Err(CONFIG_CHANGED.to_owned());
        }
        let mut resources = std::mem::take(&mut prepared.resources);
        let written = if direct {
            self.prepare_add(&invoke, &prepared, request, &mut fields, &mut resources, media)
        } else {
            self.prepare_write(&invoke, request, &mut fields, &mut resources, media)
        };
        prepared.resources = resources;
        let write_resources = written.map_err(|error| error.message)?;
        // Failed media can restore a field's original value: leave it untouched.
        preflight::omit_unchanged(&mut fields, target.as_ref().map(|(_, fields)| fields));
        let release = || release_captured(&invoke, write_resources.screenshot.as_deref());
        if self.config_json(template_id.as_ref())? != prepared.config_json {
            release();
            return Err(CONFIG_CHANGED.to_owned());
        }
        if let Err(error) = self.current_generation(request) {
            self.retire(template_id.as_ref());
            release();
            return Err(error.message);
        }
        let mut note = prepared.note.clone();
        if target.is_none() {
            // A new note's fields are the ones media preparation may have changed.
            note.fields.clone_from(&fields);
            note.attachments.clone_from(&write_resources.attachments);
        }
        let expression = request["term"].get("expression").or_else(|| request.get("expression")).filter(|value| !value.is_null()).cloned();
        let written = write_note(&invoke, &note, target.as_ref().map(|(id, _)| *id), &fields);
        if written.is_err() {
            self.retire(template_id.as_ref());
        }
        let note_id = match written {
            Ok(note_id) => note_id,
            Err(error) if duplicates::is_duplicate_error(&error.message) => {
                release();
                return Ok(self.duplicate_reply(&invoke, &prepared, &note, &fields, expression.as_ref()));
            }
            // Nothing reached Anki, or AnkiConnect refused before writing because an
            // open editor shows the note: a definite failure the reader may retry.
            Err(error) if error.undispatched() || error.editor_conflict() => {
                release();
                return Err(error.message);
            }
            Err(error) => {
                // A lost acknowledgement may follow a completed write; nothing retries it.
                let error = format!("The write could not be confirmed. Check Anki before trying again. {}", error.message);
                return Ok(super::object(json!({ "state": "uncertain", "error": error })));
            }
        };
        let mut warnings = write_resources.warnings.clone();
        self.index.record_write(self, &config, expression.as_ref(), note_id, checked.mature);
        // A native add has already accepted the whole note, its media included:
        // there is no deferred update that needs a readback.
        if direct {
            return Ok(super::object(json!({ "state": "added", "noteId": note_id, "warnings": warnings })));
        }
        // A field an add-on rewrote is reported; only a first field Anki did not
        // save as submitted leaves the note unverified, and unenriched.
        let verified = match read_note_fields(&invoke, note_id) {
            Ok(saved) => match field_differences(&saved, note_id, &fields) {
                Some((changed, message)) => {
                    warnings.push(message);
                    !changed.contains(&prepared.first_field)
                }
                None => true,
            },
            Err(error) => {
                warnings.push(error.message);
                false
            }
        };
        if verified {
            match self.enrich(&invoke, &prepared, request, note_id, &fields, target.as_ref().map(|(_, fields)| fields)) {
                Ok(more) => warnings.extend(more),
                Err(error) => warnings.push(error.message),
            }
        }
        Ok(super::object(json!({ "state": if target.is_some() { "updated" } else { "added" }, "noteId": note_id, "warnings": warnings })))
    }

    /// `submitClient`: a linked browser's note, with the media it sent, written in turn.
    pub(super) fn submit(&self, request: &Value, client_media: &Value) -> Reply {
        let media = media::validate_client_media(request, client_media)?;
        let _queue = self.mutations.lock().unwrap_or_else(PoisonError::into_inner);
        let mut request = request.clone();
        self.write(&mut request, &media)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_empty_refusals() {
        let note = Note {
            deck: "Mining".to_owned(),
            model: "Kiku".to_owned(),
            fields: vec![("Expression".to_owned(), "<br>".to_owned())],
            options: Map::new(),
            tags: Vec::new(),
            attachments: Map::new(),
        };
        let error = add_note_context(Failure::new("Anki refused (AnkiConnect: cannot create note because it is empty)"), &note);
        assert_eq!(
            error.message,
            "Anki refused the note for deck “Mining”, note type “Kiku” because its first field “Expression” is empty once Anki stripped its \
             formatting. (AnkiConnect: cannot create note because it is empty)"
        );
        assert_eq!(add_note_context(Failure::new("other"), &note).message, "other");
    }

    #[test]
    fn names_saved_differences() {
        let saved = serde_json::from_value(json!({ "A": "x", "B": "ｙ" })).unwrap();
        let expected = vec![("A".to_owned(), "x".to_owned()), ("B".to_owned(), "y".to_owned()), ("C".to_owned(), String::new())];
        let (fields, message) = field_differences(&saved, 7, &expected).unwrap();
        assert_eq!(fields, ["C", "B"]);
        assert_eq!(
            message,
            "Anki's saved note differs from the submitted values: field “C” is missing from note 7; field “B” was saved with different content. \
             Inspect note 7 in Anki."
        );
        assert_eq!(data_url_base64(Some(&json!("data:image/png;base64,QUJD"))), Some("QUJD".to_owned()));
        assert_eq!(data_url_base64(Some(&json!("data:;base64,QUJD"))), None);
    }
}
