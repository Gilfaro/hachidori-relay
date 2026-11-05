// SPDX-License-Identifier: GPL-3.0-or-later
//! The managed custom dictionary's source document (`custom-dictionary.js`): its
//! stored form, the line format Settings edits, the semantic revision that names a
//! source's entries, and the rules its package in `dictionaryState` follows.

use std::fmt::Write as _;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::host::js::{self, strict_eq};

pub(crate) const CUSTOM_DICTIONARY_ID: &str = "e4c2e20a1a964b6cbd4ae3f87643c1f0";
pub(crate) const CUSTOM_DICTIONARY_TITLE: &str = "Hachidori Custom Dictionary";
pub(crate) const CUSTOM_DICTIONARY_SOURCE_KEY: &str = "customDictionarySource";
const SCHEMA_VERSION: u64 = 1;
const EMPTY_SEMANTIC_REVISION: &str = "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945";

/// One entry of the source: what a line names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) term: String,
    pub(crate) reading: String,
    pub(crate) definition: String,
}

fn is_custom(dictionary: &Value) -> bool {
    dictionary.get("id").and_then(Value::as_str) == Some(CUSTOM_DICTIONARY_ID)
}

/// `customDictionaryMetadataMatches`.
pub(crate) fn metadata_matches(value: &Value, semantic_revision: &str, entry_count: usize) -> bool {
    let number = |key: &str| value.get(key).and_then(Value::as_f64);
    value.get("title").and_then(Value::as_str) == Some(CUSTOM_DICTIONARY_TITLE)
        && value.get("revision").and_then(Value::as_str) == Some(semantic_revision)
        && strict_eq(value.get("termCount"), Some(&json!(entry_count)))
        && ["frequencyCount", "pitchCount", "kanjiCount", "mediaCount"].iter().all(|key| number(key) == Some(0.0))
        && value.get("isUpdatable") == Some(&Value::Bool(false))
        && value.get("indexUrl") == Some(&Value::Null)
        && value.get("downloadUrl") == Some(&Value::Null)
        && value.get("language").and_then(Value::as_str) == Some("ja")
}

/// `assertCustomDictionaryCommit`.
pub(crate) fn assert_commit(dictionaries: &[Value]) -> Result<(), String> {
    let custom: Vec<usize> =
        dictionaries.iter().enumerate().filter(|(_, dictionary)| is_custom(dictionary)).map(|(index, _)| index).collect();
    if custom.len() > 1 {
        return Err("the custom dictionary state contains duplicate managed packages".to_owned());
    }
    let Some(&index) = custom.first() else { return Ok(()) };
    let entry = &dictionaries[index];
    if index != 0
        || entry.get("title").and_then(Value::as_str) != Some(CUSTOM_DICTIONARY_TITLE)
        || entry.get("enabled") != Some(&Value::Bool(true))
    {
        return Err("the managed custom dictionary must stay enabled and first".to_owned());
    }
    if dictionaries
        .iter()
        .enumerate()
        .any(|(other, dictionary)| other != index && dictionary.get("title").and_then(Value::as_str) == Some(CUSTOM_DICTIONARY_TITLE))
    {
        return Err(format!("a dictionary named {CUSTOM_DICTIONARY_TITLE} is already installed"));
    }
    Ok(())
}

/// `assertCustomSourceState`.
pub(crate) fn assert_source_state(dictionaries: &[Value], semantic_revision: &str, entry_count: usize) -> Result<(), String> {
    let custom: Vec<&Value> = dictionaries.iter().filter(|dictionary| is_custom(dictionary)).collect();
    if entry_count == 0 {
        if !custom.is_empty() {
            return Err("a zero-entry custom source cannot retain its managed package".to_owned());
        }
        return Ok(());
    }
    assert_commit(dictionaries)?;
    if custom.len() != 1 || !metadata_matches(custom[0], semantic_revision, entry_count) {
        return Err("the custom dictionary package does not match its source semantics".to_owned());
    }
    Ok(())
}

/// `emptyCustomDictionaryDocument`.
pub(crate) fn empty_document() -> Value {
    json!({ "schemaVersion": SCHEMA_VERSION, "revision": 0, "semanticRevision": EMPTY_SEMANTIC_REVISION, "text": "" })
}

/// `normaliseCustomDictionaryDocument`.
pub(crate) fn normalise_document(value: Option<&Value>) -> Result<Value, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else { return Ok(empty_document()) };
    let Some(fields) = value.as_object() else { return Err("the custom dictionary document is invalid".to_owned()) };
    if fields.get("schemaVersion").and_then(Value::as_f64) != Some(1.0) {
        return Err(format!("unsupported custom dictionary source schema {}", js::to_string(fields.get("schemaVersion"))));
    }
    let Some(revision) = js::whole(fields.get("revision")) else {
        return Err("the custom dictionary document revision is invalid".to_owned());
    };
    let semantic = fields.get("semanticRevision").and_then(Value::as_str);
    let text = fields.get("text").and_then(Value::as_str);
    let (Some(semantic), Some(text)) = (
        semantic.filter(|semantic| semantic.len() == 64 && semantic.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
        text,
    ) else {
        return Err("the custom dictionary document content is invalid".to_owned());
    };
    Ok(json!({ "schemaVersion": SCHEMA_VERSION, "revision": js::number(revision), "semanticRevision": semantic, "text": text }))
}

/// `decodeDefinition`: `\\` and `\n` escapes.
fn decode_definition(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\\', Some('\\')) => {
                decoded.push('\\');
                chars.next();
            }
            ('\\', Some('n')) => {
                decoded.push('\n');
                chars.next();
            }
            _ => decoded.push(c),
        }
    }
    decoded
}

/// `parseCustomDictionary`: the entries of a source, and its line errors.
pub(crate) fn parse(source: &str) -> (Vec<Entry>, Vec<Value>) {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    for (index, line) in normalized.split('\n').enumerate() {
        if js::trim(line).is_empty() || line.trim_start_matches(js::is_space).starts_with('#') {
            continue;
        }
        let mut error = |reason: &str| errors.push(json!({ "lineNumber": index + 1, "reason": reason }));
        let Some((term, rest)) = line.split_once(',') else {
            error("expected two commas");
            continue;
        };
        let Some((reading, definition)) = rest.split_once(',') else {
            error("expected two commas");
            continue;
        };
        let (term, reading) = (js::trim(term), js::trim(reading));
        let definition = decode_definition(js::trim(definition));
        if term.is_empty() {
            error("term is empty");
        } else if reading.is_empty() {
            error("reading is empty");
        } else if js::trim(&definition).is_empty() {
            error("definition is empty");
        } else {
            entries.push(Entry { term: term.to_owned(), reading: reading.to_owned(), definition });
        }
    }
    (entries, errors)
}

/// `normaliseEntry`.
fn normalise_entry(entry: &Entry) -> Result<Entry, String> {
    let term = js::trim(&entry.term);
    let reading = js::trim(&entry.reading);
    let definition = js::trim(&entry.definition).replace("\r\n", "\n").replace('\r', "\n");
    let breaks = |text: &str| text.contains(['\r', '\n']);
    if term.is_empty() {
        return Err("the custom dictionary term is empty".to_owned());
    }
    if term.starts_with('#') {
        return Err("the custom dictionary term cannot begin with #".to_owned());
    }
    if term.contains(',') || breaks(term) {
        return Err("the custom dictionary term cannot contain a comma or newline".to_owned());
    }
    if reading.is_empty() {
        return Err("the custom dictionary reading is empty".to_owned());
    }
    if reading.contains(',') || breaks(reading) {
        return Err("the custom dictionary reading cannot contain a comma or newline".to_owned());
    }
    if definition.is_empty() {
        return Err("the custom dictionary definition is empty".to_owned());
    }
    Ok(Entry { term: term.to_owned(), reading: reading.to_owned(), definition })
}

/// A string as `JSON.stringify` writes it.
fn json_string(text: &str) -> String {
    Value::from(text).to_string()
}

/// `customDictionarySemanticRevision`: the SHA-256 of the normalised entries'
/// `JSON.stringify`, in hex.
pub(crate) fn semantic_revision(entries: &[Entry]) -> Result<String, String> {
    let mut json = String::from("[");
    for (index, entry) in entries.iter().enumerate() {
        let entry = normalise_entry(entry)?;
        if index > 0 {
            json.push(',');
        }
        let _ = write!(
            json,
            r#"{{"term":{},"reading":{},"definition":{}}}"#,
            json_string(&entry.term),
            json_string(&entry.reading),
            json_string(&entry.definition)
        );
    }
    json.push(']');
    Ok(super::hex(&Sha256::digest(json.as_bytes())))
}

/// `sameJsonValue` of two dictionary entries without their presentation fields
/// (`customPackageEngineState`).
pub(crate) fn same_engine_state(left: &Value, right: &Value) -> bool {
    let strip = |value: &Value| match value {
        Value::Object(fields) => {
            let mut fields = fields.clone();
            fields.remove("displayName");
            fields.remove("favorite");
            Value::Object(fields)
        }
        other => other.clone(),
    };
    js::same_json(&strip(left), &strip(right))
}

/// `assertOrdinaryCustomTransition`: an ordinary state write leaves the managed
/// package where and as it is.
pub(crate) fn assert_ordinary_transition(current: &[Value], next: &[Value]) -> Result<(), String> {
    let current_index = current.iter().position(is_custom);
    let next_indexes: Vec<usize> =
        next.iter().enumerate().filter(|(_, dictionary)| is_custom(dictionary)).map(|(index, _)| index).collect();
    match (current_index, next_indexes.as_slice()) {
        (None, []) => Ok(()),
        (Some(current_index), [next_index]) => {
            let (before, after) = (&current[current_index], &next[*next_index]);
            let title = |value: &Value| strict_eq(value.get("title"), Some(&json!(CUSTOM_DICTIONARY_TITLE)));
            let enabled = |value: &Value| value.get("enabled") == Some(&Value::Bool(true));
            if current_index != 0
                || !title(before)
                || !enabled(before)
                || *next_index != 0
                || !title(after)
                || !enabled(after)
                || !same_engine_state(before, after)
            {
                return Err("the managed custom dictionary must stay enabled and first".to_owned());
            }
            Ok(())
        }
        _ => Err("the managed custom dictionary can only be changed by its source editor".to_owned()),
    }
}

/// `normaliseEntry` of an entry a request carried.
fn entry_from(value: Option<&Value>) -> Result<Entry, String> {
    let Some(fields) = js::plain_object(value) else { return Err("the custom dictionary entry must be an object".to_owned()) };
    let field = |key: &str| fields.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    normalise_entry(&Entry { term: field("term"), reading: field("reading"), definition: field("definition") })
}

/// `appendCustomDictionaryEntry`: `source` with one more line, in its own line endings.
pub(crate) fn append(source: &str, entry: Option<&Value>) -> Result<String, String> {
    let Entry { term, reading, definition } = entry_from(entry)?;
    let newline = if source.contains("\r\n") { "\r\n" } else { "\n" };
    let separator = if source.is_empty() || source.ends_with(['\r', '\n']) { "" } else { newline };
    let definition = definition.replace('\\', "\\\\").replace('\n', "\\n");
    Ok(format!("{source}{separator}{term}, {reading}, {definition}{newline}"))
}

/// `TERM_BANK_SIZE`.
const TERM_BANK_SIZE: usize = 1000;

/// `buildCustomDictionaryZip`: the Yomitan archive the custom package is imported from.
pub(crate) fn build_zip(entries: &[Entry], revision: &str) -> Result<Vec<u8>, String> {
    if entries.is_empty() {
        return Err("the custom dictionary needs at least one entry".to_owned());
    }
    if revision.is_empty() {
        return Err("the custom dictionary needs a semantic revision".to_owned());
    }
    let entries = entries.iter().map(normalise_entry).collect::<Result<Vec<_>, _>>()?;
    let mut files = vec![(
        "index.json".to_owned(),
        json!({
            "title": CUSTOM_DICTIONARY_TITLE, "format": 3, "revision": revision, "sequenced": true, "author": "Hachidori",
            "description": "Personal entries managed by Hachidori", "sourceLanguage": "ja", "targetLanguage": "en",
        }),
    )];
    for (bank, chunk) in entries.chunks(TERM_BANK_SIZE).enumerate() {
        let rows: Vec<Value> = chunk
            .iter()
            .enumerate()
            .map(|(index, entry)| json!([entry.term, entry.reading, "", "", 0, [entry.definition], bank * TERM_BANK_SIZE + index + 1, ""]))
            .collect();
        files.push((format!("term_bank_{}.json", files.len()), Value::Array(rows)));
    }
    let too_large = || "the custom dictionary exceeds the classic ZIP representation".to_owned();
    if files.len() > usize::from(u16::MAX) {
        return Err(too_large());
    }
    let stored = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(zip::DateTime::default());
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, value) in files {
        writer.start_file(name, stored).map_err(|_| too_large())?;
        std::io::Write::write_all(&mut writer, value.to_string().as_bytes()).map_err(|error| error.to_string())?;
    }
    Ok(writer.finish().map_err(|_| too_large())?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_empty_source_has_the_extensions_revision() {
        assert_eq!(semantic_revision(&[]).as_deref(), Ok(EMPTY_SEMANTIC_REVISION));
    }

    #[test]
    fn lines_parse_as_settings_writes_them() {
        let (entries, errors) =
            parse("\u{feff}# note\r\n食べる, たべる, to eat\\nconsume\n\n  #x\nbad line\n,a,b\nx,,y\nx,y, \\\\ \r漢字,かんじ,a,b");
        assert_eq!(
            entries,
            [
                Entry { term: "食べる".into(), reading: "たべる".into(), definition: "to eat\nconsume".into() },
                Entry { term: "x".into(), reading: "y".into(), definition: "\\".into() },
                Entry { term: "漢字".into(), reading: "かんじ".into(), definition: "a,b".into() },
            ]
        );
        let reasons: Vec<_> = errors.iter().map(|error| (error["lineNumber"].as_u64(), error["reason"].as_str())).collect();
        assert_eq!(
            reasons,
            [(Some(5), Some("expected two commas")), (Some(6), Some("term is empty")), (Some(7), Some("reading is empty"))]
        );
    }
}
