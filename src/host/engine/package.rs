// SPDX-License-Identifier: GPL-3.0-or-later
//! A dictionary package as `dictionaryState` names it: the browser's generation
//! paths (`dictionaryRoot`, `dictionaryFolderName`) and the entry an installed
//! package's `index.json` describes (`packageFromIndex`).

use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::host::js;

/// `DICT_ROOT`: where the browser's packages live, which their paths still name.
const DICT_ROOT: &str = "/dicts";
const GENERATION_PREFIX: &str = ".hdw-generation-";
const STAGING_DIR: &str = ".hdw-import";
const REMOVAL_DIR: &str = ".hdw-remove";
const SCAN_INDEX_MAGIC: u32 = 0x4953_4448;
const SCAN_INDEX_VERSION: u32 = 1;
const SCAN_INDEX_HEADER_BYTES: usize = 16;

/// `text`: a string, or `""`.
fn text(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or_default()
}

/// `optionalText`.
fn optional_text(value: Option<&Value>) -> Value {
    value.and_then(Value::as_str).filter(|text| !text.is_empty()).map_or(Value::Null, Value::from)
}

/// `count`: a positive whole count, or 0.
fn count(value: Option<&Value>) -> f64 {
    let number = js::to_number(value).trunc();
    if number.is_finite() && number > 0.0 { number } else { 0.0 }
}

/// `dictionaryFolderName`: the title, or a safe name with its hash where the title
/// is not one path component.
pub(crate) fn folder_name(title: &str) -> String {
    if !title.is_empty() && title != "." && title != ".." && !title.contains(['/', '\\', '\0']) {
        return title.to_owned();
    }
    let hash = title.bytes().fold(0x811c_9dc5_u32, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193));
    format!("{} #{hash:08x}", title.replace(['/', '\\', ':', '\0'], "_"))
}

/// `usableDictionaryTitle`.
pub(crate) fn usable_title(title: &str) -> bool {
    let folder = if title.is_empty() { String::new() } else { folder_name(title) };
    !folder.is_empty() && folder != STAGING_DIR && folder != REMOVAL_DIR
}

/// `GENERATION_NAME`: `.hdw-generation-` and a lowercase version 4 UUID.
fn generation_name(name: &str) -> bool {
    let Some(uuid) = name.strip_prefix(GENERATION_PREFIX) else { return false };
    let bytes = uuid.as_bytes();
    let hex = |range: std::ops::Range<usize>| bytes[range].iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b));
    bytes.len() == 36
        && [8, 13, 18, 23].iter().all(|&dash| bytes[dash] == b'-')
        && hex(0..8)
        && hex(9..13)
        && bytes[14] == b'4'
        && hex(15..18)
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
        && hex(20..23)
        && hex(24..36)
}

/// `isGenerationRoot`.
fn is_generation_root(path: &str) -> bool {
    path.strip_prefix(DICT_ROOT).and_then(|rest| rest.strip_prefix('/')).is_some_and(generation_name)
}

/// `dictionaryRoot`: the directory that holds a package, if its path is one the
/// extension would load.
pub(crate) fn root(dictionary: &Value) -> Option<&str> {
    let title = text(dictionary.get("title"));
    let path = text(dictionary.get("path"));
    let folder = if title.is_empty() { String::new() } else { folder_name(title) };
    if folder.is_empty() || folder == STAGING_DIR {
        return None;
    }
    if path.strip_prefix(DICT_ROOT).and_then(|rest| rest.strip_prefix('/')) == Some(folder.as_str()) {
        return Some(path);
    }
    let separator = path.rfind('/')?;
    let root = &path[..separator];
    (separator > DICT_ROOT.len() && path[separator + 1..] == folder && is_generation_root(root)).then_some(root)
}

/// `isGenerationRoot(dictionaryRoot(dictionary) ?? "")`.
pub(crate) fn in_generation(dictionary: &Value) -> bool {
    root(dictionary).is_some_and(is_generation_root)
}

/// `createGenerationRoot`: a new generation's path.
pub(crate) fn new_generation_root() -> Result<String, String> {
    Ok(format!("{DICT_ROOT}/{GENERATION_PREFIX}{}", crate::host::settings::worker::random_uuid()?))
}

/// `stableDictionaryId`: the first half of the title's SHA-256, in hex.
pub(crate) fn stable_id(title: &str) -> String {
    crate::host::settings::hex(&Sha256::digest(title.as_bytes())[..16])
}

/// `longKeyLengthFromScanIndex`: the long-key window `scan.idx` declares, or 0.
fn long_key_length(dir: &Path) -> u16 {
    let Ok(bytes) = fs::read(dir.join("scan.idx")) else { return 0 };
    let Some(header) = bytes.get(..SCAN_INDEX_HEADER_BYTES) else { return 0 };
    let word = |at: usize| u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]]);
    if word(0) != SCAN_INDEX_MAGIC || word(4) != SCAN_INDEX_VERSION {
        return 0;
    }
    u16::from_le_bytes([header[12], header[13]])
}

/// `installedAt`: the import date, or else when `index.json` was written.
fn installed_at(import_date: Option<&Value>, dir: &Path) -> Result<String, String> {
    let time = if let Some(time) = import_date.and_then(Value::as_f64) {
        time
    } else {
        let modified = fs::metadata(dir.join("index.json")).and_then(|meta| meta.modified()).map_err(|error| error.to_string())?;
        let since = modified.duration_since(std::time::UNIX_EPOCH).map_err(|error| error.to_string())?;
        #[allow(clippy::cast_precision_loss, reason = "milliseconds since 1970 fit an f64 exactly")]
        let time = since.as_millis() as f64;
        time
    };
    js::iso_time(time).ok_or_else(|| "Invalid time value".to_owned())
}

/// `packageFromIndex`: the `dictionaryState` entry for the package installed in
/// `dir` as generation path `path`.
pub(crate) fn from_index(dir: &Path, path: &str) -> Result<Value, String> {
    let label = format!("{path}/index.json");
    let index = fs::read_to_string(dir.join("index.json")).map_err(|error| format!("{label}: {error}"))?;
    let index: Value = serde_json::from_str(&index).map_err(|error| format!("{label} is not valid JSON: {error}"))?;
    let title = text(index.get("title"));
    if title.is_empty() {
        return Err(format!("{label} has no dictionary title"));
    }
    let counts = |pointer: &str| count(index.pointer(pointer));
    Ok(json!({
        "id": stable_id(title),
        "title": title,
        "displayName": null,
        "path": path,
        "enabled": true,
        "favorite": false,
        "revision": text(index.get("revision")),
        "isUpdatable": index.get("isUpdatable") == Some(&Value::Bool(true)),
        "indexUrl": optional_text(index.get("indexUrl")),
        "downloadUrl": optional_text(index.get("downloadUrl")),
        "language": optional_text(index.get("sourceLanguage")),
        "frequencyMode": optional_text(index.get("frequencyMode")),
        "termCount": js::number(counts("/counts/terms/total")),
        "frequencyCount": js::number(counts("/counts/termMeta/freq") + counts("/counts/kanjiMeta/freq")),
        "pitchCount": js::number(counts("/counts/termMeta/pitch") + counts("/counts/termMeta/ipa")),
        "kanjiCount": js::number(counts("/counts/kanji/total")),
        "mediaCount": js::number(counts("/counts/media/total")),
        "longKeyLength": long_key_length(dir),
        "installedAt": installed_at(index.get("importDate"), dir)?,
        "lastUpdateCheck": null,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_and_roots_follow_the_extension() {
        assert_eq!(folder_name("JMdict"), "JMdict");
        assert_eq!(folder_name("a/b"), "a_b #3a8e75c1");
        assert_eq!(folder_name(".."), ".. #a3d4a70d");
        assert!(!usable_title("") && !usable_title(".hdw-import") && usable_title("x"));
        let uuid = "/dicts/.hdw-generation-0b0e8a8e-1c2d-4e5f-8a9b-0c1d2e3f4a5b";
        let entry = |title: &str, path: &str| json!({ "title": title, "path": path });
        assert_eq!(root(&entry("JMdict", &format!("{uuid}/JMdict"))), Some(uuid));
        assert!(in_generation(&entry("JMdict", &format!("{uuid}/JMdict"))));
        assert_eq!(root(&entry("JMdict", "/dicts/JMdict")), Some("/dicts/JMdict"));
        assert!(!in_generation(&entry("JMdict", "/dicts/JMdict")));
        assert_eq!(root(&entry("JMdict", &format!("{uuid}/Other"))), None);
        assert_eq!(root(&entry("JMdict", "/dicts/.hdw-generation-x/JMdict")), None);
        assert_eq!(root(&entry("", "/dicts/")), None);
        assert!(generation_name(&new_generation_root().unwrap_or_else(|error| panic!("{error}"))["/dicts/".len()..]));
        assert_eq!(stable_id("Hachidori Custom Dictionary").len(), 32);
    }
}
