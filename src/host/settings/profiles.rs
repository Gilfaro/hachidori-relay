// SPDX-License-Identifier: GPL-3.0-or-later
//! Records with language profiles (docs/languages.md 4 and 5 upstream): the
//! profile layer of `reader-options.js` (reading a profile's options, the one
//! options writer, a profile's view of `dictionaryState`) and `profile-state.js`
//! (the conversion between the single-profile shape and records with profiles,
//! the merges into a schema-2 state, the Japanese view, the held languages).
//!
//! `tests` replays cases recorded from the extension's own code
//! (`scripts/profile-cases.mjs`).

use std::collections::{BTreeSet, HashSet};
use std::sync::OnceLock;

use serde_json::{Map, Value, json};

use super::custom::{CUSTOM_DICTIONARY_ID, personal_language};
use super::options::{
    defaults, normalise_anki, normalise_anki_connect_url, normalise_experimental, normalise_flat, project_option_keys, project_stored,
    valid_identifier,
};
use crate::host::js::{self, plain_object};
use crate::host::profile::{JAPANESE, language_code};
use crate::host::registry;

/// `PROFILE_MESSAGES.noJapaneseProfile`.
pub(crate) const NO_JAPANESE_PROFILE: &str =
    "This Hachidori has no Japanese profile. Add Japanese in its Settings, or update the linked browser.";
/// `PROFILE_MESSAGES.profileRemoved`.
pub(crate) const PROFILE_REMOVED: &str = "This language was removed in another Hachidori page.";
/// `applyOptionsPatch`'s refusal of a profile the record lacks.
pub(crate) const NO_SUCH_PROFILE: &str = "the options write names a profile that does not exist";

/// `GLOBAL_OPTION_KEYS`: the keys of the reader's hands, screen and machine.
pub(crate) const GLOBAL_OPTION_KEYS: [&str; 37] = [
    "hoverEnabled",
    "googleDocs",
    "lookupMode",
    "activationKey",
    "definitionLookupMode",
    "scanDelayMs",
    "definitionScanDelayMs",
    "popupHideDelayMs",
    "hidePopupOnCursorExit",
    "hidePopupOnCursorExitDelayMs",
    "popupNestingMaxDepth",
    "popupToolbarPosition",
    "customPopupCss",
    "userScripts",
    "smallerAnkiCards",
    "youtubeMining",
    "experimental",
    "videoAudioFormat",
    "videoAudioBitrate",
    "videoImageFormat",
    "videoImageQuality",
    "popupWidthPx",
    "popupHeightPx",
    "popupScalePercent",
    "sourceHighlightEnabled",
    "automaticBackupDays",
    "useLessRamByDefault",
    "lowMemoryMode",
    "dictionaryEntryStorage",
    "dictionaryIndexStorage",
    "showToolbarSearchButton",
    "searchClickLookup",
    "searchWindowWidthPx",
    "searchWindowHeightPx",
    "keybinds",
    "searchHistoryEnabled",
    "searchHistoryLimit",
];
/// `GLOBAL_RETIRED_KEYS`: retired keys that migrate into a global key.
const GLOBAL_RETIRED_KEYS: [&str; 3] = ["modifier", "hoverDelayMs", "customPopupJavascript"];
/// `HAN_SCRIPTS`: what a `Hani` requirement matches.
const HAN_SCRIPTS: [&str; 6] = ["Hani", "Hans", "Hant", "Hanb", "Jpan", "Kore"];
/// `DICTIONARY_STATE_PROFILES_SCHEMA`.
pub(crate) const PROFILES_SCHEMA: u64 = 2;
/// `DICTIONARY_SELECTION_KEYS`: selections naming the profile's own dictionaries.
const DICTIONARY_SELECTION_KEYS: [&str; 6] = [
    "frequencyDictionary",
    "kanjiClickDictionary",
    "compactDefinitionSummaryDictionary",
    "pitchAccentFuriganaDictionary",
    "definitionBlurFrequencyDictionary",
    "popupImageSource",
];

/// `OPTION_REQUIREMENTS`: what each key needs of the language, and its value
/// for a profile that lacks it.
fn option_requirements() -> [(&'static str, &'static str, Value); 16] {
    [
        ("showPitchAccentFurigana", "pitchAccent", json!(false)),
        ("pitchAccentFuriganaDictionary", "pitchAccent", json!("")),
        ("pitchAccentFuriganaStyle", "pitchAccent", json!("contour")),
        ("showPitchAccentBadge", "pitchAccent", json!(false)),
        ("showPitchAccentDictionaryNames", "pitchAccent", json!(false)),
        ("showPitchAccentText", "pitchAccent", json!(false)),
        ("showPitchAccentPosition", "pitchAccent", json!(false)),
        ("showPitchAccentGraph", "pitchAccent", json!(false)),
        ("showPitchAccentColors", "pitchAccent", json!(false)),
        ("kanjiClickDictionary", "characterView", json!("")),
        ("wordHighlightEnabled", "wordHighlighting", json!(false)),
        ("wordHighlightUnknown", "wordHighlighting", json!(true)),
        ("wordHighlightLearning", "wordHighlighting", json!(true)),
        ("wordHighlightKnown", "wordHighlighting", json!(false)),
        ("wordHighlightIgnored", "wordHighlighting", json!(false)),
        ("wordHighlightStyle", "wordHighlighting", json!("underline")),
    ]
}

/// `PROFILE_OPTION_KEYS`: every option key but the global ones and `anki`.
pub(crate) fn profile_option_keys() -> &'static [&'static str] {
    static KEYS: OnceLock<Vec<&'static str>> = OnceLock::new();
    KEYS.get_or_init(|| defaults().keys().map(String::as_str).filter(|key| *key != "anki" && !GLOBAL_OPTION_KEYS.contains(key)).collect())
}

fn is_profile_key(key: &str) -> bool {
    profile_option_keys().contains(&key)
}

fn global_and_retired() -> impl Iterator<Item = &'static str> {
    GLOBAL_OPTION_KEYS.into_iter().chain(GLOBAL_RETIRED_KEYS)
}

/// `pick(source, keys)`.
fn pick<'a>(source: &Map<String, Value>, keys: impl IntoIterator<Item = &'a str>) -> Map<String, Value> {
    keys.into_iter().filter_map(|key| source.get(key).map(|value| (key.to_owned(), value.clone()))).collect()
}

fn object(value: Option<&Value>) -> Map<String, Value> {
    plain_object(value).cloned().unwrap_or_default()
}

/// `{ ...value }`: an object's own entries, an array's or a string's by index.
fn spread(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map.clone(),
        Value::Array(items) => items.iter().enumerate().map(|(index, item)| (index.to_string(), item.clone())).collect(),
        Value::String(text) => text
            .encode_utf16()
            .enumerate()
            .map(|(index, unit)| (index.to_string(), Value::from(String::from_utf16_lossy(&[unit]))))
            .collect(),
        _ => Map::new(),
    }
}

/// `{ id: row.id, enabled }`, without `id` where the row has none.
fn entry(id: Option<&Value>, enabled: bool) -> Value {
    let mut entry = Map::new();
    if let Some(id) = id {
        entry.insert("id".to_owned(), id.clone());
    }
    entry.insert("enabled".to_owned(), Value::Bool(enabled));
    Value::Object(entry)
}

fn array(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// `revisionOf` / `storedRevision`: a non-negative integer revision, else 0.
pub(crate) fn stored_revision(record: Option<&Value>) -> Value {
    js::number(js::whole(plain_object(record).and_then(|record| record.get("revision"))).unwrap_or(0.0))
}

/// `hasProfiles`.
pub(crate) fn has_profiles(options: Option<&Value>) -> bool {
    plain_object(options).is_some_and(|options| options.get("profiles").is_some_and(Value::is_array))
}

/// `ankiConnection`: the top-level `anki` of a record with profiles.
fn anki_connection(value: Option<&Value>) -> Option<Map<String, Value>> {
    let source = plain_object(value)?;
    let mut connection = Map::new();
    if let Some(url) = source.get("url") {
        connection.insert("url".to_owned(), Value::from(normalise_anki_connect_url(Some(url)).unwrap_or_default()));
    }
    let api_key = source.get("apiKey").filter(|key| key.is_string()).cloned().unwrap_or_else(|| Value::from(""));
    connection.insert("apiKey".to_owned(), api_key);
    Some(connection)
}

/// `{ templates: normaliseAnki({ templates }).templates }`.
fn normalised_templates(templates: Option<&Value>) -> Value {
    let mut source = Map::new();
    if let Some(templates) = templates {
        source.insert("templates".to_owned(), templates.clone());
    }
    normalise_anki(Some(&Value::Object(source)))["templates"].clone()
}

/// `ankiTemplates`: a profile's own `anki`, its Templates only.
fn anki_templates(value: Option<&Value>) -> Option<Value> {
    let source = plain_object(value)?;
    Some(json!({ "templates": normalised_templates(source.get("templates")) }))
}

/// `normaliseProfile`: a stored profile with anything malformed dropped.
fn normalise_profile(value: &Value) -> Option<Value> {
    let source = value.as_object()?;
    let id = valid_identifier(source.get("id"))?;
    let language = source.get("language").and_then(Value::as_str).filter(|code| language_code(code))?;
    let settings = object(source.get("settings"));
    let sparse = Value::Object(pick(&settings, profile_option_keys().iter().copied().chain(["definitionBlurEnabled"])));
    let projected = project_option_keys(Some(&sparse), false).unwrap_or_default();
    let mut kept = pick(&projected, profile_option_keys().iter().copied());
    if let Some(anki) = anki_templates(settings.get("anki")) {
        kept.insert("anki".to_owned(), anki);
    }
    Some(json!({ "id": id, "language": language, "settings": kept, "packSettings": object(source.get("packSettings")) }))
}

/// `storedProfiles`: the stored list, each profile normalised, duplicates dropped.
fn stored_profiles(options: &Value) -> Vec<Value> {
    let mut ids = HashSet::new();
    array(options.get("profiles"))
        .iter()
        .filter_map(normalise_profile)
        .filter(|profile| ids.insert(profile["id"].clone().to_string()))
        .collect()
}

/// `profileSettingsOf`: a flat record's own profile keys.
fn profile_settings_of(flat: &Map<String, Value>) -> Map<String, Value> {
    let mut settings = pick(flat, profile_option_keys().iter().copied());
    if let Some(anki) = flat.get("anki") {
        settings.insert("anki".to_owned(), json!({ "templates": normalise_anki(Some(anki))["templates"] }));
    }
    settings
}

/// `profiles`: a record without `profiles` is one Japanese profile.
pub(crate) fn profiles(options: Option<&Value>) -> Vec<Value> {
    match options {
        Some(options) if has_profiles(Some(options)) => stored_profiles(options),
        _ => {
            let flat = project_option_keys(options, false).unwrap_or_default();
            vec![json!({ "id": JAPANESE, "language": JAPANESE, "settings": profile_settings_of(&flat), "packSettings": {} })]
        }
    }
}

fn find_profile(options: Option<&Value>, profile_id: &str) -> Option<Value> {
    profiles(options).into_iter().find(|profile| profile["id"] == profile_id)
}

/// `flatProfileRecord`: global keys from the top level, the profile's keys, `anki` joined.
pub(crate) fn flat_profile_record(options: Option<&Value>, profile_id: &str) -> Map<String, Value> {
    let source = object(options);
    let mut flat = pick(&source, global_and_retired());
    let connection = anki_connection(source.get("anki"));
    if let Some(profile) = find_profile(options, profile_id) {
        let mut settings = object(profile.get("settings"));
        let profile_anki = settings.remove("anki");
        flat.extend(settings);
        if connection.is_some() || profile_anki.is_some() {
            let mut anki = connection.unwrap_or_default();
            if let Some(profile_anki) = profile_anki {
                anki.insert("templates".to_owned(), profile_anki["templates"].clone());
            }
            flat.insert("anki".to_owned(), Value::Object(anki));
        }
    } else if let Some(connection) = connection {
        flat.insert("anki".to_owned(), Value::Object(connection));
    }
    flat
}

/// `languageDescriptor`.
fn descriptor(language: Option<&str>) -> Option<&'static Value> {
    registry::get(language?)
}

fn pack_setting(descriptor: Option<&Value>, matches: impl Fn(&Value) -> bool) -> Option<&Value> {
    array(descriptor.and_then(|descriptor| descriptor.get("settings"))).iter().find(|setting| matches(setting))
}

/// `validPackSettingValue`.
fn valid_pack_setting_value(setting: &Value, value: Option<&Value>) -> bool {
    match setting.get("type").and_then(Value::as_str) {
        Some("boolean") => value.is_some_and(Value::is_boolean),
        Some("enum") => setting
            .get("values")
            .and_then(Value::as_array)
            .is_some_and(|values| values.iter().any(|entry| js::strict_eq(entry.as_object().and_then(|entry| entry.get("id")), value))),
        Some("number") => value.and_then(Value::as_f64).is_some_and(|number| {
            number.is_finite()
                && setting.get("min").and_then(Value::as_f64).is_none_or(|min| number >= min)
                && setting.get("max").and_then(Value::as_f64).is_none_or(|max| number <= max)
        }),
        _ => false,
    }
}

/// `packSettingValue`: the stored value when valid for the pack's setting, else its default.
pub(crate) fn pack_setting_value(descriptor: Option<&Value>, pack_settings: &Map<String, Value>, id: &str) -> Option<Value> {
    let setting = pack_setting(descriptor, |setting| setting.get("id").and_then(Value::as_str) == Some(id))?;
    let value = pack_settings.get(id);
    if valid_pack_setting_value(setting, value) { value.cloned() } else { setting.get("default").cloned() }
}

/// `languageCapability`.
fn language_capability(descriptor: &Value, name: &str, pack_settings: &Map<String, Value>) -> bool {
    let value = descriptor.get("capabilities").and_then(|capabilities| capabilities.get(name));
    if name == "readings" {
        return value.is_some_and(|value| value == "required" || value == "optional");
    }
    match value {
        None | Some(Value::Null | Value::Bool(false)) => false,
        Some(Value::Object(value)) if value.get("optional") == Some(&Value::Bool(true)) => {
            let setting =
                pack_setting(Some(descriptor), |setting| setting.pointer("/effect/capability").and_then(Value::as_str) == Some(name));
            setting
                .and_then(|setting| setting.get("id"))
                .and_then(Value::as_str)
                .is_some_and(|id| pack_setting_value(Some(descriptor), pack_settings, id) == Some(Value::Bool(true)))
        }
        Some(_) => true,
    }
}

/// `optionRequirementMet`: a capability name, `{scripts: [...]}` or
/// `"ankiNoteTypeFamilies"`. Japanese meets every requirement; a language this
/// build has no pack for meets none.
pub(crate) fn option_requirement_met(requirement: &Value, language: Option<&str>, pack_settings: &Map<String, Value>) -> bool {
    let Some(descriptor) = descriptor(language) else {
        return language == Some(JAPANESE);
    };
    if requirement == "ankiNoteTypeFamilies" {
        return descriptor.pointer("/host/anki/noteTypeFamilies").and_then(Value::as_array).is_some_and(|families| !families.is_empty());
    }
    if let Some(wanted) = requirement.get("scripts").and_then(Value::as_array) {
        let scripts = array(descriptor.get("scripts"));
        return wanted.iter().any(|wanted| {
            scripts
                .iter()
                .any(|script| script == wanted || (wanted == "Hani" && script.as_str().is_some_and(|script| HAN_SCRIPTS.contains(&script))))
        });
    }
    requirement.as_str().is_some_and(|name| language_capability(descriptor, name, pack_settings))
}

/// `maskOptions`.
fn mask_options(options: &mut Map<String, Value>, language: Option<&str>, pack_settings: &Map<String, Value>) {
    for (key, requires, neutral) in option_requirements() {
        if !option_requirement_met(&Value::from(requires), language, pack_settings) {
            options.insert(key.to_owned(), neutral);
        }
    }
}

/// `projectOptions`: flat options for one profile, shaped like `normaliseOptions`'
/// result, masked by the language's capabilities unless `masked` is false. A
/// profile the record lacks reads as the global keys over the defaults.
pub(crate) fn project_options(options: Option<&Value>, profile_id: &str, masked: bool) -> Map<String, Value> {
    if !has_profiles(options) {
        if profile_id == JAPANESE {
            return normalise_flat(options);
        }
        let mut flat = normalise_flat(Some(&Value::Object(pick(&object(options), global_and_retired()))));
        if masked {
            mask_options(&mut flat, None, &Map::new());
        }
        return flat;
    }
    let profile = find_profile(options, profile_id);
    let mut flat = normalise_flat(Some(&Value::Object(flat_profile_record(options, profile_id))));
    if masked {
        let language = profile.as_ref().and_then(|profile| profile["language"].as_str());
        mask_options(&mut flat, language, &object(profile.as_ref().and_then(|profile| profile.get("packSettings"))));
    }
    flat
}

/// `profileDescriptor`: `{ id, language, packSettings }`.
pub(crate) fn profile_descriptor(options: Option<&Value>, profile_id: &str) -> Option<Value> {
    let profile = find_profile(options, profile_id)?;
    Some(json!({ "id": profile["id"], "language": profile["language"], "packSettings": profile["packSettings"] }))
}

/// `storedGlobals`: the normalised top level of a record with profiles.
fn stored_globals(options: Option<&Value>) -> Map<String, Value> {
    let source = object(options);
    let sparse = Value::Object(pick(&source, global_and_retired()));
    let mut globals = pick(&project_option_keys(Some(&sparse), false).unwrap_or_default(), GLOBAL_OPTION_KEYS);
    if let Some(connection) = anki_connection(source.get("anki")) {
        globals.insert("anki".to_owned(), Value::Object(connection));
    }
    globals
}

/// `patchHoldsProfileKeys`.
pub(crate) fn patch_holds_profile_keys(patch: &Map<String, Value>) -> bool {
    patch.keys().any(|key| key == "anki" || is_profile_key(key))
}

/// `applyOptionsPatch`: the one options writer. Applies a validated flat patch
/// to the stored, unmasked record, keeping its revision: global keys to the top
/// level, profile keys to the named profile, `anki` split.
pub(crate) fn apply_options_patch(
    options: Option<&Value>,
    patch: &Map<String, Value>,
    profile_id: &str,
) -> Result<Map<String, Value>, String> {
    let revision = stored_revision(options);
    let Some(record) = options.filter(|options| has_profiles(Some(options))) else {
        if profile_id != JAPANESE && patch_holds_profile_keys(patch) {
            return Err(NO_SUCH_PROFILE.to_owned());
        }
        let mut next = project_option_keys(options, false).unwrap_or_default();
        next.insert("revision".to_owned(), revision);
        next.extend(patch.clone());
        return Ok(next);
    };
    let mut list = stored_profiles(record);
    let index = list.iter().position(|profile| profile["id"] == profile_id);
    if index.is_none() && patch_holds_profile_keys(patch) {
        return Err(NO_SUCH_PROFILE.to_owned());
    }
    let mut top = stored_globals(options);
    let mut settings = index.map(|index| object(list[index].get("settings")));
    for (key, value) in patch {
        if key == "anki" {
            top.insert("anki".to_owned(), anki_connection(Some(value)).map_or(Value::Null, Value::Object));
            if let Some(settings) = settings.as_mut() {
                settings.insert("anki".to_owned(), anki_templates(Some(value)).unwrap_or(Value::Null));
            }
        } else if GLOBAL_OPTION_KEYS.contains(&key.as_str()) {
            top.insert(key.clone(), value.clone());
        } else if is_profile_key(key)
            && let Some(settings) = settings.as_mut()
        {
            settings.insert(key.clone(), value.clone());
        }
    }
    if let (Some(index), Some(settings)) = (index, settings) {
        list[index]["settings"] = Value::Object(settings);
    }
    top.insert("revision".to_owned(), revision);
    top.insert("profiles".to_owned(), Value::Array(list));
    Ok(top)
}

/// `applyPackSettingsPatch`: ids the language's pack declares must carry valid
/// values; ids the patch omits keep theirs; an id the pack does not declare is
/// kept as sent.
pub(crate) fn apply_pack_settings_patch(options: Option<&Value>, pack_patch: Option<&Value>, profile_id: &str) -> Result<Value, String> {
    let patch = plain_object(pack_patch).ok_or("the pack settings write carried no object")?;
    if patch.is_empty() {
        return Ok(options.cloned().unwrap_or(Value::Null));
    }
    let Some(record) = options.filter(|options| has_profiles(Some(options))) else {
        return Err("This language has no settings of its own.".to_owned());
    };
    let mut list = stored_profiles(record);
    let index = list.iter().position(|profile| profile["id"] == profile_id).ok_or(NO_SUCH_PROFILE)?;
    let descriptor = descriptor(list[index]["language"].as_str());
    for (id, value) in patch {
        let setting = pack_setting(descriptor, |setting| setting.get("id").and_then(Value::as_str) == Some(id));
        if setting.is_some_and(|setting| !valid_pack_setting_value(setting, Some(value))) {
            return Err(format!("the pack setting {} carried an invalid value", Value::from(id.as_str())));
        }
    }
    let mut pack_settings = object(list[index].get("packSettings"));
    pack_settings.extend(patch.clone());
    list[index]["packSettings"] = Value::Object(pack_settings);
    let mut next = apply_options_patch(options, &Map::new(), profile_id)?;
    next.insert("profiles".to_owned(), Value::Array(list));
    Ok(Value::Object(next))
}

/// `hasDictionaryProfiles`: a schema-2 state.
pub(crate) fn has_dictionary_profiles(state: Option<&Value>) -> bool {
    plain_object(state).is_some_and(|state| {
        state.get("schemaVersion").and_then(Value::as_f64) == Some(2.0) && state.get("profiles").is_some_and(Value::is_object)
    })
}

/// `packageWithEnabled`.
pub(crate) fn package_with_enabled(row: &Value, enabled: bool) -> Value {
    let mut row = spread(row);
    row.insert("enabled".to_owned(), Value::Bool(enabled));
    Value::Object(row)
}

fn enabled_flag(entry: &Value) -> bool {
    entry.get("enabled") != Some(&Value::Bool(false))
}

/// `{ schemaVersion, revision, ... }` with the revision as the state stores it
/// (left out where it has none, as `JSON.stringify` leaves out `undefined`).
fn with_revision(schema: u64, state: &Value, mut rest: Map<String, Value>) -> Value {
    rest.insert("schemaVersion".to_owned(), Value::from(schema));
    if let Some(revision) = state.get("revision") {
        rest.insert("revision".to_owned(), revision.clone());
    }
    Value::Object(rest)
}

/// `projectDictionaryState`: the schema-1 view of one profile; `None` when the
/// state has no such profile.
pub(crate) fn project_dictionary_state(state: Option<&Value>, profile_id: &str) -> Option<Value> {
    let state = state.filter(|state| !state.is_null())?;
    if !has_dictionary_profiles(Some(state)) {
        return (profile_id == JAPANESE).then(|| state.clone());
    }
    let profile = state["profiles"].get(profile_id).filter(|profile| profile.is_object())?;
    let rows: Vec<&Value> = array(state.get("dictionaries")).iter().filter(|row| row.is_object()).collect();
    let package = |id: Option<&Value>| rows.iter().rev().find(|row| js::strict_eq(row.get("id"), id)).copied();
    let dictionaries: Vec<Value> = array(profile.get("dictionaries"))
        .iter()
        .filter_map(|entry| {
            package(entry.as_object().and_then(|entry| entry.get("id"))).map(|row| package_with_enabled(row, enabled_flag(entry)))
        })
        .collect();
    let groups = profile.get("groups").filter(|groups| groups.is_array()).cloned().unwrap_or_else(|| json!([]));
    let mut rest = Map::new();
    rest.insert("dictionaries".to_owned(), Value::Array(dictionaries));
    rest.insert("groups".to_owned(), groups);
    Some(with_revision(1, state, rest))
}

/// `allPackages`: every package once, `enabled` meaning "enabled in any profile".
pub(crate) fn all_packages(state: Option<&Value>) -> Vec<Value> {
    let Some(state) = state.filter(|state| !state.is_null()) else {
        return Vec::new();
    };
    let rows = array(state.get("dictionaries"));
    if !has_dictionary_profiles(Some(state)) {
        return rows.to_vec();
    }
    let enabled: Vec<Option<&Value>> = state["profiles"]
        .as_object()
        .into_iter()
        .flat_map(Map::values)
        .flat_map(|profile| array(profile.get("dictionaries")))
        .filter(|entry| enabled_flag(entry))
        .map(|entry| entry.as_object().and_then(|entry| entry.get("id")))
        .collect();
    rows.iter()
        .filter(|row| row.is_object())
        .map(|row| package_with_enabled(row, enabled.iter().any(|id| js::strict_eq(*id, row.get("id")))))
        .collect()
}

/// `LANGUAGE_SELECTION_KEY`.
pub(crate) const LANGUAGE_SELECTION_KEY: &str = "languageSelection";

/// `normaliseLanguageSelection`.
pub(crate) fn normalise_language_selection(value: Option<&Value>) -> Value {
    let source = object(value);
    json!({
        "revision": stored_revision(value),
        "activeProfileId": valid_identifier(source.get("activeProfileId")),
        "autoSwitch": source.get("autoSwitch") == Some(&Value::Bool(true)),
    })
}

/// `languagesEnabled`: Advanced → Experimental features → Multiple languages.
#[cfg_attr(not(test), expect(dead_code, reason = "only the extension's toolbar and pages read it; the replayed cases test it"))]
pub(crate) fn languages_enabled(options: Option<&Value>) -> bool {
    normalise_experimental(plain_object(options).and_then(|options| options.get("experimental")))["multipleLanguages"] == true
}

/// `activeProfileId`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "only the extension's pages and its local language selection read it; the replayed cases test it")
)]
pub(crate) fn active_profile_id(options: Option<&Value>, selection: Option<&Value>) -> String {
    let list = profiles(options);
    let chosen = if languages_enabled(options) { normalise_language_selection(selection)["activeProfileId"].clone() } else { Value::Null };
    if chosen.is_string() && list.iter().any(|profile| profile["id"] == chosen) {
        return chosen.as_str().unwrap_or(JAPANESE).to_owned();
    }
    let japanese = list.iter().find(|profile| profile["id"] == JAPANESE).or_else(|| list.first());
    japanese.and_then(|profile| profile["id"].as_str()).unwrap_or(JAPANESE).to_owned()
}

// ---------------------------------------------------------------------------
// profile-state.js

/// `packageRow`: a package row as schema 2 stores it, without `enabled`.
pub(crate) fn package_row(row: &Value) -> Value {
    let mut row = spread(row);
    row.remove("enabled");
    Value::Object(row)
}

/// `isPersonalPackage`.
pub(crate) fn is_personal_package(row: &Value) -> bool {
    personal_language(row).is_some()
}

/// `optionsWithProfiles`: the record a single-profile record becomes when a
/// second profile is added; every value is kept, and the revision.
pub(crate) fn options_with_profiles(options: Option<&Value>) -> Value {
    if let Some(options) = options.filter(|options| has_profiles(Some(options))) {
        return options.clone();
    }
    let flat = project_stored(options);
    let mut top = pick(&flat, GLOBAL_OPTION_KEYS);
    if let Some(anki) = flat.get("anki") {
        top.insert("anki".to_owned(), Value::Object(pick(&object(Some(anki)), ["url", "apiKey"])));
    }
    top.insert("revision".to_owned(), stored_revision(options));
    top.insert("profiles".to_owned(), Value::Array(profiles(options)));
    Value::Object(top)
}

/// `optionsWithoutProfiles`: the single-profile record a record with only the
/// Japanese profile returns to.
pub(crate) fn options_without_profiles(record: Option<&Value>) -> Value {
    if !has_profiles(record) {
        return record.cloned().unwrap_or(Value::Null);
    }
    let mut flat = project_stored(record);
    flat.insert("revision".to_owned(), stored_revision(record));
    Value::Object(flat)
}

/// `applyToEveryProfile`: a flat patch laid over every profile, global keys at
/// the top level. The revision is kept.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "only a linked browser's overlay lays its keys over every profile (upstream background-sharing.js); the relay host is never one, so the profiles oracle alone runs it"
    )
)]
pub(crate) fn apply_to_every_profile(record: Option<&Value>, patch: &Map<String, Value>) -> Result<Value, String> {
    let globals = pick(patch, GLOBAL_OPTION_KEYS);
    let mut next = Value::Object(apply_options_patch(record, &globals, JAPANESE)?);
    for profile in profiles(Some(&next)) {
        let id = profile["id"].as_str().unwrap_or_default().to_owned();
        next = Value::Object(apply_options_patch(Some(&next), patch, &id)?);
    }
    Ok(next)
}

/// `newProfileSettings`: a copy of another profile's settings with the
/// language-bound ones replaced (docs/languages.md 6.3).
pub(crate) fn new_profile_settings(source: Option<&Value>, descriptor: Option<&Value>, mut new_id: impl FnMut() -> String) -> Value {
    let mut settings = object(source);
    for key in DICTIONARY_SELECTION_KEYS {
        settings.remove(key);
    }
    let templates: Vec<Value> =
        array(Some(&normalised_templates(settings.get("anki").and_then(|anki| anki.as_object()).and_then(|anki| anki.get("templates")))))
            .iter()
            .map(|template| {
                let mut template = object(Some(template));
                template.insert("id".to_owned(), Value::from(new_id()));
                template.insert("deck".to_owned(), Value::from(""));
                Value::Object(template)
            })
            .collect();
    settings.insert("anki".to_owned(), json!({ "templates": templates }));
    let speech =
        if descriptor.and_then(|descriptor| descriptor.pointer("/capabilities/readings")).is_some_and(|readings| readings == "none") {
            "text-to-speech"
        } else {
            "text-to-speech-reading"
        };
    settings.insert("audioSources".to_owned(), json!([{ "id": "default-tts", "type": speech, "enabled": true, "url": "", "voice": "" }]));
    settings.insert("customButtons".to_owned(), json!([]));
    settings.insert("customLinks".to_owned(), json!([]));
    Value::Object(settings)
}

/// `packSettingDefaults`: the pack's own settings at their defaults.
pub(crate) fn pack_setting_defaults(descriptor: Option<&Value>) -> Map<String, Value> {
    array(descriptor.and_then(|descriptor| descriptor.get("settings")))
        .iter()
        .filter_map(|setting| Some((setting.get("id")?.as_str()?.to_owned(), setting.get("default")?.clone())))
        .collect()
}

/// `dictionaryStateWithProfiles`: the schema-2 state a schema-1 state becomes.
pub(crate) fn dictionary_state_with_profiles(state: Option<&Value>) -> Value {
    if let Some(state) = state.filter(|state| has_dictionary_profiles(Some(state))) {
        return state.clone();
    }
    let rows: Vec<&Value> = array(state.and_then(|state| state.get("dictionaries"))).iter().filter(|row| row.is_object()).collect();
    let groups = state.and_then(|state| state.get("groups")).filter(|groups| groups.is_array()).cloned().unwrap_or_else(|| json!([]));
    json!({
        "schemaVersion": PROFILES_SCHEMA,
        "revision": stored_revision(state),
        "dictionaries": rows.iter().map(|row| package_row(row)).collect::<Vec<_>>(),
        "profiles": { JAPANESE: {
            "dictionaries": rows.iter().map(|row| entry(row.get("id"), enabled_flag(row))).collect::<Vec<_>>(),
            "groups": groups,
        } },
    })
}

/// `singleProfileRow`: a row without `targetLanguage`, which only records with
/// profiles carry.
pub(crate) fn single_profile_row(row: &Value) -> Value {
    let mut row = spread(row);
    row.remove("targetLanguage");
    Value::Object(row)
}

/// `japaneseDictionaryView` (and `dictionaryStateWithoutProfiles`): the schema-1
/// view code that predates profiles reads. A schema-1 state is its own view.
pub(crate) fn japanese_dictionary_view(state: Option<&Value>) -> Option<Value> {
    let Some(state) = state.filter(|state| has_dictionary_profiles(Some(state))) else {
        return state.cloned();
    };
    let rows: Vec<&Value> = array(state.get("dictionaries")).iter().filter(|row| row.is_object()).collect();
    let japanese = project_dictionary_state(Some(state), JAPANESE);
    let mut dictionaries = Vec::new();
    if let Some(personal) = rows.iter().find(|row| row["id"] == CUSTOM_DICTIONARY_ID) {
        dictionaries.push(package_with_enabled(personal, true));
    }
    let japanese_rows = japanese.as_ref().map_or(&[][..], |view| array(view.get("dictionaries")));
    dictionaries.extend(japanese_rows.iter().filter(|row| row["id"] != CUSTOM_DICTIONARY_ID).cloned());
    let listed: Vec<Option<Value>> = dictionaries.iter().map(|row| row.get("id").cloned()).collect();
    for row in &rows {
        if !listed.iter().any(|id| js::strict_eq(id.as_ref(), row.get("id"))) && !is_personal_package(row) {
            dictionaries.push(package_with_enabled(row, false));
        }
    }
    let groups = japanese.as_ref().and_then(|view| view.get("groups")).cloned().unwrap_or_else(|| json!([]));
    let mut rest = Map::new();
    rest.insert("dictionaries".to_owned(), Value::Array(dictionaries.iter().map(single_profile_row).collect()));
    rest.insert("groups".to_owned(), groups);
    Some(with_revision(1, state, rest))
}

/// `withStoredTargetLanguage`: a view written by code that does not carry
/// `targetLanguage` keeps the stored one.
fn with_stored_target_language(row: Value, stored: Option<&Value>) -> Value {
    let Some(target) = stored.and_then(|stored| stored.get("targetLanguage")) else {
        return row;
    };
    let mut row = object(Some(&row));
    row.entry("targetLanguage").or_insert_with(|| target.clone());
    Value::Object(row)
}

/// `mergeDictionaryView`: one profile's schema-1 view merged into the whole
/// state. With `legacy`, the view comes from code that predates profiles and
/// lists every package: the profile keeps its own packages in the view's order
/// with the view's flags and gains another only if the view enables it.
pub(crate) fn merge_dictionary_view(state: &Value, profile_id: &str, view: &Value, legacy: bool) -> Result<Value, String> {
    let rows: Vec<&Value> = array(view.get("dictionaries")).iter().filter(|row| row.is_object()).collect();
    let current = state.get("profiles").and_then(|profiles| profiles.get(profile_id)).filter(|profile| profile.is_object());
    let own: Vec<Option<&Value>> = array(current.and_then(|current| current.get("dictionaries")))
        .iter()
        .map(|entry| entry.as_object().and_then(|entry| entry.get("id")))
        .collect();
    let stored = array(state.get("dictionaries"));
    let mut packages: Vec<(Option<Value>, Value)> = Vec::new();
    for row in stored {
        set(&mut packages, row.get("id"), row.clone());
    }
    if rows.iter().any(|row| !packages.iter().any(|(id, _)| js::strict_eq(id.as_ref(), row.get("id")))) {
        return Err("the dictionary state write lists a dictionary that is not installed".to_owned());
    }
    let listed: Vec<&&Value> =
        rows.iter().filter(|row| !legacy || own.iter().any(|id| js::strict_eq(*id, row.get("id"))) || enabled_flag(row)).collect();
    for row in &rows {
        let previous = packages.iter().find(|(id, _)| js::strict_eq(id.as_ref(), row.get("id"))).map(|(_, row)| row.clone());
        set(&mut packages, row.get("id"), with_stored_target_language(package_row(row), previous.as_ref()));
    }
    let dictionaries: Vec<Value> = stored
        .iter()
        .map(|row| packages.iter().find(|(id, _)| js::strict_eq(id.as_ref(), row.get("id"))).map_or(Value::Null, |(_, row)| row.clone()))
        .collect();
    let groups = view
        .get("groups")
        .filter(|groups| groups.is_array())
        .or_else(|| current.and_then(|current| current.get("groups")))
        .cloned()
        .unwrap_or_else(|| json!([]));
    let mut profiles = object(state.get("profiles"));
    profiles.insert(
        profile_id.to_owned(),
        json!({
            "dictionaries": listed.iter().map(|row| entry(row.get("id"), enabled_flag(row))).collect::<Vec<_>>(),
            "groups": groups,
        }),
    );
    let mut rest = Map::new();
    rest.insert("dictionaries".to_owned(), Value::Array(dictionaries));
    rest.insert("profiles".to_owned(), Value::Object(profiles));
    Ok(with_revision(PROFILES_SCHEMA, state, rest))
}

/// `Map.prototype.set` on a list keyed by `id`.
fn set(packages: &mut Vec<(Option<Value>, Value)>, id: Option<&Value>, row: Value) {
    match packages.iter_mut().find(|(key, _)| js::strict_eq(key.as_ref(), id)) {
        Some(entry) => entry.1 = row,
        None => packages.push((id.cloned(), row)),
    }
}

/// A refused engine commit: its message and `errorCode`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused {
    pub(crate) error: &'static str,
    pub(crate) code: &'static str,
}

/// `mergeEngineDictionaries`: the engine's next package list (every package
/// once, `enabled` meaning "enabled in any profile") merged into a schema-2
/// state. A new package joins `profile_id`, last; `joins` names installed
/// packages that join it when its list lacks them; a personal package is first
/// and enabled in every profile of its language and in no other.
pub(crate) fn merge_engine_dictionaries(state: &Value, packages: &[Value], profile_id: &str, joins: &[Value]) -> Result<Value, Refused> {
    let id_of = |row: &Value| row.get("id").cloned();
    let previous: Vec<Option<Value>> = array(state.get("dictionaries")).iter().map(id_of).collect();
    let present: Vec<Option<Value>> = packages.iter().map(id_of).collect();
    let has = |list: &[Option<Value>], id: Option<&Value>| list.iter().any(|entry| js::strict_eq(entry.as_ref(), id));
    let profiles = state.get("profiles");
    let joined = profiles.and_then(|profiles| profiles.get(profile_id)).is_some_and(Value::is_object);
    if !joined && packages.iter().any(|row| !has(&previous, row.get("id")) && !is_personal_package(row)) {
        return Err(if profile_id == JAPANESE {
            Refused { error: NO_JAPANESE_PROFILE, code: "no-japanese-profile" }
        } else {
            Refused { error: PROFILE_REMOVED, code: "profile-missing" }
        });
    }
    let personal_ids: Vec<Option<Value>> = packages.iter().filter(|row| is_personal_package(row)).map(id_of).collect();
    let mut merged = Map::new();
    for (id, profile) in profiles.and_then(Value::as_object).into_iter().flatten() {
        let mut list: Vec<Value> = array(profile.get("dictionaries"))
            .iter()
            .filter(|entry| has(&present, entry.as_object().and_then(|entry| entry.get("id"))))
            .cloned()
            .collect();
        if id == profile_id {
            for row in packages {
                let joins_now = joins.iter().any(|join| js::strict_eq(Some(join), row.get("id")));
                if (has(&previous, row.get("id")) && !joins_now) || is_personal_package(row) {
                    continue;
                }
                if !list.iter().any(|entry| js::strict_eq(entry.get("id"), row.get("id"))) {
                    list.push(entry(row.get("id"), enabled_flag(row)));
                }
            }
        }
        let mut entries: Vec<Value> = packages
            .iter()
            .filter(|row| personal_language(row).as_deref() == Some(id.as_str()))
            .map(|row| entry(row.get("id"), true))
            .collect();
        entries.extend(list.into_iter().filter(|entry| !has(&personal_ids, entry.get("id"))));
        let ids: Vec<Option<Value>> = entries.iter().map(|entry| entry.get("id").cloned()).collect();
        merged.insert(id.clone(), json!({ "dictionaries": entries, "groups": pruned_groups(profile, &ids) }));
    }
    let mut rest = Map::new();
    rest.insert("dictionaries".to_owned(), Value::Array(packages.iter().map(package_row).collect()));
    rest.insert("profiles".to_owned(), Value::Object(merged));
    Ok(with_revision(PROFILES_SCHEMA, state, rest))
}

/// `prunedGroups`: members outside the profile's own list are dropped, and repeats.
fn pruned_groups(profile: &Value, ids: &[Option<Value>]) -> Vec<Value> {
    array(profile.get("groups"))
        .iter()
        .map(|group| {
            let members = array(group.get("dictionaryIds"));
            let kept: Vec<Value> = members
                .iter()
                .enumerate()
                .filter(|(index, member)| {
                    ids.iter().any(|id| js::strict_eq(id.as_ref(), Some(member)))
                        && members.iter().position(|other| js::strict_eq(Some(other), Some(member))) == Some(*index)
                })
                .map(|(_, member)| member.clone())
                .collect();
            let mut group = spread(group);
            group.insert("dictionaryIds".to_owned(), Value::Array(kept));
            Value::Object(group)
        })
        .collect()
}

/// The records `heldLanguages` reads.
#[derive(Clone, Copy, Default)]
pub(crate) struct Records<'a> {
    pub(crate) options: Option<&'a Value>,
    pub(crate) state: Option<&'a Value>,
    pub(crate) document: Option<&'a Value>,
    pub(crate) lookup_stats: Option<&'a Value>,
    pub(crate) word_status_overrides: Option<&'a Value>,
}

/// `heldLanguages`: those for which the records have a profile, a personal
/// package, a personal document with text, a lookup-count entry, or a marks
/// entry with a word.
pub(crate) fn held_languages(records: Records<'_>) -> BTreeSet<String> {
    let mut held: BTreeSet<String> =
        profiles(records.options).iter().filter_map(|profile| profile["language"].as_str().map(str::to_owned)).collect();
    held.extend(array(records.state.and_then(|state| state.get("dictionaries"))).iter().filter_map(personal_language));
    let has_text =
        |entry: Option<&Value>| entry.and_then(|entry| entry.get("text")).and_then(Value::as_str).is_some_and(|text| !text.is_empty());
    if has_text(records.document) {
        held.insert(JAPANESE.to_owned());
    }
    let languages = |record: Option<&Value>| object(record.and_then(|record| record.get("languages")));
    held.extend(languages(records.document).into_iter().filter(|(_, entry)| has_text(Some(entry))).map(|(code, _)| code));
    held.extend(languages(records.lookup_stats).into_iter().map(|(code, _)| code));
    let words = |entry: Option<&Value>| {
        ["known", "ignored"]
            .iter()
            .any(|status| entry.and_then(|entry| entry.get(*status)).and_then(Value::as_array).is_some_and(|words| !words.is_empty()))
    };
    if words(records.word_status_overrides) {
        held.insert(JAPANESE.to_owned());
    }
    held.extend(languages(records.word_status_overrides).into_iter().filter(|(_, entry)| words(Some(entry))).map(|(code, _)| code));
    held
}

/// `singleProfileShape`: the only profile is Japanese and no other language is held.
pub(crate) fn single_profile_shape(records: Records<'_>) -> bool {
    let list = profiles(records.options);
    if list.len() != 1 || list[0]["id"] != JAPANESE || list[0]["language"] != JAPANESE {
        return false;
    }
    held_languages(records).iter().all(|language| language == JAPANESE)
}

#[cfg(test)]
mod tests {
    use super::super::options::normalise;
    use super::*;

    fn outcome<T: Into<Value>>(result: Result<T, String>) -> Value {
        match result {
            Ok(value) => json!({ "ok": value.into() }),
            Err(error) => json!({ "error": error }),
        }
    }

    /// `undefined` is left out of a recorded result, as `JSON.stringify` leaves it out.
    fn maybe(value: Option<Value>) -> Value {
        value.map_or_else(|| json!({}), |value| json!({ "ok": value }))
    }

    /// Every case `scripts/profile-cases.mjs` recorded from the extension's code.
    #[test]
    #[allow(clippy::too_many_lines, reason = "one dispatch over the recorded functions")]
    fn every_recorded_case_matches_the_extension() {
        let recorded: Value = serde_json::from_str(include_str!("testdata/profiles.json")).unwrap_or_default();
        let values = recorded["values"].as_array().cloned().unwrap_or_default();
        let cases = recorded["cases"].as_array().cloned().unwrap_or_default();
        assert!(cases.len() > 5000, "{} cases", cases.len());
        let base = normalise(Some(&json!({})));
        let mut failures = Vec::new();
        for case in &cases {
            let args: Vec<Option<&Value>> = case["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|index| usize::try_from(index.as_i64().unwrap()).ok().map(|index| &values[index]))
                .collect();
            let arg = |index: usize| args.get(index).copied().flatten();
            let id = || arg(1).and_then(Value::as_str).unwrap_or_default();
            let patch = |index: usize| object(arg(index));
            let got = match case["fn"].as_str().unwrap_or_default() {
                "profiles" => json!({ "ok": profiles(arg(0)) }),
                "projectOptions" => json!({ "ok": project_options(arg(0), id(), arg(2) == Some(&Value::Bool(true))) }),
                "projectStoredOptions" => json!({ "ok": project_stored(arg(0)) }),
                "normaliseOptions" => json!({ "ok": normalise(arg(0)) }),
                "profileDescriptor" => json!({ "ok": profile_descriptor(arg(0), id()) }),
                "applyOptionsPatch" => outcome(apply_options_patch(arg(0), &patch(1), arg(2).and_then(Value::as_str).unwrap_or_default())),
                "applyPackSettingsPatch" => {
                    outcome(apply_pack_settings_patch(arg(0), arg(1), arg(2).and_then(Value::as_str).unwrap_or_default()))
                }
                "optionsWithProfiles" => json!({ "ok": options_with_profiles(arg(0)) }),
                "optionsWithoutProfiles" => maybe(arg(0).map(|record| options_without_profiles(Some(record)))),
                "applyToEveryProfile" => outcome(apply_to_every_profile(arg(0), &patch(1))),
                "activeProfileId" => json!({ "ok": active_profile_id(arg(0), arg(1)) }),
                "normaliseLanguageSelection" => json!({ "ok": normalise_language_selection(arg(0)) }),
                "newProfileSettings" => {
                    let mut counter = 0;
                    let language = arg(1).and_then(Value::as_str).and_then(registry::get);
                    json!({ "ok": new_profile_settings(arg(0), language, || {
                        counter += 1;
                        format!("id-{counter}")
                    }) })
                }
                "packSettingDefaults" => json!({ "ok": pack_setting_defaults(arg(0).and_then(Value::as_str).and_then(registry::get)) }),
                "projectDictionaryState" => json!({ "ok": project_dictionary_state(arg(0), id()) }),
                "allPackages" => json!({ "ok": all_packages(arg(0)) }),
                "dictionaryStateWithProfiles" => json!({ "ok": dictionary_state_with_profiles(arg(0)) }),
                "japaneseDictionaryView" => maybe(japanese_dictionary_view(arg(0))),
                "mergeDictionaryView" => {
                    outcome(merge_dictionary_view(arg(0).unwrap(), id(), arg(2).unwrap(), arg(3) == Some(&Value::Bool(true))))
                }
                "mergeEngineDictionaries" => {
                    let packages = array(arg(1)).to_vec();
                    match merge_engine_dictionaries(
                        arg(0).unwrap(),
                        &packages,
                        arg(2).and_then(Value::as_str).unwrap_or_default(),
                        array(arg(3)),
                    ) {
                        Ok(state) => json!({ "ok": state }),
                        Err(refused) => json!({ "error": refused.error, "code": refused.code }),
                    }
                }
                "heldLanguages" | "singleProfileShape" => {
                    let records = arg(0).unwrap();
                    let records = Records {
                        options: records.get("options"),
                        state: records.get("state"),
                        document: records.get("document"),
                        lookup_stats: records.get("lookupStats"),
                        word_status_overrides: records.get("wordStatusOverrides"),
                    };
                    if case["fn"] == "heldLanguages" {
                        json!({ "ok": held_languages(records) })
                    } else {
                        json!({ "ok": single_profile_shape(records) })
                    }
                }
                other => panic!("unknown case {other}"),
            };
            let mut expected = case["expected"].clone();
            if let Some(changed) = expected.get("changed").and_then(Value::as_object) {
                let mut options = base.clone();
                options.extend(changed.clone());
                expected = json!({ "ok": options });
            }
            if !js::same_json(&got, &expected) {
                failures.push(format!("{}({:?}):\n  got      {got}\n  expected {expected}", case["fn"], args));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} cases differ:\n{}",
            failures.len(),
            cases.len(),
            failures.iter().take(6).cloned().collect::<Vec<_>>().join("\n")
        );
    }
}
