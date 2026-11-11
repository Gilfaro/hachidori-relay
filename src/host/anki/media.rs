// SPDX-License-Identifier: GPL-3.0-or-later
//! The files a note references, stored in Anki before the note is written
//! (`anki-media.js`), and the media a linked browser sends with its submission
//! (`anki-client-media.js`).

use serde_json::{Map, Value, json};

use super::Invoke;
use super::duplicates::Fields;
use super::gateway::{Failure, Result};
use crate::host::js;

const MAX_LINKED_SCREENSHOT_BYTES: usize = 6 * 1024 * 1024;
/// `MAX_WAV_BYTES`.
const MAX_LINKED_SPEECH_BYTES: usize = 1024 * 1024;
const MEDIA_TIMEOUT_MS: u64 = 10_000;
const STORE_TIMEOUT_MS: u64 = 30_000;

/// `decodedBase64Length`: the bytes `value` decodes to, if it is padded base64.
pub(crate) fn decoded_base64_length(value: Option<&Value>) -> Option<usize> {
    let value = value?.as_str()?;
    let body = value.trim_end_matches('=');
    if value.is_empty()
        || value.len() % 4 != 0
        || value.len() - body.len() > 2
        || !body.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
    {
        return None;
    }
    Some(value.len() / 4 * 3 - (value.len() - body.len()))
}

fn generated_filename(filename: &str) -> bool {
    let Some(rest) = filename.strip_prefix("hachidori_") else { return false };
    let Some((digest, extension)) = rest.split_once('.') else { return false };
    digest.len() == 64
        && digest.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && !extension.is_empty()
        && extension.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn screenshot_filename(filename: &str) -> bool {
    let Some(id) = filename.strip_prefix("hachidori-screenshot-").and_then(|rest| rest.strip_suffix(".jpg")) else { return false };
    let parts: Vec<&str> = id.split('-').collect();
    let hex = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    parts.len() == 5
        && [8, 4, 4, 4, 12].iter().zip(&parts).all(|(length, part)| part.len() == *length && hex(part))
        && parts[2].starts_with('4')
        && parts[3].starts_with(['8', '9', 'a', 'b'])
}

/// `exactMediaExists`.
fn exists(invoke: &Invoke<'_>, filename: &str) -> Result<bool> {
    let names = invoke("getMediaFilesNames", json!({ "pattern": filename }), Some(MEDIA_TIMEOUT_MS))?;
    let names = names
        .as_array()
        .filter(|names| names.iter().all(Value::is_string))
        .ok_or_else(|| Failure::new("Anki returned an invalid media inventory."))?;
    Ok(names.iter().any(|name| name == filename))
}

/// What `ensure` found or did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stored {
    Existing,
    ConfirmedAfterError,
    ExistingRace,
    New,
}

/// `ensure`: `filename` in Anki's media, loading its bytes only if it is missing.
pub(crate) fn ensure(
    invoke: &Invoke<'_>,
    filename: &str,
    kind: &str,
    load: &mut dyn FnMut() -> Result<String>,
    validate: &dyn Fn() -> Result<()>,
) -> Result<Stored> {
    if !generated_filename(filename) {
        return Err(Failure::new("The generated Anki media filename is invalid."));
    }
    validate()?;
    let found = exists(invoke, filename)?;
    validate()?;
    if found {
        return Ok(Stored::Existing);
    }
    let payload = load()?;
    if decoded_base64_length(Some(&json!(payload))).is_none_or(|bytes| bytes < 1) {
        return Err(Failure::new(format!("The {kind} payload is not valid base64.")));
    }
    validate()?;
    let stored =
        match invoke("storeMediaFile", json!({ "filename": filename, "data": payload, "deleteExisting": false }), Some(STORE_TIMEOUT_MS)) {
            Ok(stored) => stored,
            // A timed-out acknowledgement may follow a completed write.
            Err(error) => return if exists(invoke, filename).unwrap_or(false) { Ok(Stored::ConfirmedAfterError) } else { Err(error) },
        };
    if stored.as_str() != Some(filename) {
        if !exists(invoke, filename)? {
            return Err(Failure::new(format!("Anki stored {kind} under a different filename.")));
        }
        return Ok(Stored::ExistingRace);
    }
    if !exists(invoke, filename)? {
        return Err(Failure::new(format!("Anki acknowledged {kind} without confirming the requested media filename.")));
    }
    Ok(Stored::New)
}

/// A dictionary file a rendered field references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DictionaryMedia {
    pub(crate) dictionary: String,
    pub(crate) path: String,
    pub(crate) filename: String,
}

fn referenced(fields: &Fields, filename: &str) -> bool {
    fields.iter().any(|(_, value)| value.contains(filename))
}

/// `requiredDictionaryMedia`: the planned files `fields` still reference.
fn required(media: &[DictionaryMedia], fields: &Fields) -> Result<Vec<DictionaryMedia>> {
    let mut wanted: Vec<DictionaryMedia> = Vec::new();
    for item in media.iter().filter(|item| referenced(fields, &item.filename)) {
        match wanted.iter().find(|known| known.filename == item.filename) {
            Some(known) if known.dictionary != item.dictionary || known.path != item.path => {
                return Err(Failure::new(format!("Two dictionary resources planned the same Anki filename: {}", item.filename)));
            }
            Some(_) => {}
            None => wanted.push(item.clone()),
        }
    }
    Ok(wanted)
}

/// `prepare`: every dictionary file and the first-field pronunciation the note
/// references, in Anki; the filenames confirmed there.
pub(crate) fn prepare(
    invoke: &Invoke<'_>,
    fields: &Fields,
    media: &[DictionaryMedia],
    audio: Option<(&str, &str)>,
    load: &dyn Fn(&DictionaryMedia) -> Result<String>,
    validate: &dyn Fn() -> Result<()>,
) -> Result<Vec<String>> {
    let mut files: Vec<String> = Vec::new();
    let mut run = || -> Result<()> {
        for item in required(media, fields)? {
            ensure(invoke, &item.filename, "dictionary image", &mut || load(&item), validate)?;
            files.push(item.filename.clone());
        }
        if let Some((filename, data)) = audio.filter(|(filename, _)| referenced(fields, filename)) {
            ensure(invoke, filename, "pronunciation", &mut || Ok(data.to_owned()), validate)?;
            files.push(filename.to_owned());
        }
        Ok(())
    };
    if let Err(error) = run() {
        let retained = files.len();
        if retained > 0 {
            let files = if retained == 1 { "file was" } else { "files were" };
            return Err(Failure::new(format!("{} {retained} confirmed media {files} retained for a safe retry.", error.message)));
        }
        return Err(error);
    }
    Ok(files)
}

/// A file a native add carries: `{filename, data, type, kind, required}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddFile {
    pub(crate) filename: String,
    pub(crate) data: String,
    /// `type`: a picture (`<img>`), or audio (`[sound:]`).
    pub(crate) picture: bool,
    pub(crate) kind: String,
    /// A file the note cannot go without: its failure fails the add.
    pub(crate) required: bool,
}

impl AddFile {
    fn reference(&self) -> String {
        if self.picture { format!("<img src=\"{}\">", self.filename) } else { format!("[sound:{}]", self.filename) }
    }
}

/// `attachAnkiAddMedia`: AnkiConnect's `addNote` stores native `audio` and
/// `picture` attachments and appends their stored filenames to their fields, so a
/// field that is only the file's reference is emptied and the file attached.
/// Files inside rich HTML must keep their exact position: those are stored
/// together, each reply checked, before the note is sent. A failed optional file
/// loses its references and leaves a warning; a failed required one fails.
pub(crate) fn attach_add_media(
    fields: &mut Fields,
    attachments: &mut Map<String, Value>,
    files: &[AddFile],
    invoke: &Invoke<'_>,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let mut failed = |fields: &mut Fields, file: &AddFile, message: String| -> Result<()> {
        if file.required {
            return Err(Failure::new(message));
        }
        let reference = file.reference();
        for (_, value) in fields.iter_mut() {
            *value = value.replace(&reference, "");
        }
        warnings.push(format!("{}: {message}", file.kind));
        Ok(())
    };
    let mut uploads: Vec<&AddFile> = Vec::new();
    for file in files {
        if decoded_base64_length(Some(&json!(file.data))).is_none_or(|bytes| bytes < 1) {
            failed(fields, file, format!("The {} payload is not valid base64.", file.kind))?;
            continue;
        }
        let reference = file.reference();
        let named: Vec<String> = fields.iter().filter(|(_, value)| value.contains(&file.filename)).map(|(name, _)| name.clone()).collect();
        if named.is_empty() {
            continue;
        }
        if !file.required && fields.iter().filter(|(name, _)| named.contains(name)).all(|(_, value)| *value == reference) {
            for (name, value) in fields.iter_mut() {
                if named.contains(name) {
                    value.clear();
                }
            }
            let key = if file.picture { "picture" } else { "audio" };
            let list = attachments.entry(key).or_insert_with(|| Value::Array(Vec::new()));
            if let Some(list) = list.as_array_mut() {
                list.push(json!({ "filename": file.filename, "data": file.data, "fields": named, "deleteExisting": false }));
            }
        } else {
            uploads.push(file);
        }
    }
    if uploads.is_empty() {
        return Ok(warnings);
    }
    let actions: Vec<Value> = uploads
        .iter()
        .map(|file| json!({ "action": "storeMediaFile", "params": { "filename": file.filename, "data": file.data, "deleteExisting": false } }))
        .collect();
    let results = invoke("multi", json!({ "actions": actions }), Some(STORE_TIMEOUT_MS))?;
    let results = results.as_array().ok_or_else(|| Failure::new("Anki returned an invalid media reply."))?;
    for (reply, file) in results.iter().zip(&uploads) {
        let error = reply.get("error").filter(|error| !error.is_null());
        if let Some(error) = error {
            failed(fields, file, super::gateway::describe(&js::to_string(Some(error))))?;
        } else if reply.get("result").and_then(Value::as_str) != Some(file.filename.as_str()) {
            failed(fields, file, "Anki stored media under a different filename.".to_owned())?;
        }
    }
    Ok(warnings)
}

/// A linked browser's screenshot, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Screenshot {
    pub(crate) token: String,
    pub(crate) filename: String,
    pub(crate) data: String,
}

/// What a linked browser sent with its submission, checked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ClientMedia {
    pub(crate) screenshot: Option<Screenshot>,
    /// Browser speech; this host never plans any, so it is only checked.
    pub(crate) speech: bool,
}

fn exact_fields<'a>(value: Option<&'a Value>, allowed: &[&str], label: &str) -> std::result::Result<&'a Map<String, Value>, String> {
    value
        .and_then(Value::as_object)
        .filter(|value| value.keys().all(|key| allowed.contains(&key.as_str())))
        .ok_or_else(|| format!("The linked {label} payload is invalid."))
}

fn unavailable(request: &Value, kind: &str) -> bool {
    request.get("captureUnavailable").and_then(Value::as_array).is_some_and(|kinds| kinds.iter().any(|known| known == kind))
}

fn record(value: Option<&Value>) -> bool {
    value.is_some_and(Value::is_object)
}

fn validate_screenshot(request: &Value, value: Option<&Value>) -> std::result::Result<Screenshot, String> {
    let value = exact_fields(value, &["token", "filename", "data"], "screenshot")?;
    let expected = request.get("screenshot");
    let token = value.get("token").and_then(Value::as_str).unwrap_or_default();
    let filename = value.get("filename").and_then(Value::as_str).unwrap_or_default();
    let data = value.get("data").and_then(Value::as_str).unwrap_or_default();
    let bytes = decoded_base64_length(value.get("data"));
    let valid = record(expected)
        && !unavailable(request, "screenshot")
        && value.get("token").is_some_and(Value::is_string)
        && !token.is_empty()
        && js::length(token) <= 256
        && js::strict_eq(value.get("token"), expected.and_then(|expected| expected.get("token")))
        && js::strict_eq(value.get("filename"), expected.and_then(|expected| expected.get("filename")))
        && screenshot_filename(filename)
        && data.starts_with("/9j/")
        && bytes.is_some_and(|bytes| bytes <= MAX_LINKED_SCREENSHOT_BYTES);
    if !valid {
        return Err("The linked screenshot payload is invalid, stale, or exceeds its size limit.".to_owned());
    }
    Ok(Screenshot { token: token.to_owned(), filename: filename.to_owned(), data: data.to_owned() })
}

const SPEECH_PLAN_FIELDS: [&str; 4] = ["sourceId", "sourceKey", "expression", "reading"];

fn validate_speech_plan(value: Option<&Value>, label: &str) -> std::result::Result<Map<String, Value>, String> {
    let value = exact_fields(value, &SPEECH_PLAN_FIELDS, label)?;
    let text = |field: &str, empty: bool, limit: usize| {
        value.get(field).and_then(Value::as_str).is_some_and(|text| (empty || !text.is_empty()) && js::length(text) <= limit)
    };
    if !(text("sourceId", false, 256) && text("sourceKey", false, 16_384) && text("expression", false, 4096) && text("reading", true, 4096))
    {
        return Err(format!("The linked {label} is invalid."));
    }
    Ok(SPEECH_PLAN_FIELDS.iter().map(|field| ((*field).to_owned(), value[*field].clone())).collect())
}

fn validate_speech(request: &Value, value: Option<&Value>) -> std::result::Result<(), String> {
    let fields: Vec<&str> = SPEECH_PLAN_FIELDS.iter().copied().chain(["filename", "byteLength", "data"]).collect();
    let value = exact_fields(value, &fields, "browser-speech payload")?;
    let expected = validate_speech_plan(request.get("clientSpeech"), "browser-speech plan")?;
    let identity: Map<String, Value> =
        SPEECH_PLAN_FIELDS.iter().map(|field| ((*field).to_owned(), value.get(*field).cloned().unwrap_or(Value::Null))).collect();
    let actual = validate_speech_plan(Some(&Value::Object(identity)), "browser-speech identity")?;
    let filename = value.get("filename").and_then(Value::as_str).unwrap_or_default();
    let filename_ok = filename
        .strip_suffix(".wav")
        .and_then(|rest| rest.strip_prefix("hachidori_"))
        .is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
    let length = js::safe_integer(value.get("byteLength")).filter(|_| value.get("byteLength").is_some_and(Value::is_number));
    let bytes = decoded_base64_length(value.get("data"));
    #[expect(clippy::cast_precision_loss, reason = "a small limit")]
    let valid = actual == expected
        && filename_ok
        && length.is_some_and(|length| length >= 1.0 && length <= MAX_LINKED_SPEECH_BYTES as f64)
        && bytes.is_some_and(|bytes| Some(bytes as f64) == length)
        && value.get("data").and_then(Value::as_str).is_some_and(|data| data.starts_with("UklG"));
    if !valid {
        return Err("The linked browser-speech payload is invalid, stale, or exceeds its size limit.".to_owned());
    }
    Ok(())
}

/// `validateLinkedAnkiClientMedia`: exactly the media the request said it would send.
pub(crate) fn validate_client_media(request: &Value, value: &Value) -> std::result::Result<ClientMedia, String> {
    let value = exact_fields(Some(value), &["screenshot", "speech"], "client-media envelope")?;
    let expects_screenshot = record(request.get("screenshot")) && !unavailable(request, "screenshot");
    let expects_speech = record(request.get("clientSpeech"));
    if expects_screenshot != value.contains_key("screenshot") || expects_speech != value.contains_key("speech") {
        return Err("The linked client-media envelope is missing media for this mining request.".to_owned());
    }
    let screenshot = value.get("screenshot").map(|screenshot| validate_screenshot(request, Some(screenshot))).transpose()?;
    if let Some(speech) = value.get("speech") {
        validate_speech(request, Some(speech))?;
    }
    Ok(ClientMedia { screenshot, speech: value.contains_key("speech") })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn measures_base64() {
        assert_eq!(decoded_base64_length(Some(&json!("QUJD"))), Some(3));
        assert_eq!(decoded_base64_length(Some(&json!("QUI="))), Some(2));
        assert_eq!(decoded_base64_length(Some(&json!("QQ=="))), Some(1));
        assert_eq!(decoded_base64_length(Some(&json!("Q==="))), None);
        assert_eq!(decoded_base64_length(Some(&json!("QUJ"))), None);
        assert_eq!(decoded_base64_length(Some(&json!(""))), None);
    }

    #[test]
    fn checks_client_media() {
        let filename = "hachidori-screenshot-0f8fad5b-d9cb-469f-a165-70867728950e.jpg";
        let request = json!({ "screenshot": { "token": "t", "filename": filename } });
        let media =
            validate_client_media(&request, &json!({ "screenshot": { "token": "t", "filename": filename, "data": "/9j/AA==" } })).unwrap();
        assert_eq!(media.screenshot.unwrap().data, "/9j/AA==");
        assert!(validate_client_media(&request, &json!({})).unwrap_err().contains("missing media"));
        assert!(
            validate_client_media(&request, &json!({ "screenshot": { "token": "u", "filename": filename, "data": "/9j/AA==" } })).is_err()
        );
        assert!(validate_client_media(&json!({}), &json!({ "other": 1 })).unwrap_err().contains("envelope payload is invalid"));
        assert_eq!(
            validate_client_media(&json!({ "captureUnavailable": ["screenshot"], "screenshot": {} }), &json!({})).unwrap(),
            ClientMedia::default()
        );
    }

    #[test]
    fn stores_only_missing_media() {
        let name = format!("hachidori_{}.png", "a".repeat(64));
        let log = RefCell::new(Vec::new());
        let have = RefCell::new(Vec::<String>::new());
        let invoke = |action: &str, params: Value, _: Option<u64>| -> Result<Value> {
            log.borrow_mut().push(action.to_owned());
            match action {
                "getMediaFilesNames" => Ok(json!(have.borrow().clone())),
                "storeMediaFile" => {
                    have.borrow_mut().push(params["filename"].as_str().unwrap().to_owned());
                    Ok(params["filename"].clone())
                }
                _ => Err(Failure::new("unexpected")),
            }
        };
        let fields = vec![("Glossary".to_owned(), format!("<img src=\"{name}\">"))];
        let media = vec![
            DictionaryMedia { dictionary: "D".to_owned(), path: "a.png".to_owned(), filename: name.clone() },
            DictionaryMedia { dictionary: "D".to_owned(), path: "b.png".to_owned(), filename: format!("hachidori_{}.png", "b".repeat(64)) },
        ];
        let stored = prepare(&invoke, &fields, &media, None, &|_| Ok("QUJD".to_owned()), &|| Ok(())).unwrap();
        assert_eq!(stored, vec![name.clone()]);
        assert_eq!(*log.borrow(), ["getMediaFilesNames", "storeMediaFile", "getMediaFilesNames"]);
        assert_eq!(ensure(&invoke, &name, "x", &mut || unreachable!(), &|| Ok(())).unwrap(), Stored::Existing);
        let clash = vec![media[0].clone(), DictionaryMedia { path: "c.png".to_owned(), ..media[0].clone() }];
        assert!(
            prepare(&invoke, &fields, &clash, None, &|_| Ok("QUJD".to_owned()), &|| Ok(())).unwrap_err().message.contains("Two dictionary")
        );
        assert!(ensure(&invoke, "x.png", "x", &mut || unreachable!(), &|| Ok(())).is_err());
    }
}
