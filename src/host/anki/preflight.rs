// SPDX-License-Identifier: GPL-3.0-or-later
//! `hd_anki_preflight`: whether the result can be added, and how, from the note
//! its Templates make. The extension's `prepare`, `decision` and `preflightReply`
//! (`anki-mining.js`), with the worker's `buildFields` (`anki-worker.js`) and the
//! offscreen document's `buildAnkiResourceFields` (`anki-resources.js`).

use std::cell::RefCell;
use std::collections::HashMap;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::audio::{self, AudioFile};
use super::duplicates::{self, Fields, Note, Templates};
use super::gateway::{Failure, Result};
use super::glossary::{Layout, Renderer};
use super::language::NoteLanguage;
use super::media::DictionaryMedia;
use super::{Anki, CONFIG_CHANGED, Configured, Invoke, OPTIONS_KEY, index, split_code, templates, values};
use crate::host::profile::JAPANESE;
use crate::host::settings::hex;
use crate::host::settings::{profile_worker, profiles};
use crate::host::{js, store};

const IMAGE_EXTENSIONS: [&str; 10] = ["png", "jpg", "jpeg", "gif", "webp", "svg", "avif", "bmp", "ico", "tiff"];
const SENTENCE_FURIGANA_MARKERS: [&str; 2] = ["sentence-furigana", "sentence-furigana-plain"];
const NO_TARGET: &str = "A duplicate exists, but no matching configured note type is inside the selected scope.";

/// What preparing a note planned beyond its fields: the dictionary files they
/// reference, and the first field's pronunciation.
#[derive(Debug, Default)]
pub(super) struct Resources {
    pub(super) media: Vec<DictionaryMedia>,
    /// `audioPrepared`: the pronunciation, downloaded.
    pub(super) audio: Option<AudioFile>,
    /// `confirmedMedia`: the files already in Anki.
    pub(super) confirmed: Vec<String>,
}

/// A request, prepared: its configuration and the note its Templates make.
pub(super) struct Prepared {
    pub(super) current: Configured,
    /// The configuration as it was read, to notice a change before the write.
    pub(super) config_json: String,
    pub(super) note: Note,
    pub(super) first_field: String,
    pub(super) resources: Resources,
}

impl Prepared {
    pub(super) fn templates(&self) -> &Templates {
        static NONE: Templates = Vec::new();
        self.current.resolved.as_ref().map_or(&NONE, |resolved| &resolved.templates)
    }
}

/// An existing note an overwrite updates: its ID and fields.
pub(super) type Target = (i64, Fields);

/// `decision`'s answer.
#[derive(Debug, Default)]
pub(super) struct Decision {
    pub(super) state: &'static str,
    pub(super) can_add: bool,
    pub(super) error: Option<String>,
    pub(super) action: Option<&'static str>,
    pub(super) note_ids: Option<Vec<i64>>,
    pub(super) target: Option<Target>,
    pub(super) mature: bool,
}

/// `fieldsForDecision`'s answer: what a write applies, and to which note.
pub(super) struct Applied {
    pub(super) fields: Fields,
    pub(super) target: Option<Target>,
}

/// `omitUnchangedFields`.
pub(super) fn omit_unchanged(fields: &mut Fields, existing: Option<&Fields>) {
    if let Some(existing) = existing {
        fields.retain(|(name, value)| duplicates::field(existing, name) != Some(value.as_str()));
    }
}

/// `fieldsForDecision`: a new note's fields, or an overwrite's under the target's
/// own field names, combined by each Template's overwrite mode.
pub(super) fn fields_for_decision(prepared: &Prepared, decision: &Decision, request: &Value) -> Result<Applied> {
    let Some(target) = &decision.target else {
        return Ok(Applied { fields: prepared.note.fields.clone(), target: None });
    };
    let (templates, incoming) = duplicates::canonical_fields(&prepared.note.fields, prepared.templates(), &target.1)?;
    let templates: Templates = templates.into_iter().filter(|(_, template)| !duplicates::update_keeps_field(template, request)).collect();
    let mut fields = duplicates::overwrite_fields(&incoming, &target.1, &templates, false);
    omit_unchanged(&mut fields, Some(&target.1));
    Ok(Applied { fields, target: Some(target.clone()) })
}

/// A target's fields as `Fields`.
fn target_fields(fields: &Map<String, Value>) -> Fields {
    fields.iter().map(|(name, value)| (name.clone(), js::to_string(Some(value)))).collect()
}

/// `ankiMediaFilename(JSON.stringify([generationPath, path]))`: a dictionary image's
/// Anki filename, the same for every preparation of the same generation.
fn media_filename(generation_path: &str, path: &str) -> String {
    let suffix = path.rsplit('.').next().unwrap_or_default().to_lowercase();
    let extension = if IMAGE_EXTENSIONS.contains(&suffix.as_str()) { suffix.as_str() } else { "bin" };
    let identity = Value::Array(vec![json!(generation_path), json!(path)]).to_string();
    format!("hachidori_{}.{extension}", hex(&Sha256::digest(identity.as_bytes())))
}

/// `buildFields`' check of the result it is given.
fn valid_result(request: &Value) -> bool {
    let generation = js::safe_integer(request.get("generation")).is_some_and(|generation| generation >= 0.0)
        && request.get("generation").is_some_and(Value::is_number);
    let term = &request["term"];
    generation
        && term.get("expression").and_then(Value::as_str).is_some_and(|text| !text.is_empty())
        && term.get("reading").is_some_and(Value::is_string)
}

fn checked_decision(invoke: &Invoke<'_>, prepared: &Prepared, addable: bool, error: Option<String>) -> Decision {
    let templates = prepared.current.resolved.as_ref().map(|resolved| resolved.templates.clone()).unwrap_or_default();
    Decision {
        state: if addable { "addable" } else { "invalid" },
        can_add: addable,
        error: error.map(|error| duplicates::explain_refusal(invoke, &prepared.note, &error, &templates)),
        ..Decision::default()
    }
}

fn addable_decision(invoke: &Invoke<'_>, prepared: &Prepared) -> Result<Decision> {
    let (addable, error) = duplicates::validate_note(invoke, &prepared.note)?;
    Ok(checked_decision(invoke, prepared, addable, error))
}

/// `nativeDuplicateCheck`: Anki's own first-field duplicate guard is enough for
/// this Template. Overwrite, and expression fields elsewhere in a custom model,
/// still need the word index and exact target fields; cross-model scopes keep
/// their policy.
pub(super) fn native_check(current: &Configured) -> bool {
    let text = |key: &str| current.config.get(key).and_then(Value::as_str);
    let first = current.discovery.as_ref().and_then(|discovery| discovery.fields.first());
    let template = first.and_then(|field| current.resolved.as_ref().map(|resolved| resolved.value(field))).unwrap_or_default();
    text("duplicateScope") == Some("model")
        && text("duplicateBehavior") != Some("overwrite")
        && template.eq_ignore_ascii_case("{expression}")
}

/// `nativeDecision`: a duplicate as Anki's add check reports it, without the index.
fn native_decision(invoke: &Invoke<'_>, prepared: &Prepared) -> Result<Decision> {
    let checked = duplicates::check_duplicate(invoke, &prepared.note)?;
    if checked.duplicate {
        return Ok(duplicate(behavior(prepared) == "new", Vec::new(), false));
    }
    Ok(checked_decision(invoke, prepared, checked.addable, checked.error))
}

fn behavior(prepared: &Prepared) -> &str {
    prepared.current.config.get("duplicateBehavior").and_then(Value::as_str).unwrap_or_default()
}

fn duplicate(can_add: bool, note_ids: Vec<i64>, mature: bool) -> Decision {
    Decision { state: "duplicate", can_add, note_ids: Some(note_ids), mature, ..Decision::default() }
}

fn overwrite(target: Option<Target>, note_ids: Vec<i64>, mature: bool) -> Decision {
    let found = target.is_some();
    Decision { action: Some("overwrite"), error: (!found).then(|| NO_TARGET.to_owned()), target, ..duplicate(found, note_ids, mature) }
}

/// `unindexedDecision`: Anki's own first-field duplicate rule.
fn unindexed_decision(invoke: &Invoke<'_>, prepared: &Prepared) -> Result<Decision> {
    let checked = duplicates::check_duplicate(invoke, &prepared.note)?;
    if !checked.duplicate {
        return Ok(checked_decision(invoke, prepared, checked.addable, checked.error));
    }
    let matches = duplicates::find_duplicates(invoke, &prepared.note, &prepared.first_field, &prepared.current.config)?;
    let note_ids: Vec<i64> = matches.iter().map(|found| found.note_id).collect();
    match behavior(prepared) {
        "overwrite" => return Ok(overwrite(matches.into_iter().next().map(|found| (found.note_id, found.fields)), note_ids, false)),
        "new" => {
            let addable = addable_decision(invoke, prepared)?;
            if !addable.can_add {
                return Ok(addable);
            }
        }
        _ => {}
    }
    Ok(duplicate(behavior(prepared) == "new", note_ids, false))
}

impl Anki {
    /// `decision`.
    pub(super) fn decision(&self, invoke: &Invoke<'_>, prepared: &Prepared, request: &Value) -> Result<Decision> {
        if request.get("allowDuplicate") == Some(&Value::Bool(true)) {
            return Ok(Decision { state: "addable", can_add: true, ..Decision::default() });
        }
        if native_check(&prepared.current) {
            return native_decision(invoke, prepared);
        }
        let config = &prepared.current.config;
        let expression = request["term"].get("expression").or_else(|| request.get("expression")).filter(|value| !value.is_null());
        let Some(source) = index::source(config) else { return unindexed_decision(invoke, prepared) };
        let mut found = self.index.find(self, config, expression, invoke, false)?;
        if !found.note_ids.is_empty() && behavior(prepared) == "overwrite" {
            let mut inspected = index::inspect(invoke, &source, expression, &found.note_ids)?;
            if inspected.stale {
                found = self.index.find(self, config, expression, invoke, true)?;
                if found.note_ids.is_empty() {
                    return addable_decision(invoke, prepared);
                }
                inspected = index::inspect(invoke, &source, expression, &found.note_ids)?;
            }
            let target = inspected.target.map(|(id, fields)| (id, target_fields(&fields)));
            return Ok(overwrite(target, found.note_ids, found.mature));
        }
        // `indexedDecision`, with Anki's add check where the policy needs one.
        if found.note_ids.is_empty() || behavior(prepared) == "new" {
            let (addable, error) = duplicates::validate_note(invoke, &prepared.note)?;
            let checked = checked_decision(invoke, prepared, addable, error);
            if found.note_ids.is_empty() || !checked.can_add {
                return Ok(checked);
            }
        }
        Ok(duplicate(behavior(prepared) == "new", found.note_ids, found.mature))
    }

    /// One engine request, its reply's error as a failure.
    pub(super) fn engine(&self, message: &Value) -> Result<Value> {
        let (options, state) = (self.publisher.read(OPTIONS_KEY), self.publisher.read(store::DICTIONARY_STATE));
        let message =
            profile_worker::engine_request(message, options.as_ref(), state.as_ref()).map_err(|refused| Failure::new(refused.error))?;
        let reply = self.engine.handle(&message);
        if reply.get("ok") == Some(&Value::Bool(true)) {
            Ok(reply)
        } else {
            Err(Failure::new(js::to_string(
                reply.get("error").filter(|error| js::truthy(Some(error))).or(Some(&json!("Anki preparation did not complete."))),
            )))
        }
    }

    /// `currentGeneration`.
    pub(super) fn current_generation(&self, request: &Value) -> Result<()> {
        let status = self.engine.handle(&json!({ "type": "hd_status" }));
        if status.get("ready") != Some(&Value::Bool(true))
            || js::truthy(status.get("loading"))
            || !js::strict_eq(status.get("generation"), request.get("generation"))
        {
            return Err(Failure::new("The dictionary generation changed or is being updated. Look up this result again before adding it."));
        }
        Ok(())
    }

    /// `readDictionaries`, as `dictionaryPaths`: each enabled dictionary's
    /// generation path, in the dictionaries of the request's profile.
    fn dictionary_paths(&self, request: &Value) -> HashMap<String, String> {
        let profile_id = request.get("profileId").and_then(Value::as_str).unwrap_or(JAPANESE);
        let state = self.publisher.read(store::DICTIONARY_STATE);
        let state = profiles::project_dictionary_state(state.as_ref(), profile_id).unwrap_or(Value::Null);
        state
            .get("dictionaries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| item.get("enabled") != Some(&Value::Bool(false)))
            .map(|item| (js::to_string(item.get("title")), js::to_string(item.get("path"))))
            .collect()
    }

    /// `sentenceSegments`: the words around the match, split as a hover splits them.
    fn sentence_segments(&self, request: &Value, templates: &[(String, Map<String, Value>)]) -> Result<Option<Value>> {
        let wanted = templates.iter().any(|(_, template)| {
            templates::marker_names(template.get("value").and_then(Value::as_str).unwrap_or_default())
                .iter()
                .any(|name| SENTENCE_FURIGANA_MARKERS.contains(&name.as_str()))
        });
        if !wanted {
            return Ok(None);
        }
        // A profile's sentence is split in its language, and not at all in one
        // without ruby.
        let profile_id = request.get("profileId").and_then(Value::as_str);
        let (options, profile) = self.options_and_profile(profile_id).map_err(|error| Failure::new(split_code(&error).0))?;
        if let Some(profile) = &profile {
            let pack_settings = profile.get("packSettings").and_then(Value::as_object).cloned().unwrap_or_default();
            if !profiles::option_requirement_met(&Value::from("ruby"), profile["language"].as_str(), &pack_settings) {
                return Ok(None);
            }
        }
        let sentence = js::to_string(request.get("sentence"));
        let offset = js::to_number(request.get("matchOffset"));
        #[expect(clippy::cast_precision_loss, reason = "a UTF-16 length")]
        let end = offset + js::length(request.get("matched").and_then(Value::as_str).unwrap_or_default()) as f64;
        let mut segment = json!({
            "type": "hd_segment",
            "chunks": [{ "id": 0, "text": js::slice(&sentence, 0.0, Some(offset)) }, { "id": 1, "text": js::slice(&sentence, end, None) }],
            "scanLength": options.get("scanLength"),
            "options": {
                "frequencyDictionary": options.get("frequencyDictionary"),
                "frequencyOrder": options.get("frequencyOrder"),
                "personalDictionary": options.get("personalDictionaryEnabled"),
            },
        });
        if let Some(id) = profile_id {
            segment["profileId"] = Value::from(id);
        }
        let reply = self.engine(&segment)?;
        let segments = reply.get("segments").and_then(Value::as_array).ok_or_else(|| Failure::new("hd_segment returned no segments."))?;
        Ok(Some(Value::Array(segments.iter().map(|segment| segment.get("spans").cloned().unwrap_or(Value::Null)).collect())))
    }

    /// `audio`: the pronunciation the enabled sources offer for the result, in
    /// the language of the profile the note is mined in.
    pub(super) fn audio(request: &Value, config: &Map<String, Value>) -> Result<AudioFile> {
        let sources = config.get("audioSources").and_then(Value::as_array).cloned().unwrap_or_default();
        let language = audio::Pronunciation::of(config.get("profile").and_then(|profile| profile["language"].as_str()));
        audio::export(&sources, &request["term"], request.get("audioSelection"), language)
    }

    /// `buildFields`: the fields the request's Templates make, and what they need.
    pub(super) fn build_fields(&self, request: &Value, current: &Configured, preflight: bool) -> Result<(Fields, Resources)> {
        if !valid_result(request) {
            return Err(Failure::new("Mining requires a current dictionary result."));
        }
        let (Some(discovery), Some(resolved)) = (&current.discovery, &current.resolved) else {
            return Err(Failure::new("Choose an Anki note type in Settings."));
        };
        let first = discovery.fields.first().map(|field| resolved.value(field)).unwrap_or_default();
        if preflight && resolved.templates.len() == 1 && first.eq_ignore_ascii_case("{expression}") {
            // No dictionary reads or engine status just to ask Anki about a word
            // this lookup already supplied.
            let mut definition = |_: &super::glossary::Wanted| Ok(String::new());
            return Ok((values::build_fields(request, &resolved.templates, &mut definition, "", None)?, Resources::default()));
        }
        self.current_generation(request)?;
        let sources = current.config.get("audioSources").and_then(Value::as_array).is_some_and(|sources| !sources.is_empty());
        let mut resources = Resources::default();
        // Audio in the first field is part of Anki's duplicate identity, so it is
        // downloaded first, for preflight as for the write.
        if sources && templates::marker_names(first).iter().any(|name| name == "audio") {
            resources.audio = Some(Self::audio(request, &current.config)?);
        }
        let pronunciation = resources.audio.as_ref().map(|file| format!("[sound:{}]", file.filename)).unwrap_or_default();
        let (fields, media) =
            self.render(request, &resolved.templates, &pronunciation, Layout::of(&current.config), current.config.get("profile"))?;
        resources.media = media;
        Ok((fields, resources))
    }

    /// `render` (`hd_anki_fields`): `templates`' fields for the request, and the
    /// dictionary files they reference.
    pub(super) fn render(
        &self,
        request: &Value,
        templates: &Templates,
        audio: &str,
        layout: Layout,
        profile: Option<&Value>,
    ) -> Result<(Fields, Vec<DictionaryMedia>)> {
        let segments = self.sentence_segments(request, templates)?;
        let mut request = request.clone();
        if let Some(segments) = segments {
            request["sentenceSegments"] = segments;
        }
        self.render_plain(&request, templates, audio, layout, profile)
    }

    /// The offscreen document's `hd_anki_fields`: the fields for a request as
    /// it is, in the language of `profile` (`{language, packSettings}`) once
    /// the records have profiles.
    pub(super) fn render_plain(
        &self,
        request: &Value,
        templates: &Templates,
        audio: &str,
        layout: Layout,
        profile: Option<&Value>,
    ) -> Result<(Fields, Vec<DictionaryMedia>)> {
        let language = NoteLanguage::of(profile, request);
        let paths = self.dictionary_paths(request);
        let generation = request.get("generation").cloned();
        let styles =
            Box::new(move || {
                let reply = self.engine.handle(&json!({ "type": "hd_styles" }));
                if reply.get("ok") != Some(&Value::Bool(true)) || !js::strict_eq(reply.get("generation"), generation.as_ref()) {
                    let error = reply.get("error").filter(|error| js::truthy(Some(error)));
                    return Err(Failure::new(error.map_or_else(
                        || "Dictionary styles changed during Anki preparation.".to_owned(),
                        |error| js::to_string(Some(error)),
                    )));
                }
                Ok(reply.get("styles").and_then(Value::as_array).cloned().unwrap_or_default())
            });
        let planned: RefCell<Vec<DictionaryMedia>> = RefCell::new(Vec::new());
        let filename_for = Box::new(|dictionary: &str, path: &str| match paths.get(dictionary).filter(|path| !path.is_empty()) {
            Some(generation_path) => {
                let filename = media_filename(generation_path, path);
                let mut planned = planned.borrow_mut();
                if !planned.iter().any(|item| item.dictionary == dictionary && item.path == path) {
                    planned.push(DictionaryMedia { dictionary: dictionary.to_owned(), path: path.to_owned(), filename: filename.clone() });
                }
                Ok(filename)
            }
            None => Err(Failure::new(format!("The dictionary generation is no longer available: {dictionary}"))),
        });
        let fields = {
            let mut renderer = Renderer::new(request, layout, styles, filename_for).in_language(language.as_ref());
            let mut definition = |wanted: &_| renderer.render(wanted);
            values::build_fields(request, templates, &mut definition, audio, language.as_ref())?
        };
        Ok((fields, planned.into_inner()))
    }

    /// `prepare`: the request's note, from a configuration that still matches it.
    pub(super) fn prepare(&self, request: &Value, fresh: bool) -> std::result::Result<Prepared, String> {
        let current = self.configuration(request.get("templateId"))?;
        if request.get("configKey").and_then(Value::as_str) != Some(current.config_key.as_str()) {
            return Err(CONFIG_CHANGED.to_owned());
        }
        if !current.errors.is_empty() {
            return Err(current.errors.join("\n"));
        }
        let config_json = Value::Object(current.config.clone()).to_string();
        // Only authored cloze templates besides the first field can affect Anki's
        // validity check, so a native add check renders just those: glossaries and
        // sentence furigana are not needed to enable Add.
        let built = if !fresh && native_check(&current) {
            let mut rendering = current.clone();
            if let (Some(resolved), Some(first)) = (&mut rendering.resolved, current.discovery.as_ref().and_then(|d| d.fields.first())) {
                resolved.templates.retain(|(field, template)| {
                    field == first
                        || template.get("value").and_then(Value::as_str).is_some_and(|value| value.to_lowercase().contains("{{c"))
                });
            }
            self.build_fields(request, &rendering, true)
        } else {
            self.build_fields(request, &current, !fresh)
        };
        let (fields, resources) = built.map_err(|error| error.message)?;
        let first_field = current.discovery.as_ref().and_then(|discovery| discovery.fields.first()).cloned().unwrap_or_default();
        let config = &current.config;
        let model = config.get("model").and_then(Value::as_str).unwrap_or_default();
        if js::trim(duplicates::field(&fields, &first_field).unwrap_or_default()).is_empty() {
            let template = current.resolved.as_ref().map(|resolved| resolved.value(&first_field)).unwrap_or_default();
            let expression = request["term"].get("expression").or_else(|| request.get("expression"));
            let term = match expression.and_then(Value::as_str) {
                Some(text) if !js::trim(text).is_empty() => format!("“{text}”"),
                _ => "this result".to_owned(),
            };
            let detail = if template.is_empty() { String::new() } else { format!(": its template {template} produced nothing for {term}") };
            return Err(format!(
                "The first field of note type “{model}”, “{first_field}”, is empty for this result{detail}. Anki requires it."
            ));
        }
        let mut options = duplicates::note_options(config);
        if request.get("allowDuplicate") == Some(&Value::Bool(true)) {
            options.insert("allowDuplicate".to_owned(), json!(true));
        }
        let mut tags: Vec<String> = Vec::new();
        for tag in config.get("tags").and_then(Value::as_array).into_iter().flatten() {
            let tag = js::to_string(Some(tag));
            if !tags.contains(&tag) {
                tags.push(tag);
            }
        }
        let note =
            Note { deck: js::to_string(config.get("deck")), model: model.to_owned(), fields, options, tags, attachments: Map::new() };
        Ok(Prepared { current, config_json, note, first_field, resources })
    }

    /// `preflight`, for a linked browser: the decision and what the reader captures.
    pub(super) fn preflight(&self, request: &Value) -> std::result::Result<Map<String, Value>, String> {
        let reply = self.preflight_reply(request);
        // A failure, or a note Anki refuses, retires the configuration for a retry.
        if reply.as_ref().is_err() || reply.as_ref().is_ok_and(|reply| reply.get("state") == Some(&json!("invalid"))) {
            self.retire(request.get("templateId"));
        }
        reply
    }

    fn preflight_reply(&self, request: &Value) -> std::result::Result<Map<String, Value>, String> {
        let prepared = self.prepare(request, false)?;
        let invoke = self.config_invoker(&prepared.current.config);
        let decision = self.decision(&invoke, &prepared, request).map_err(|error| error.message)?;
        // `preflightReply` applies the decision as the write would, which can refuse it.
        if decision.can_add {
            fields_for_decision(&prepared, &decision, request).map_err(|error| error.message)?;
        }
        let templates = prepared.templates();
        let capture = templates::capture_requirements(
            templates.iter().map(|(_, template)| template.get("value").and_then(Value::as_str).unwrap_or_default()),
        );
        let mut reply = Map::new();
        reply.insert("state".to_owned(), json!(decision.state));
        reply.insert("canAdd".to_owned(), json!(decision.can_add));
        reply.insert("error".to_owned(), json!(decision.error));
        if let Some(action) = decision.action {
            reply.insert("action".to_owned(), json!(action));
        }
        if let Some(note_ids) = decision.note_ids {
            reply.insert("noteIds".to_owned(), json!(note_ids));
        }
        let screenshot = prepared.current.config.get("captureScreenshot") == Some(&Value::Bool(true)) && capture.screenshot;
        reply.insert("screenshot".to_owned(), json!(screenshot));
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_media_by_generation_path() {
        let name = media_filename("dictionaries/abc", "img/a.PNG");
        assert!(name.starts_with("hachidori_") && name.rsplit('.').next() == Some("png"));
        assert_eq!(name.len(), "hachidori_".len() + 64 + 4);
        assert_eq!(media_filename("d", "noext").rsplit('.').next(), Some("bin"));
        assert!(valid_result(&json!({ "generation": 0, "term": { "expression": "a", "reading": "" } })));
        assert!(!valid_result(&json!({ "generation": 1.5, "term": { "expression": "a", "reading": "" } })));
        assert!(!valid_result(&json!({ "generation": 1, "term": { "expression": "", "reading": "" } })));
    }
}
