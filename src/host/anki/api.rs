// SPDX-License-Identifier: GPL-3.0-or-later
//! What the Yomitan-compatible API asks of Anki mining (`api-host.js`): a
//! result's fields for markers, its pronunciation, and the Templates as
//! Yomitan's card formats.

use serde_json::{Map, Value, json};

use super::glossary::Layout;
use super::{Anki, templates};

pub(crate) use super::templates::escape_html;

/// A result's fields, one per marker, and the dictionary files they show.
pub(crate) struct Rendered {
    pub(crate) fields: Vec<(String, String)>,
    /// `{dictionary, path, filename}` of each file, in the order the fields use them.
    pub(crate) media: Vec<(String, String, String)>,
}

/// `cardFormat`: Yomitan's `AnkiCardFormat` for one Anki Template. A Template has
/// no add-button icon; a legacy mapping answers the rows mining builds from it.
fn card_format(template: &Value) -> Value {
    let empty = Map::new();
    let config = template.as_object().unwrap_or(&empty);
    let fields: Vec<(String, Value)> = if let Some(saved) = config.get("fieldTemplates").filter(|value| !value.is_null()) {
        saved.as_object().map(|saved| saved.iter().map(|(name, value)| (name.clone(), value.clone())).collect()).unwrap_or_default()
    } else {
        // `ankiMappedFieldNames(config)`: the mapped fields, first spelling winning.
        let mut names: Vec<String> = Vec::new();
        for semantic in templates::ANKI_FIELDS {
            let field = config.get("fields").and_then(|fields| fields.get(semantic)).and_then(Value::as_str).unwrap_or_default();
            if !field.is_empty() && !names.iter().any(|name| name.to_lowercase() == field.to_lowercase()) {
                names.push(field.to_owned());
            }
        }
        templates::resolve(config, &names).templates.into_iter().map(|(name, template)| (name, Value::Object(template))).collect()
    };
    let fields: Map<String, Value> = fields
        .into_iter()
        .map(|(name, template)| {
            let mut row = Map::new();
            for key in ["value", "overwriteMode"] {
                if let Some(value) = template.get(key) {
                    row.insert(key.to_owned(), value.clone());
                }
            }
            (name, Value::Object(row))
        })
        .collect();
    let mut format = Map::new();
    for (key, value) in [("name", template.get("name")), ("deck", template.get("deck")), ("model", template.get("model"))] {
        if let Some(value) = value {
            format.insert(key.to_owned(), value.clone());
        }
    }
    format.insert("icon".to_owned(), json!("big-circle"));
    format.insert("fields".to_owned(), Value::Object(fields));
    format.insert("type".to_owned(), json!("term"));
    Value::Object(format)
}

impl Anki {
    /// `readSmallerAnkiCards` and `readCompactGlossaries`.
    pub(crate) fn api_layout(&self) -> Layout {
        let options = self.options();
        Layout {
            compact: options.get("smallerAnkiCards") == Some(&Value::Bool(true)),
            compact_glossaries: options.get("glossaryLayoutMode") == Some(&json!("compact-popup-anki")),
        }
    }

    /// `readProfile`: the `{language, packSettings}` of the profile an API
    /// request is for, `None` while the records have no profiles.
    pub(crate) fn api_profile(&self, profile_id: Option<&str>) -> Option<Value> {
        self.options_and_profile(profile_id).ok()?.1
    }

    /// The options of the profile an API request is for (`readAnkiOptions`).
    fn api_options(&self, profile_id: Option<&str>) -> Map<String, Value> {
        self.options_and_profile(profile_id).map_or_else(|_| self.options(), |(options, _)| options)
    }

    /// `hd_anki_audio` for the profile's enabled sources, in its language, or
    /// nothing where none plays.
    pub(crate) fn api_audio(&self, term: &Value, profile_id: Option<&str>, profile: Option<&Value>) -> Option<(String, String)> {
        let options = self.api_options(profile_id);
        let sources: Vec<Value> = options
            .get("audioSources")
            .and_then(Value::as_array)
            .map(|sources| sources.iter().filter(|source| source.get("enabled") == Some(&Value::Bool(true))).cloned().collect())
            .unwrap_or_default();
        if sources.is_empty() {
            return None;
        }
        let mut config = Map::from_iter([("audioSources".to_owned(), Value::Array(sources))]);
        if let Some(profile) = profile {
            config.insert("profile".to_owned(), profile.clone());
        }
        Self::audio(&json!({ "term": term }), &config).ok().map(|file| (file.filename, file.data))
    }

    /// `hd_anki_fields` with each marker as a Template of its own, in the
    /// language of the profile the request is for.
    pub(crate) fn api_render(
        &self,
        request: &Value,
        markers: &[String],
        audio: &str,
        layout: Layout,
        profile: Option<&Value>,
    ) -> Result<Rendered, String> {
        let mut templates: Vec<(String, Map<String, Value>)> = Vec::new();
        for marker in markers {
            if !templates.iter().any(|(name, _)| name == marker) {
                let template = json!({ "value": format!("{{{marker}}}"), "overwriteMode": "coalesce" });
                templates.push((marker.clone(), template.as_object().cloned().unwrap_or_default()));
            }
        }
        let (built, media) = self.render_plain(request, &templates, audio, layout, profile).map_err(|error| error.message)?;
        let fields = markers
            .iter()
            .map(|marker| {
                (marker.clone(), built.iter().find(|(name, _)| name == marker).map(|(_, value)| value.clone()).unwrap_or_default())
            })
            .collect();
        Ok(Rendered { fields, media: media.into_iter().map(|item| (item.dictionary, item.path, item.filename)).collect() })
    }

    /// `hd_media`: a dictionary file as its media type and base64, if it is still there.
    pub(crate) fn api_media(&self, dictionary: &str, path: &str, generation: Option<&Value>) -> Option<(String, String)> {
        let reply = self.engine(&json!({ "type": "hd_media", "dictionary": dictionary, "path": path, "generation": generation })).ok()?;
        let rest = reply.get("dataUrl")?.as_str()?.strip_prefix("data:")?;
        let (mime, data) = rest.split_once(";base64,")?;
        let body = data.trim_end_matches('=');
        let valid = !mime.is_empty()
            && !mime.contains([';', ','])
            && data.len() - body.len() <= 2
            && body.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/');
        valid.then(|| (mime.to_owned(), data.to_owned()))
    }

    /// `hd_api_anki_card_formats`: the Templates of the profile the request is
    /// for; Yomitan's profile 0 is Japanese's.
    pub(crate) fn api_card_formats(&self, profile_id: Option<&str>) -> Vec<Value> {
        let options = self.api_options(profile_id);
        let templates = options.get("anki").and_then(|anki| anki.get("templates")).and_then(Value::as_array).cloned().unwrap_or_default();
        templates.iter().map(card_format).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_card_formats() {
        let saved = json!({ "name": "Main", "deck": "Mining", "model": "Kiku", "fieldTemplates": {
            "Expression": { "value": "{expression}", "overwriteMode": "skip", "extra": 1 } } });
        assert_eq!(
            card_format(&saved),
            json!({ "name": "Main", "icon": "big-circle", "deck": "Mining", "model": "Kiku", "type": "term",
                "fields": { "Expression": { "value": "{expression}", "overwriteMode": "skip" } } })
        );
        let legacy = json!({ "name": "Old", "deck": "D", "model": "M", "fieldTemplates": null,
            "fields": { "expression": "Front", "reading": "front", "definition": "Back" } });
        let fields = &card_format(&legacy)["fields"];
        // Two semantics mapped to one field (case-insensitively) share it.
        assert_eq!(fields["Front"]["value"], json!("{expression}<br>{reading}"));
        assert_eq!(fields["Back"]["value"], json!("{definition}"));
        assert!(fields.get("front").is_none());
    }
}
