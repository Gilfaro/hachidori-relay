// SPDX-License-Identifier: GPL-3.0-or-later
//! Anki field Templates (`anki-templates.js`): the `{marker}` language, how a
//! Template's mapping resolves against a note type's fields, and the Kiku, Lapis and
//! Senren presets.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::host::js;

/// `ANKI_FIELDS`: the legacy mapping's semantic fields, in order.
pub(crate) const ANKI_FIELDS: [&str; 8] = ["expression", "reading", "definition", "sentence", "frequency", "pitch", "audio", "screenshot"];

const CORE_MARKERS: &[&str] = &[
    "expression",
    "reading",
    "character",
    "onyomi",
    "kunyomi",
    "stroke-count",
    "furigana",
    "furigana-plain",
    "dictionary",
    "dictionary-alias",
    "definition",
    "glossary",
    "glossary-brief",
    "glossary-no-dictionary",
    "glossary-plain",
    "glossary-plain-no-dictionary",
    "glossary-first",
    "glossary-first-brief",
    "glossary-first-no-dictionary",
    "main-definition",
    "jpmn-primary-definition",
    "conjugation",
    "part-of-speech",
    "phonetic-transcriptions",
    "tags",
    "popup-selection-text",
    "search-query",
    "document-title",
    "url",
    "url-plain",
    "sentence",
    "sentence-furigana",
    "sentence-furigana-plain",
    "cloze-prefix",
    "cloze-body",
    "cloze-suffix",
    "frequency",
    "frequencies",
    "frequency-harmonic-rank",
    "frequency-harmonic-occurrence",
    "frequency-average-rank",
    "frequency-average-occurrence",
    "pitch",
    "pitch-position",
    "pitch-accent-positions",
    "pitch-categories",
    "pitch-accent-categories",
    "pitch-accent-graphs",
    "pitch-accent-graphs-jj",
    "audio",
    "screenshot",
    "sentence-audio",
    "gif",
    // `MARKER_ALIASES`' names count as markers too.
    "pitch-accent",
    "pitch-accents",
];
const DYNAMIC_PREFIXES: [&str; 2] = ["single-glossary-", "single-frequency-"];

/// `MARKER_ALIASES`.
fn alias(name: &str) -> Option<&'static str> {
    matches!(name, "pitch-accent" | "pitch-accents").then_some("pitch")
}

/// `knownMarker`.
fn known_marker(name: &str) -> bool {
    CORE_MARKERS.contains(&name) || DYNAMIC_PREFIXES.iter().any(|prefix| name.starts_with(prefix) && name.len() > prefix.len())
}

/// `MARKER_PATTERN` (`/\{([^{}]+)\}/gu`): each match's byte range and its name.
fn markers(template: &str) -> Vec<(usize, usize, &str)> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(open) = template[from..].find('{').map(|at| from + at) {
        let rest = &template[open + 1..];
        match rest.find(['{', '}']) {
            Some(at) if at > 0 && rest.as_bytes()[at] == b'}' => {
                found.push((open, open + 1 + at + 1, &rest[..at]));
                from = open + 1 + at + 1;
            }
            _ => from = open + 1,
        }
    }
    found
}

/// `ankiTemplateMarkerNames`: each marker's name, lower-cased, aliases resolved.
pub(crate) fn marker_names(template: &str) -> Vec<String> {
    markers(template)
        .into_iter()
        .map(|(_, _, name)| {
            let name = name.to_lowercase();
            alias(&name).map_or(name, str::to_owned)
        })
        .collect()
}

/// `ankiCaptureRequirements`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Capture {
    pub(crate) screenshot: bool,
    pub(crate) sentence_audio: bool,
    pub(crate) gif: bool,
}

pub(crate) fn capture_requirements<'a>(templates: impl IntoIterator<Item = &'a str>) -> Capture {
    let names: Vec<String> = templates.into_iter().flat_map(marker_names).collect();
    let has = |name: &str| names.iter().any(|marker| marker == name);
    Capture { screenshot: has("screenshot") || has("gif"), sentence_audio: has("sentence-audio"), gif: has("gif") }
}

/// `ankiTemplateErrors`.
pub(crate) fn template_errors(template: &str) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();
    for (start, end, name) in markers(template) {
        if !known_marker(&name.to_lowercase()) {
            let error = format!("Unknown marker: {}", &template[start..end]);
            if !errors.contains(&error) {
                errors.push(error);
            }
        }
    }
    errors
}

/// `BREAK_PATTERN` (`/<br\s*\/?>/giu`): the byte ranges of each line break tag.
fn breaks(template: &str) -> Vec<(usize, usize)> {
    let bytes = template.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at + 3 <= bytes.len() {
        if bytes[at] == b'<' && bytes[at + 1].eq_ignore_ascii_case(&b'b') && bytes[at + 2].eq_ignore_ascii_case(&b'r') {
            let rest = &template[at + 3..];
            let spaced = rest.trim_start_matches(js::is_space);
            let after = spaced.strip_prefix('/').unwrap_or(spaced);
            if let Some(after) = after.strip_prefix('>') {
                let end = template.len() - after.len();
                found.push((at, end));
                at = end;
                continue;
            }
        }
        at += 1;
    }
    found
}

/// `template.split(BREAK_PATTERN)`.
fn split_breaks(template: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut from = 0;
    for (start, end) in breaks(template) {
        segments.push(&template[from..start]);
        from = end;
    }
    segments.push(&template[from..]);
    segments
}

/// `isAnkiAudioOnlyTemplate`: `{audio}` and line breaks, nothing else.
pub(crate) fn is_audio_only(template: &str) -> bool {
    let mut has_audio = false;
    let mut rest = String::new();
    let mut from = 0;
    for (start, end, name) in markers(template) {
        rest.push_str(&template[from..start]);
        if name.to_lowercase() == "audio" {
            has_audio = true;
        } else {
            rest.push_str(&template[start..end]);
        }
        from = end;
    }
    rest.push_str(&template[from..]);
    has_audio && js::trim(&split_breaks(&rest).concat()).is_empty()
}

/// `escapeAnkiHtml`.
pub(crate) fn escape_html(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#x27;")
}

/// `STYLE_ELEMENT` (`/(<style\b[^>]*>[\s\S]*?<\/style\s*>)/giu`): the byte range of
/// the first `<style>` element at or after `from`.
fn style_element(text: &str, from: usize) -> Option<(usize, usize)> {
    let lower = text.to_ascii_lowercase();
    let mut at = from;
    while let Some(start) = lower[at..].find("<style").map(|found| at + found) {
        at = start + 1;
        let after = start + "<style".len();
        // `\b`: the tag name ends at a non-word character (or the end).
        if lower.as_bytes().get(after).is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_') {
            continue;
        }
        let Some(open_end) = lower[after..].find('>').map(|found| after + found + 1) else { continue };
        let mut close = open_end;
        while let Some(found) = lower[close..].find("</style").map(|found| close + found) {
            let tail = &text[found + "</style".len()..];
            let spaced = tail.trim_start_matches(js::is_space);
            if spaced.starts_with('>') {
                return Some((start, text.len() - spaced.len() + 1));
            }
            close = found + 1;
        }
    }
    None
}

/// `encodeAnkiClozeBraces`: braces outside `<style>` elements, as entities.
pub(crate) fn encode_cloze_braces(value: &str) -> String {
    let encode = |part: &str| part.replace('{', "&#123;").replace('}', "&#125;");
    let mut out = String::with_capacity(value.len());
    let mut from = 0;
    while let Some((start, end)) = style_element(value, from) {
        out.push_str(&encode(&value[from..start]));
        out.push_str(&value[start..end]);
        from = end;
    }
    out.push_str(&encode(&value[from..]));
    out
}

/// `renderAnkiTemplate`: `template` with each marker's value, a line whose markers
/// all came out empty dropped.
pub(crate) fn render(template: &str, values: &HashMap<String, String>) -> Result<String, String> {
    let errors = template_errors(template);
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    let mut lines = Vec::new();
    for segment in split_breaks(template) {
        let found = markers(segment);
        let mut rendered = String::new();
        let mut from = 0;
        for &(start, end, name) in &found {
            rendered.push_str(&segment[from..start]);
            let key = name.to_lowercase();
            let value = values.get(&key).or_else(|| alias(&key).and_then(|canonical| values.get(canonical)));
            rendered.push_str(&encode_cloze_braces(value.map_or("", String::as_str)));
            from = end;
        }
        rendered.push_str(&segment[from..]);
        if found.is_empty() || !js::trim(&rendered).is_empty() {
            lines.push(rendered);
        }
    }
    Ok(lines.join("<br>"))
}

/// A note type's fields, each with the Template that fills it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Resolved {
    /// In the note type's field order; each value is `{value, overwriteMode, ...}`.
    pub(crate) templates: Vec<(String, Map<String, Value>)>,
    pub(crate) stale_fields: Vec<String>,
    pub(crate) errors: Vec<String>,
}

impl Resolved {
    pub(crate) fn get(&self, field: &str) -> Option<&Map<String, Value>> {
        self.templates.iter().find(|(name, _)| name == field).map(|(_, template)| template)
    }

    /// The Template text of `field`, or "".
    pub(crate) fn value(&self, field: &str) -> &str {
        self.get(field).and_then(|template| template.get("value")).and_then(Value::as_str).unwrap_or_default()
    }
}

/// `blankTemplate`.
fn blank() -> Map<String, Value> {
    let mut template = Map::new();
    template.insert("value".to_owned(), json!(""));
    template.insert("overwriteMode".to_owned(), json!("coalesce"));
    template
}

/// `availableFields`.
fn available_fields(fields: &[String]) -> String {
    if fields.is_empty() {
        return String::new();
    }
    format!(" Its fields are {}.", fields.iter().map(|field| format!("“{field}”")).collect::<Vec<_>>().join(", "))
}

fn text<'a>(config: &'a Map<String, Value>, key: &str) -> &'a str {
    config.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// `ankiFieldNames`: each field by its lower-cased name, the last one winning.
pub(crate) fn field_names(fields: &[String]) -> HashMap<String, String> {
    fields.iter().map(|field| (field.to_lowercase(), field.clone())).collect()
}

/// `basicTemplates`: a legacy mapping's semantic fields as markers.
fn basic(config: &Map<String, Value>, fields: &[String]) -> Resolved {
    let canonical = field_names(fields);
    let mut templates: Vec<(String, Map<String, Value>)> = Vec::new();
    for field in fields {
        if let Some(slot) = templates.iter_mut().find(|(name, _)| name == field) {
            slot.1 = blank();
        } else {
            templates.push((field.clone(), blank()));
        }
    }
    let mut errors = Vec::new();
    let mapping = config.get("fields");
    for semantic in ANKI_FIELDS {
        let name = mapping.and_then(|fields| fields.get(semantic)).and_then(Value::as_str).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let Some(field) = canonical.get(&name.to_lowercase()) else {
            errors.push(format!(
                "The {semantic} mapping points at field “{name}”, which is unavailable in note type “{}”.{}",
                text(config, "model"),
                available_fields(fields)
            ));
            continue;
        };
        let marker = if semantic == "pitch" && field.to_lowercase() == "pitchposition" { "pitch-position" } else { semantic };
        if let Some((_, row)) = templates.iter_mut().find(|(name, _)| name == field) {
            let value = row.get("value").and_then(Value::as_str).unwrap_or_default();
            let next = format!("{value}{}{{{marker}}}", if value.is_empty() { "" } else { "<br>" });
            row.insert("value".to_owned(), json!(next));
        }
    }
    Resolved { templates, stale_fields: Vec::new(), errors }
}

/// `resolveAnkiTemplates`: the Template of each of `fields`, and what does not fit.
pub(crate) fn resolve(config: &Map<String, Value>, fields: &[String]) -> Resolved {
    let Some(saved) = config.get("fieldTemplates").and_then(Value::as_object) else { return basic(config, fields) };
    let mut folded: HashMap<String, &String> = HashMap::new();
    for name in saved.keys() {
        folded.entry(name.to_lowercase()).or_insert(name);
    }
    let mut used: Vec<&str> = Vec::new();
    let mut templates: Vec<(String, Map<String, Value>)> = Vec::new();
    for field in fields {
        let name = if saved.contains_key(field) { Some(field) } else { folded.get(&field.to_lowercase()).copied() };
        let template = match name {
            None => blank(),
            Some(name) => {
                used.push(name);
                saved.get(name).and_then(Value::as_object).cloned().unwrap_or_default()
            }
        };
        if let Some(slot) = templates.iter_mut().find(|(known, _)| known == field) {
            slot.1 = template;
        } else {
            templates.push((field.clone(), template));
        }
    }
    let stale_fields: Vec<String> = saved.keys().filter(|field| !used.contains(&field.as_str())).cloned().collect();
    let mut errors: Vec<String> = stale_fields
        .iter()
        .map(|field| {
            format!("Template field “{field}” is unavailable in note type “{}”.{}", text(config, "model"), available_fields(fields))
        })
        .collect();
    for (field, template) in &templates {
        let value = template.get("value").and_then(Value::as_str).unwrap_or_default();
        errors.extend(template_errors(value).into_iter().map(|error| format!("Field “{field}”: {error}")));
    }
    Resolved { templates, stale_fields, errors }
}

/// A preset's table: each note type field and the Template it gets.
type Table = &'static [(&'static str, &'static str)];

const KIKU: Table = &[
    ("Expression", "{expression}"),
    ("ExpressionFurigana", "{furigana-plain}"),
    ("ExpressionReading", "{reading}"),
    ("ExpressionAudio", "{audio}"),
    ("RelatedExpression", ""),
    ("SelectionText", "{popup-selection-text}"),
    ("MainDefinition", "{main-definition}"),
    ("DefinitionPicture", ""),
    ("Sentence", "{cloze-prefix}<b>{cloze-body}</b>{cloze-suffix}"),
    ("SentenceFurigana", "{sentence-furigana-plain}"),
    ("SentenceTranslation", ""),
    ("SentenceAudio", ""),
    ("Picture", "{screenshot}"),
    ("Glossary", "{glossary}"),
    ("Hint", ""),
    ("IsWordAndSentenceCard", ""),
    ("IsClickCard", ""),
    ("IsSentenceCard", ""),
    ("IsAudioCard", ""),
    ("PitchPosition", "{pitch-accent-positions}"),
    ("PitchCategories", "{pitch-accent-categories}"),
    ("Frequency", "{frequencies}"),
    ("FreqSort", "{frequency-harmonic-rank}"),
    ("MiscInfo", "{document-title}"),
];
const LAPIS: Table = &[
    ("Expression", "{expression}"),
    ("ExpressionFurigana", "{furigana-plain}"),
    ("ExpressionReading", "{reading}"),
    ("ExpressionAudio", "{audio}"),
    ("SelectionText", "{popup-selection-text}"),
    ("MainDefinition", "{main-definition}"),
    ("DefinitionPicture", ""),
    ("Sentence", "{cloze-prefix}<b>{cloze-body}</b>{cloze-suffix}"),
    ("SentenceFurigana", ""),
    ("SentenceAudio", ""),
    ("Picture", "{screenshot}"),
    ("Glossary", "{glossary}"),
    ("Hint", ""),
    ("IsWordAndSentenceCard", ""),
    ("IsClickCard", ""),
    ("IsSentenceCard", ""),
    ("IsAudioCard", ""),
    ("PitchPosition", "{pitch-accent-positions}"),
    ("PitchCategories", "{pitch-accent-categories}"),
    ("Frequency", "{frequencies}"),
    ("FreqSort", "{frequency-harmonic-rank}"),
    ("MiscInfo", "{document-title}"),
];
const SENREN: Table = &[
    ("word", "{expression}"),
    ("reading", "{reading}"),
    ("sentence", "<span class=\"group\">{cloze-prefix}<span class=\"highlight\">{cloze-body}</span>{cloze-suffix}</span>"),
    ("sentenceFurigana", "<span class=\"group\">{sentence-furigana}</span>"),
    ("sentenceTranslation", ""),
    ("sentenceCard", ""),
    ("audioCard", ""),
    ("notes", ""),
    ("hint", ""),
    ("picture", "{screenshot}"),
    ("wordAudio", "{audio}"),
    ("sentenceAudio", ""),
    ("selectionText", "{popup-selection-text}"),
    ("definition", "{main-definition}"),
    ("glossary", "{glossary}"),
    ("pitchAccents", "{pitch}"),
    ("pitchPositions", "{pitch-accent-positions}"),
    ("pitchCategories", "{pitch-accent-categories}"),
    ("frequencies", "{frequencies}"),
    ("freqSort", "{frequency-harmonic-rank}"),
    ("miscInfo", "{document-title}"),
    ("dictionaryPreference", ""),
];
/// `KIKU_LAPIS_SLOTS`.
const KIKU_LAPIS_SLOTS: [(&str, &str); 10] = [
    ("ExpressionFurigana", "expression-furigana"),
    ("ExpressionReading", "reading"),
    ("ExpressionAudio", "audio"),
    ("SelectionText", "selection-text"),
    ("MainDefinition", "main-definition"),
    ("SentenceFurigana", "sentence-furigana"),
    ("PitchPosition", "pitch"),
    ("PitchCategories", "pitch-categories"),
    ("FreqSort", "frequency-sort"),
    ("MiscInfo", "document-title"),
];
/// `genericAliases`, for the automatic preset.
const GENERIC_ALIASES: [(&str, &[&str]); 7] = [
    ("expression", &["Expression", "Word", "Term", "Front"]),
    ("reading", &["Reading", "Word Reading", "WordReading", "Kana"]),
    ("definition", &["Definition", "Definitions", "Meaning", "Glossary"]),
    ("sentence", &["Sentence", "Context", "Example Sentence"]),
    ("frequency", &["Frequency", "Frequencies"]),
    ("pitch", &["Pitch Accent", "PitchAccent", "Pitch", "Accent"]),
    ("audio", &["WordAudio", "PronunciationAudio", "Pronunciation", "Audio"]),
];

fn preset(family: &str) -> Option<Table> {
    match family {
        "kiku" => Some(KIKU),
        "lapis" => Some(LAPIS),
        "senren" => Some(SENREN),
        _ => None,
    }
}

fn lookup(table: Table, field: &str) -> &'static str {
    table.iter().find(|(name, _)| *name == field).map_or("", |(_, value)| value)
}

/// `fieldKey`: lower-cased, letters and numbers only.
fn field_key(value: &str) -> String {
    value.to_lowercase().chars().filter(|&c| js::is_letter_or_number(c)).collect()
}

/// `ankiPresetCoreMapped`: whether `field_templates` maps the family's core fields.
pub(crate) fn preset_core_mapped(field_templates: &Map<String, Value>, family: &str) -> bool {
    let Some(table) = preset(family) else { return false };
    let values: Vec<&str> = field_templates.values().filter_map(|template| template.get("value").and_then(Value::as_str)).collect();
    let has = |value: &str| values.contains(&value);
    let senren = family == "senren";
    let pick = |senren_field: &str, field: &str| lookup(table, if senren { senren_field } else { field });
    has(pick("word", "Expression"))
        && has(pick("reading", "ExpressionReading"))
        && (has(pick("definition", "MainDefinition")) || has(pick("glossary", "Glossary")))
        && has(pick("sentence", "Sentence"))
}

/// `applyAnkiPreset`: `config` with the field Templates `preset` proposes for `fields`.
pub(crate) fn apply_preset(config: &Map<String, Value>, fields: &[String], preset_name: &str) -> Map<String, Value> {
    let table = preset(preset_name).unwrap_or(KIKU);
    let senren = std::ptr::eq(table, SENREN);
    let mut suggestions: HashMap<String, (String, String)> = HashMap::new();
    if preset_name == "automatic" {
        for (semantic, aliases) in GENERIC_ALIASES {
            for alias in aliases {
                suggestions.insert(field_key(alias), (semantic.to_owned(), format!("{{{semantic}}}")));
            }
        }
    }
    for (field, value) in table {
        if preset_name == "automatic" && value.is_empty() {
            continue;
        }
        let slot = if senren {
            field_key(field)
        } else {
            KIKU_LAPIS_SLOTS.iter().find(|(name, _)| name == field).map_or_else(|| field.to_lowercase(), |(_, slot)| (*slot).to_owned())
        };
        suggestions.insert(field_key(field), (slot, (*value).to_owned()));
    }
    let mut used: Vec<String> = Vec::new();
    let mut field_templates = Map::new();
    for field in fields {
        let mut template = blank();
        if let Some((slot, value)) = suggestions.get(&field_key(field))
            && !used.contains(slot)
        {
            used.push(slot.clone());
            template.insert("value".to_owned(), json!(value));
        }
        field_templates.insert(field.clone(), Value::Object(template));
    }
    let mut next = config.clone();
    next.insert("fieldTemplates".to_owned(), Value::Object(field_templates));
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn markers_are_found_as_the_pattern_finds_them() {
        assert_eq!(marker_names("{Expression}<br>{pitch-accent}{{c1::{cloze-body}}} {} {a{b}"), ["expression", "pitch", "cloze-body", "b"]);
        assert_eq!(
            template_errors("{nope}{nope}{single-glossary-}{single-glossary-JMdict}"),
            ["Unknown marker: {nope}", "Unknown marker: {single-glossary-}"]
        );
        assert!(is_audio_only("{audio}<br/> "));
        assert!(!is_audio_only("{audio} x"));
    }

    #[test]
    fn rendering_drops_empty_marker_lines_and_encodes_braces() {
        let values: HashMap<String, String> =
            [("expression", "{猫}"), ("pitch", "<style>.a{b}</style>{x}")].into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
        assert_eq!(
            render("{expression}<br>{reading}<br >plain<BR/>{pitch-accent}", &values).unwrap(),
            "&#123;猫&#125;<br>plain<br><style>.a{b}</style>&#123;x&#125;"
        );
        assert!(render("{nope}", &values).is_err());
        assert_eq!(escape_html("<a href=\"x\">'&'</a>"), "&lt;a href=&quot;x&quot;&gt;&#x27;&amp;&#x27;&lt;/a&gt;");
    }

    #[test]
    fn templates_resolve_against_the_note_type() {
        let mut config: Map<String, Value> = serde_json::from_value(json!({ "model": "Basic", "fieldTemplates": null,
            "fields": { "expression": "front", "reading": "Front", "definition": "Gone", "sentence": "", "frequency": "", "pitch": "PitchPosition",
                "audio": "", "screenshot": "" } }))
        .unwrap();
        let resolved = resolve(&config, &fields(&["Front", "Back", "PitchPosition"]));
        assert_eq!(resolved.value("Front"), "{expression}<br>{reading}");
        assert_eq!(resolved.value("PitchPosition"), "{pitch-position}");
        assert_eq!(
            resolved.errors,
            [
                "The definition mapping points at field “Gone”, which is unavailable in note type “Basic”. Its fields are “Front”, “Back”, “PitchPosition”."
            ]
        );
        config.insert("fieldTemplates".to_owned(), json!({ "front": { "value": "{expression}", "overwriteMode": "overwrite" }, "Old": { "value": "{x}", "overwriteMode": "coalesce" } }));
        let resolved = resolve(&config, &fields(&["Front", "Back"]));
        assert_eq!(resolved.value("Front"), "{expression}");
        assert_eq!(resolved.value("Back"), "");
        assert_eq!(resolved.stale_fields, ["Old"]);
    }

    #[test]
    fn presets_map_their_families() {
        let config = Map::new();
        let kiku = apply_preset(&config, &fields(&["Expression", "ExpressionReading", "MainDefinition", "Sentence", "Picture"]), "kiku");
        let templates = kiku["fieldTemplates"].as_object().unwrap();
        assert_eq!(templates["Picture"]["value"], "{screenshot}");
        assert!(preset_core_mapped(templates, "kiku"));
        assert!(!preset_core_mapped(templates, "senren"));
        let automatic = apply_preset(&config, &fields(&["Word", "Term", "Meaning"]), "automatic");
        assert_eq!(automatic["fieldTemplates"]["Word"]["value"], "{expression}");
        assert_eq!(automatic["fieldTemplates"]["Term"]["value"], "");
        assert_eq!(automatic["fieldTemplates"]["Meaning"]["value"], "{definition}");
    }
}
