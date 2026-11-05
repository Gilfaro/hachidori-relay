// SPDX-License-Identifier: GPL-3.0-or-later
//! The reader options as the extension stores and checks them (`reader-options.js`):
//! defaults, each field's normalisation, the strict check an options write passes,
//! and the pruning of dictionary selections against the library.
//!
//! `tests` replays cases recorded from the extension's own code
//! (`scripts/settings-cases.mjs`).

use std::collections::HashSet;
use std::sync::OnceLock;

use serde_json::{Map, Value, json};

use crate::host::js::{self, plain_object, strict_eq, truthy, type_of};

const ANKI_FIELDS: [&str; 8] = ["expression", "reading", "definition", "sentence", "frequency", "pitch", "audio", "screenshot"];
const ANKI_DUPLICATE_SCOPES: [&str; 3] = ["model", "deck", "all"];
const ANKI_DUPLICATE_BEHAVIORS: [&str; 3] = ["prevent", "new", "overwrite"];
const ANKI_OVERWRITE_MODES: [&str; 6] = ["coalesce", "coalesce-new", "skip", "append", "prepend", "overwrite"];
const STABLE_ID_MAX_LENGTH: usize = 256;
pub(crate) const ANKI_TEMPLATE_CONFIG_KEYS: [&str; 8] =
    ["deck", "model", "tags", "fields", "duplicateScope", "duplicateBehavior", "captureScreenshot", "fieldTemplates"];
const DEFAULT_ANKI_URL: &str = "http://127.0.0.1:8765";
const EXPERIMENTAL_FEATURES: [&str; 3] = ["googleDocs", "netflixMining", "netflixPreviewScreenshots"];

/// `KEYBIND_ACTIONS`: each action and the kind of argument it takes.
const KEYBIND_ACTIONS: &[(&str, Option<&str>)] = &[
    ("", None),
    ("close", None),
    ("nextEntry", Some("count")),
    ("previousEntry", Some("count")),
    ("lastEntry", None),
    ("firstEntry", None),
    ("nextEntryDifferentDictionary", None),
    ("previousEntryDifferentDictionary", None),
    ("historyBackward", None),
    ("historyForward", None),
    ("addNote", None),
    ("viewNotes", None),
    ("playAudio", None),
    ("playAudioFromSource", Some("audioSource")),
    ("scanSelectedText", None),
    ("scanTextAtSelection", None),
    ("toggleOption", Some("option")),
    ("toggleWordHighlights", None),
    ("markWordKnown", None),
    ("ignoreWord", None),
    ("searchPreviousLine", None),
    ("searchNextLine", None),
    ("searchPreviousWord", None),
    ("searchNextWord", None),
    ("searchRowUp", None),
    ("searchRowDown", None),
];
const KEYBIND_SCOPES: [&str; 3] = ["popup", "search", "web"];
const KEYBIND_MODIFIERS: [&str; 4] = ["meta", "ctrl", "alt", "shift"];
const DEFAULT_KEYBINDS: [(&str, &str, &str, &[&str]); 14] = [
    ("close", "", "Escape", &[]),
    ("previousEntry", "3", "PageUp", &["alt"]),
    ("nextEntry", "3", "PageDown", &["alt"]),
    ("lastEntry", "", "End", &["alt"]),
    ("firstEntry", "", "Home", &["alt"]),
    ("previousEntry", "1", "ArrowUp", &["alt"]),
    ("nextEntry", "1", "ArrowDown", &["alt"]),
    ("historyBackward", "", "KeyB", &["alt"]),
    ("historyForward", "", "KeyF", &["alt"]),
    ("addNote", "", "KeyE", &["alt"]),
    ("playAudio", "", "KeyP", &["alt"]),
    ("viewNotes", "", "KeyV", &["alt"]),
    ("previousEntry", "1", "WheelUp", &["alt"]),
    ("nextEntry", "1", "WheelDown", &["alt"]),
];
const SEARCH_KEYBINDS: [(&str, &str, &str, &[&str]); 7] = [
    ("searchPreviousLine", "", "ArrowUp", &[]),
    ("searchNextLine", "", "ArrowDown", &[]),
    ("searchPreviousWord", "", "ArrowLeft", &[]),
    ("searchNextWord", "", "ArrowRight", &[]),
    ("searchRowUp", "", "ArrowUp", &[]),
    ("searchRowDown", "", "ArrowDown", &[]),
    ("addNote", "", "Enter", &[]),
];
const UNTOGGLED_OPTIONS: [&str; 10] = [
    "lowMemoryMode",
    "useLessRamByDefault",
    "smallerAnkiCards",
    "wordHighlightEnabled",
    "wordHighlightUnknown",
    "wordHighlightLearning",
    "wordHighlightKnown",
    "wordHighlightIgnored",
    "showToolbarSearchButton",
    "searchHistoryEnabled",
];
const NUMBER_RANGES: &[(&str, f64, f64)] = &[
    ("scanLength", 1.0, 64.0),
    ("maxResults", 1.0, 256.0),
    ("scanDelayMs", 0.0, 5000.0),
    ("definitionScanDelayMs", 0.0, 5000.0),
    ("popupHideDelayMs", 0.0, 5000.0),
    ("hidePopupOnCursorExitDelayMs", 0.0, 5000.0),
    ("popupNestingMaxDepth", 0.0, js::MAX_SAFE_INTEGER),
    ("popupWidthPx", 280.0, 1200.0),
    ("popupHeightPx", 200.0, 900.0),
    ("popupScalePercent", 25.0, 500.0),
    ("popupOpacityPercent", 0.0, 100.0),
    ("popupColumns", 1.0, 4.0),
    ("compactDefinitionSummaryCount", 1.0, 6.0),
    ("definitionBlurThreshold", 1.0, 1_000_000.0),
    ("definitionBlurFrequencyThreshold", 1.0, js::MAX_SAFE_INTEGER),
    ("definitionBlurDelayMs", 1000.0, 3_600_000.0),
    ("automaticBackupDays", 1.0, 30.0),
    ("searchWindowWidthPx", 200.0, 16384.0),
    ("searchWindowHeightPx", 200.0, 16384.0),
    ("searchHistoryLimit", 1.0, js::MAX_SAFE_INTEGER),
];
const POPUP_THEMES: [&str; 48] = [
    "auto",
    "default",
    "miku",
    "catppuccin-mocha",
    "solarized-dark",
    "dark",
    "synthwave",
    "halloween",
    "forest",
    "aqua",
    "black",
    "luxury",
    "dracula",
    "business",
    "night",
    "coffee",
    "dim",
    "sunset",
    "abyss",
    "girlypop",
    "solarized-light",
    "light",
    "cupcake",
    "bumblebee",
    "emerald",
    "corporate",
    "retro",
    "cyberpunk",
    "valentine",
    "garden",
    "lofi",
    "pastel",
    "fantasy",
    "wireframe",
    "cmyk",
    "autumn",
    "acid",
    "lemonade",
    "winter",
    "nord",
    "caramellatte",
    "silk",
    "high-contrast",
    "nazeka",
    "plain",
    "jl",
    "bee",
    "crystal",
];
const ENUMERATED_OPTIONS: [(&str, &[&str]); 15] = [
    ("dictionaryEntryStorage", &["auto", "paged", "resident"]),
    ("dictionaryIndexStorage", &["auto", "paged", "resident"]),
    ("lookupMode", &["hover", "activation", "activationSticky"]),
    ("definitionLookupMode", &["inherit", "activation", "click"]),
    ("popupTheme", &POPUP_THEMES),
    ("popupThemeVariant", &["auto", "grey", "beige", "blue"]),
    ("popupToolbarPosition", &["auto", "top", "bottom"]),
    ("frequencyOrder", &["auto", "ascending", "descending", "disabled"]),
    ("definitionBlurFrequencyOrder", &["auto", "ascending", "descending"]),
    ("definitionBlurDirection", &["atLeast", "below"]),
    ("definitionBlurReveal", &["timed", "hover", "click"]),
    ("imageHoverPreview", &["off", "large", "all"]),
    ("glossaryLayoutMode", &["default", "compact"]),
    ("pitchAccentFuriganaStyle", &["contour", "overline"]),
    ("wordHighlightStyle", &["underline", "color", "background"]),
];
const AUDIO_SOURCE_TYPES: [&str; 4] = ["custom", "custom-json", "text-to-speech", "text-to-speech-reading"];
const INVALID_OPTION: &str = "the options write request carried an invalid reader option";

/// The activation keys, by their lower-case name (`ACTIVATION_NAMES`).
fn activation_name(value: &str) -> Option<String> {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    let names = NAMES.get_or_init(|| {
        let mut names: Vec<String> = [
            "MouseMiddle",
            "MouseBack",
            "MouseForward",
            "Shift",
            "Control",
            "Alt",
            "Meta",
            "Space",
            "Enter",
            "Escape",
            "Backspace",
            "Delete",
            "Tab",
            "ArrowUp",
            "ArrowDown",
            "ArrowLeft",
            "ArrowRight",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "Insert",
        ]
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
        names.extend("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789".chars().map(String::from));
        names.extend((1..=24).map(|index| format!("F{index}")));
        names.extend("-=[]\\;',./`".chars().map(String::from));
        names
    });
    let lower = value.to_lowercase();
    names.iter().find(|name| name.to_lowercase() == lower).cloned()
}

fn keybind(action: &str, argument: &str, key: &str, modifiers: &[&str], scope: &str) -> Value {
    json!({ "action": action, "argument": argument, "key": key, "modifiers": modifiers, "scopes": [scope], "enabled": true })
}

fn search_keybinds() -> Vec<Value> {
    SEARCH_KEYBINDS.iter().map(|(action, argument, key, modifiers)| keybind(action, argument, key, modifiers, "search")).collect()
}

fn default_anki_template() -> Map<String, Value> {
    let fields: Map<String, Value> = ANKI_FIELDS.iter().map(|field| ((*field).to_owned(), json!(""))).collect();
    object(json!({
        "id": "default", "name": "Default", "deck": "Default", "model": "", "tags": ["hachidori"], "fields": fields,
        "duplicateScope": "model", "duplicateBehavior": "prevent", "captureScreenshot": true, "fieldTemplates": null,
    }))
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(fields) => fields,
        _ => Map::new(),
    }
}

/// `DEFAULT_OPTIONS`.
pub(crate) fn defaults() -> &'static Map<String, Value> {
    static DEFAULTS: OnceLock<Map<String, Value>> = OnceLock::new();
    DEFAULTS.get_or_init(|| {
        let template = default_anki_template();
        let mut anki = object(json!({ "url": DEFAULT_ANKI_URL, "apiKey": "", "templates": [template] }));
        for key in ANKI_TEMPLATE_CONFIG_KEYS {
            anki.insert(key.to_owned(), template[key].clone());
        }
        let experimental: Map<String, Value> = EXPERIMENTAL_FEATURES.iter().map(|id| ((*id).to_owned(), json!(false))).collect();
        let mut keybinds: Vec<Value> =
            DEFAULT_KEYBINDS.iter().map(|(action, argument, key, modifiers)| keybind(action, argument, key, modifiers, "popup")).collect();
        keybinds.extend(search_keybinds());
        let options = object(json!({
            "scanLength": 16, "maxResults": 32, "hoverEnabled": true, "onlyScanJapaneseText": true,
            "personalDictionaryEnabled": true, "showNoResultNotice": true, "lookupMode": "activationSticky",
            "activationKey": "Shift", "definitionLookupMode": "inherit", "scanDelayMs": 0, "definitionScanDelayMs": null,
            "popupHideDelayMs": 160, "hidePopupOnCursorExit": false, "hidePopupOnCursorExitDelayMs": 160,
            "showMetadataOnlyResults": false, "popupNestingMaxDepth": 10, "popupTheme": "default",
            "popupThemeVariant": "auto", "popupToolbarPosition": "auto", "customPopupCss": "", "customPopupJavascript": "",
            "customLinks": [], "customButtons": [],
            "audioSources": [{ "id": "default-tts", "type": "text-to-speech-reading", "enabled": true, "url": "", "voice": "" }],
            "audioAutoplay": false, "anki": anki, "smallerAnkiCards": false, "experimental": experimental,
            "popupWidthPx": 560, "popupHeightPx": 420, "popupScalePercent": 100, "popupOpacityPercent": 85,
            "sourceHighlightEnabled": true, "showPopupAudioButton": true, "popupColumns": 1, "glossaryLayoutMode": "default",
        }));
        let mut options = options;
        options.extend(object(json!({
            "showLookupCounts": true, "definitionBlurCountEnabled": false, "definitionBlurAnkiMature": false,
            "definitionBlurFrequencyEnabled": false, "definitionBlurFrequencyDictionary": "",
            "definitionBlurFrequencyOrder": "auto", "definitionBlurFrequencyThreshold": 10000,
            "definitionBlurDirection": "atLeast", "definitionBlurThreshold": 5, "definitionBlurReveal": "timed",
            "definitionBlurDelayMs": 5000,
        })));
        options.extend(object(json!({
            "wordHighlightEnabled": false, "wordHighlightUnknown": true,
            "wordHighlightLearning": true, "wordHighlightKnown": false, "wordHighlightIgnored": false,
            "wordHighlightStyle": "underline", "showCompactDefinitionSummary": false, "compactDefinitionSummaryCount": 3,
            "compactDefinitionSummaryDictionary": "", "popupImageSource": null, "imageHoverPreview": "large",
            "averageFrequency": false, "showFrequencyTagsWithAverage": false, "showFrequencyDictionaryNames": false,

        })));
        options.extend(object(json!({
            "compactFrequencyNumbers": false, "showPitchAccentFurigana": true, "pitchAccentFuriganaDictionary": "",
            "pitchAccentFuriganaStyle": "contour", "showPitchAccentBadge": true, "showPitchAccentDictionaryNames": true,
            "showPitchAccentText": true, "showPitchAccentPosition": true, "showPitchAccentGraph": false,
            "showPitchAccentColors": false, "hidePopupGrammarTags": true, "kanjiClickDictionary": "",
            "frequencyDictionary": "", "frequencyOrder": "auto", "automaticBackupDays": 2, "useLessRamByDefault": false,

        })));
        options.extend(object(json!({
            "lowMemoryMode": false, "dictionaryEntryStorage": "auto", "dictionaryIndexStorage": "auto",
            "showToolbarSearchButton": true, "searchClickLookup": false, "searchWindowWidthPx": 600,
            "searchWindowHeightPx": 640, "searchHistoryEnabled": false, "searchHistoryLimit": 10000, "keybinds": keybinds,
        })));
        options
    })
}

/// `KEYBIND_TOGGLE_OPTIONS`.
fn toggle_option(name: &str) -> bool {
    defaults().get(name).is_some_and(Value::is_boolean) && !UNTOGGLED_OPTIONS.contains(&name)
}

fn clamp_option(key: &str, value: &Value) -> Value {
    let number = js::to_number(Some(value));
    if !number.is_finite() {
        return defaults()[key].clone();
    }
    let (_, min, max) = NUMBER_RANGES.iter().find(|(name, _, _)| *name == key).copied().unwrap_or((key, 0.0, 0.0));
    js::number(number.trunc().clamp(min, max))
}

fn normalise_experimental(value: Option<&Value>) -> Value {
    let source = plain_object(value);
    Value::Object(
        EXPERIMENTAL_FEATURES
            .iter()
            .map(|id| {
                let flag = source.and_then(|source| source.get(*id)).filter(|flag| flag.is_boolean()).cloned().unwrap_or(json!(false));
                ((*id).to_owned(), flag)
            })
            .collect(),
    )
}

fn valid_experimental(value: &Value, normalized: &Value) -> bool {
    let Some(fields) = value.as_object() else { return false };
    fields.keys().all(|key| EXPERIMENTAL_FEATURES.contains(&key.as_str()))
        && EXPERIMENTAL_FEATURES.iter().all(|id| fields.contains_key(*id) && strict_eq(fields.get(*id), normalized.get(*id)))
}

fn text(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_object().and_then(|fields| fields.get(key))
}

fn normalise_audio_sources(value: &Value) -> Value {
    let Some(items) = value.as_array() else { return json!([]) };
    let mut ids = HashSet::new();
    let mut sources = Vec::new();
    for source in items {
        let Some(id) = text(field(source, "id")).filter(|id| !id.is_empty()) else { continue };
        let Some(kind) = text(field(source, "type")).filter(|kind| AUDIO_SOURCE_TYPES.contains(kind)) else { continue };
        if !ids.insert(id.to_owned()) {
            continue;
        }
        sources.push(json!({
            "id": id, "type": kind,
            "enabled": field(source, "enabled").filter(|flag| flag.is_boolean()).cloned().unwrap_or(json!(true)),
            "url": text(field(source, "url")).unwrap_or_default(),
            "voice": text(field(source, "voice")).unwrap_or_default(),
        }));
    }
    Value::Array(sources)
}

fn keybind_argument(action: &str, value: Option<&Value>) -> String {
    let Some(kind) = KEYBIND_ACTIONS.iter().find(|(id, _)| *id == action).and_then(|(_, kind)| *kind) else { return String::new() };
    let fallback = match kind {
        "count" => "1",
        _ => "",
    };
    let Some(value) = text(value) else { return fallback.to_owned() };
    match kind {
        "count" => {
            let mut chars = value.chars();
            let valid = chars.next().is_some_and(|c| ('1'..='9').contains(&c)) && chars.all(|c| c.is_ascii_digit());
            if valid { value.to_owned() } else { "1".to_owned() }
        }
        "option" => {
            if value.is_empty() || toggle_option(value) {
                value.to_owned()
            } else {
                String::new()
            }
        }
        _ => value.to_owned(),
    }
}

fn ordered_subset(value: Option<&Value>, allowed: &[&str]) -> Value {
    let Some(items) = value.and_then(Value::as_array) else { return json!([]) };
    Value::Array(allowed.iter().filter(|item| items.iter().any(|value| value.as_str() == Some(**item))).map(|item| json!(item)).collect())
}

fn normalise_keybinds(value: &Value) -> Value {
    let Some(items) = value.as_array() else { return json!([]) };
    Value::Array(
        items
            .iter()
            .filter_map(|bind| {
                let fields = bind.as_object()?;
                let action = fields.get("action").and_then(Value::as_str)?;
                KEYBIND_ACTIONS.iter().any(|(id, _)| *id == action).then_some(())?;
                let get = |key: &str| fields.get(key);
                Some(json!({
                    "action": action,
                    "argument": keybind_argument(action, get("argument")),
                    "key": text(get("key")).filter(|key| !key.is_empty()),
                    "modifiers": ordered_subset(get("modifiers"), &KEYBIND_MODIFIERS),
                    "scopes": ordered_subset(get("scopes"), &KEYBIND_SCOPES),
                    "enabled": get("enabled").filter(|flag| flag.is_boolean()).cloned().unwrap_or(json!(true)),
                }))
            })
            .collect(),
    )
}

fn label_ok(value: Option<&Value>) -> Option<&str> {
    text(value).filter(|label| !js::trim(label).is_empty() && !js::has_control(label))
}

fn normalise_custom_links(value: &Value) -> Vec<Value> {
    let Some(items) = value.as_array() else { return Vec::new() };
    items
        .iter()
        .filter_map(|link| {
            let label = label_ok(field(link, "label"))?;
            let url = text(field(link, "url")).filter(|url| !js::trim(url).is_empty())?;
            Some(json!({ "label": label, "url": url }))
        })
        .collect()
}

fn valid_identifier(value: Option<&Value>) -> Option<&str> {
    text(value).filter(|id| !id.is_empty() && js::length(id) <= STABLE_ID_MAX_LENGTH && !js::has_control(id))
}

fn legacy_custom_buttons(links: &Value) -> Value {
    Value::Array(
        normalise_custom_links(links)
            .into_iter()
            .enumerate()
            .map(|(index, link)| json!({ "id": format!("legacy-link-{}", index + 1), "type": "link", "label": link["label"], "url": link["url"] }))
            .collect(),
    )
}

pub(crate) fn normalise_custom_buttons(value: &Value) -> Value {
    let Some(items) = value.as_array() else { return json!([]) };
    let mut ids = HashSet::new();
    let mut buttons = Vec::new();
    for button in items {
        let Some(id) = valid_identifier(field(button, "id")) else { continue };
        if ids.contains(id) {
            continue;
        }
        let Some(label) = label_ok(field(button, "label")) else { continue };
        let kind = text(field(button, "type"));
        let normalized = match (kind, text(field(button, "url")), valid_identifier(field(button, "templateId"))) {
            (Some("link"), Some(url), _) if !js::trim(url).is_empty() => json!({ "id": id, "type": "link", "label": label, "url": url }),
            (Some("anki"), _, Some(template)) => json!({ "id": id, "type": "anki", "label": label, "templateId": template }),
            _ => continue,
        };
        ids.insert(id.to_owned());
        buttons.push(normalized);
    }
    Value::Array(buttons)
}

fn custom_links_from_buttons(buttons: &Value) -> Value {
    Value::Array(
        buttons
            .as_array()
            .into_iter()
            .flatten()
            .filter(|button| field(button, "type").and_then(Value::as_str) == Some("link"))
            .map(|button| json!({ "label": button["label"], "url": button["url"] }))
            .collect(),
    )
}

/// `normaliseAnkiConnectUrl`.
pub(crate) fn normalise_anki_connect_url(value: Option<&Value>) -> Option<String> {
    let value = text(value).filter(|value| !js::has_control(value))?;
    let mut url = url::Url::parse(js::trim(value)).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some_and(|password| !password.is_empty())
    {
        return None;
    }
    url.set_fragment(None);
    let search = url.query().is_some_and(|query| !query.is_empty());
    Some(if url.path() == "/" && !search { url.origin().ascii_serialization() } else { url.as_str().to_owned() })
}

fn valid_anki_templates(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Null) => true,
        Some(Value::Object(fields)) => fields.iter().all(|(name, template)| {
            !name.is_empty()
                && truthy(Some(template))
                && field(template, "value").is_some_and(Value::is_string)
                && text(field(template, "overwriteMode")).is_some_and(|mode| ANKI_OVERWRITE_MODES.contains(&mode))
        }),
        _ => false,
    }
}

fn normalise_anki_field_templates(value: Option<&Value>) -> Value {
    match value {
        Some(value @ Value::Object(_)) if valid_anki_templates(Some(value)) => value.clone(),
        _ => Value::Null,
    }
}

/// `normaliseAnkiTemplateConfig`.
fn normalise_anki_template_config(value: Option<&Value>) -> Map<String, Value> {
    let source = plain_object(value);
    let get = |key: &str| source.and_then(|source| source.get(key));
    let mut result = default_anki_template();
    result.remove("id");
    result.remove("name");
    for key in ["deck", "model", "captureScreenshot"] {
        if let Some(value) = get(key).filter(|value| type_of(Some(value)) == type_of(result.get(key))) {
            result.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(tags) = get("tags").and_then(Value::as_array) {
        result.insert("tags".to_owned(), Value::Array(tags.iter().filter(|tag| tag.is_string()).cloned().collect()));
    }
    let fields = get("fields").and_then(Value::as_object);
    result.insert(
        "fields".to_owned(),
        Value::Object(
            ANKI_FIELDS
                .iter()
                .map(|key| {
                    (
                        (*key).to_owned(),
                        fields.and_then(|fields| fields.get(*key)).filter(|value| value.is_string()).cloned().unwrap_or(json!("")),
                    )
                })
                .collect(),
        ),
    );
    let scope = text(get("duplicateScope"));
    match scope {
        Some(scope) if ANKI_DUPLICATE_SCOPES.contains(&scope) => {
            result.insert("duplicateScope".to_owned(), json!(scope));
        }
        Some("deck-root") => {
            result.insert("duplicateScope".to_owned(), json!("deck"));
        }
        Some("collection") => {
            let all = get("duplicateScopeCheckAllModels") == Some(&Value::Bool(true));
            result.insert("duplicateScope".to_owned(), json!(if all { "all" } else { "model" }));
        }
        _ => {}
    }
    if let Some(behavior) = text(get("duplicateBehavior")).filter(|behavior| ANKI_DUPLICATE_BEHAVIORS.contains(behavior)) {
        result.insert("duplicateBehavior".to_owned(), json!(behavior));
    }
    if get("checkForDuplicates") == Some(&Value::Bool(false)) {
        result.insert("duplicateBehavior".to_owned(), json!("new"));
    }
    result.insert("fieldTemplates".to_owned(), normalise_anki_field_templates(get("fieldTemplates")));
    result
}

fn with_identity(id: Value, name: Value, config: Map<String, Value>) -> Map<String, Value> {
    let mut template = Map::new();
    template.insert("id".to_owned(), id);
    template.insert("name".to_owned(), name);
    template.extend(config);
    template
}

fn normalise_anki_template(value: &Value, index: usize) -> Map<String, Value> {
    let source = plain_object(Some(value));
    let get = |key: &str| source.and_then(|source| source.get(key));
    let (fallback_id, fallback_name) = if index == 0 {
        ("default".to_owned(), "Default".to_owned())
    } else {
        (format!("template-{}", index + 1), format!("Template {}", index + 1))
    };
    let id = valid_identifier(get("id")).map_or(fallback_id, str::to_owned);
    let name = label_ok(get("name")).map_or(fallback_name, str::to_owned);
    with_identity(json!(id), json!(name), normalise_anki_template_config(source.map(|_| value)))
}

fn same_template_config(left: &Map<String, Value>, right: &Map<String, Value>) -> bool {
    ANKI_TEMPLATE_CONFIG_KEYS.iter().all(|key| match (left.get(*key), right.get(*key)) {
        (Some(left), Some(right)) => js::same_json(left, right),
        (left, right) => left.is_none() && right.is_none(),
    })
}

/// `normaliseAnki`.
pub(crate) fn normalise_anki(value: Option<&Value>) -> Value {
    let source = plain_object(value);
    let get = |key: &str| source.and_then(|source| source.get(key));
    let config_source = source.map(|_| value.unwrap_or(&Value::Null));
    let templates_source = get("templates").and_then(Value::as_array).filter(|templates| !templates.is_empty());
    let templates: Vec<Map<String, Value>> = if let Some(list) = templates_source {
        let mut ids = HashSet::new();
        let mut templates: Vec<Map<String, Value>> = list
            .iter()
            .enumerate()
            .map(|(index, template)| normalise_anki_template(template, index))
            .filter(|template| ids.insert(template["id"].as_str().unwrap_or_default().to_owned()))
            .collect();
        if templates.is_empty() {
            templates.push(default_anki_template());
        }
        let projected = normalise_anki_template_config(config_source);
        let carries = ANKI_TEMPLATE_CONFIG_KEYS.iter().all(|key| source.is_some_and(|source| source.contains_key(*key)));
        if carries && !same_template_config(&projected, &templates[0]) {
            templates[0] = with_identity(templates[0]["id"].clone(), templates[0]["name"].clone(), projected);
        }
        templates
    } else {
        vec![with_identity(json!("default"), json!("Default"), normalise_anki_template_config(config_source))]
    };
    let mut result = Map::new();
    result.insert("url".to_owned(), json!(DEFAULT_ANKI_URL));
    result.insert("apiKey".to_owned(), json!(text(get("apiKey")).unwrap_or_default()));
    for key in ANKI_TEMPLATE_CONFIG_KEYS {
        result.insert(key.to_owned(), templates[0].get(key).cloned().unwrap_or(Value::Null));
    }
    result.insert("templates".to_owned(), Value::Array(templates.into_iter().map(Value::Object).collect()));
    if source.is_some_and(|source| source.contains_key("url")) {
        result.insert("url".to_owned(), json!(normalise_anki_connect_url(get("url")).unwrap_or_default()));
    }
    Value::Object(result)
}

fn valid_anki_template(value: &Value, normalized: &Map<String, Value>) -> bool {
    let Some(fields) = value.as_object() else { return false };
    if fields.keys().any(|key| key != "id" && key != "name" && !ANKI_TEMPLATE_CONFIG_KEYS.contains(&key.as_str()))
        || !strict_eq(fields.get("id"), normalized.get("id"))
        || !strict_eq(fields.get("name"), normalized.get("name"))
    {
        return false;
    }
    ANKI_TEMPLATE_CONFIG_KEYS.iter().all(|key| match *key {
        "fieldTemplates" => {
            valid_anki_templates(fields.get("fieldTemplates"))
                && fields
                    .get("fieldTemplates")
                    .zip(normalized.get("fieldTemplates"))
                    .is_some_and(|(left, right)| js::same_json(left, right))
        }
        "fields" => fields
            .get("fields")
            .and_then(Value::as_object)
            .is_some_and(|value| ANKI_FIELDS.iter().all(|field| strict_eq(value.get(*field), normalized["fields"].get(*field)))),
        "tags" => fields
            .get("tags")
            .filter(|tags| tags.is_array())
            .zip(normalized.get("tags"))
            .is_some_and(|(left, right)| js::same_json(left, right)),
        key => strict_eq(fields.get(key), normalized.get(key)),
    })
}

fn valid_anki_projection(value: &Map<String, Value>, normalized: &Map<String, Value>) -> bool {
    normalized.iter().filter(|(key, _)| *key != "templates").all(|(key, expected)| match key.as_str() {
        "url" => {
            !value.contains_key("url")
                || normalise_anki_connect_url(value.get("url")).is_some_and(|url| expected.as_str() == Some(url.as_str()))
        }
        "fieldTemplates" => valid_anki_templates(value.get("fieldTemplates")),
        "fields" => value
            .get("fields")
            .and_then(Value::as_object)
            .is_some_and(|fields| ANKI_FIELDS.iter().all(|field| strict_eq(fields.get(*field), expected.get(*field)))),
        "tags" => value.get("tags").and_then(Value::as_array).is_some_and(|tags| {
            let expected = expected.as_array().map_or(&[][..], Vec::as_slice);
            tags.len() == expected.len() && expected.iter().zip(tags).all(|(expected, tag)| strict_eq(Some(tag), Some(expected)))
        }),
        key => strict_eq(value.get(key), Some(expected)),
    })
}

fn valid_anki(value: &Value, normalized: &Value) -> bool {
    let (Some(fields), Some(normalized)) = (value.as_object(), normalized.as_object()) else { return false };
    if !fields.contains_key("templates") {
        return valid_anki_projection(fields, normalized);
    }
    let allowed = |key: &str| matches!(key, "url" | "apiKey" | "templates") || ANKI_TEMPLATE_CONFIG_KEYS.contains(&key);
    let Some(templates) = fields.get("templates").and_then(Value::as_array) else { return false };
    if fields.keys().any(|key| !allowed(key))
        || templates.len() != normalized["templates"].as_array().map_or(0, Vec::len)
        || !templates.iter().enumerate().all(|(index, template)| valid_anki_template(template, &normalise_anki_template(template, index)))
    {
        return false;
    }
    valid_anki_projection(fields, normalized)
}

fn normalise_activation_key(value: &Value) -> Value {
    if value.as_str() == Some(" ") {
        return json!("Space");
    }
    json!(value.as_str().and_then(activation_name).unwrap_or_else(|| "Shift".to_owned()))
}

fn normalise_kanji_selection(value: &Value) -> Value {
    if let Some(fields) = value.as_object() {
        if text(fields.get("kind")) == Some("tabGroup")
            && let Some(id) = text(fields.get("id")).filter(|id| !id.is_empty())
        {
            return json!({ "kind": "tabGroup", "id": id });
        }
        if let (Some(title), Some(kind)) = (text(fields.get("title")).filter(|title| !title.is_empty()), text(fields.get("kind")))
            && matches!(kind, "term" | "kanji")
        {
            return json!({ "title": title, "kind": kind });
        }
    }
    json!(value.as_str().unwrap_or_default())
}

fn normalise_popup_image_source(value: &Value) -> Value {
    let Some(fields) = value.as_object() else { return Value::Null };
    match text(fields.get("kind")) {
        Some("dictionary") if text(fields.get("title")).is_some_and(|title| !title.is_empty()) => {
            json!({ "kind": "dictionary", "title": fields["title"] })
        }
        Some("tabGroup") if text(fields.get("id")).is_some_and(|id| !id.is_empty()) => json!({ "kind": "tabGroup", "id": fields["id"] }),
        _ => Value::Null,
    }
}

/// `normaliseField`.
fn normalise_field(key: &str, value: &Value) -> Value {
    if key == "definitionScanDelayMs" && value.is_null() {
        return Value::Null;
    }
    if NUMBER_RANGES.iter().any(|(name, _, _)| *name == key) {
        return clamp_option(key, value);
    }
    let default = &defaults()[key];
    if default.is_boolean() {
        return if value.is_boolean() { value.clone() } else { default.clone() };
    }
    if let Some((_, allowed)) = ENUMERATED_OPTIONS.iter().find(|(name, _)| *name == key) {
        return if value.as_str().is_some_and(|value| allowed.contains(&value)) { value.clone() } else { default.clone() };
    }
    match key {
        "activationKey" => normalise_activation_key(value),
        "kanjiClickDictionary" => normalise_kanji_selection(value),
        "popupImageSource" => normalise_popup_image_source(value),
        "audioSources" => normalise_audio_sources(value),
        "customLinks" => Value::Array(normalise_custom_links(value)),
        "customButtons" => normalise_custom_buttons(value),
        "keybinds" => normalise_keybinds(value),
        "anki" => normalise_anki(Some(value)),
        "experimental" => normalise_experimental(Some(value)),
        _ => json!(value.as_str().unwrap_or_default()),
    }
}

/// `isValidOptionField`.
fn valid_option_field(key: &str, raw: &Value, normalized: &Value) -> bool {
    match key {
        "anki" => valid_anki(raw, normalized),
        "experimental" => valid_experimental(raw, normalized),
        "kanjiClickDictionary" => raw.is_string() || normalized.is_object(),
        "popupImageSource" => raw.is_null() || !normalized.is_null(),
        "definitionScanDelayMs" => raw.is_null() || (raw.is_number() && strict_eq(Some(raw), Some(normalized))),
        "keybinds" => {
            let (Some(raw), Some(normalized)) = (raw.as_array(), normalized.as_array()) else { return false };
            raw.len() == normalized.len()
                && normalized.iter().zip(raw).all(|(bind, raw)| {
                    bind.as_object()
                        .is_some_and(|bind| bind.iter().all(|(field, value)| raw.get(field).is_some_and(|raw| js::same_json(raw, value))))
                })
        }
        "audioSources" | "customLinks" | "customButtons" => {
            let (Some(raw), Some(normalized)) = (raw.as_array(), normalized.as_array()) else { return false };
            raw.len() == normalized.len()
                && normalized.iter().zip(raw).all(|(source, raw)| {
                    source.as_object().is_some_and(|source| source.iter().all(|(field, value)| strict_eq(raw.get(field), Some(value))))
                })
        }
        _ => type_of(Some(raw)) == type_of(defaults().get(key)) && strict_eq(Some(raw), Some(normalized)),
    }
}

fn legacy_activation_options(source: &Map<String, Value>, strict: bool) -> Result<Map<String, Value>, String> {
    let Some(modifier) = source.get("modifier") else { return Ok(Map::new()) };
    let key = match modifier.as_str() {
        Some("none" | "shift") => Some("Shift"),
        Some("ctrl") => Some("Control"),
        Some("alt") => Some("Alt"),
        _ => None,
    };
    if strict && key.is_none() {
        return Err(INVALID_OPTION.to_owned());
    }
    Ok(match key {
        Some(key) if modifier.as_str() != Some("none") => object(json!({ "lookupMode": "activation", "activationKey": key })),
        _ => object(json!({ "lookupMode": "hover" })),
    })
}

fn legacy_blur_count_option(source: &Map<String, Value>, strict: bool) -> Result<Map<String, Value>, String> {
    let Some(value) = source.get("definitionBlurEnabled").filter(|_| !source.contains_key("definitionBlurCountEnabled")) else {
        return Ok(Map::new());
    };
    if value.is_boolean() {
        return Ok(object(json!({ "definitionBlurCountEnabled": value })));
    }
    if strict { Err(INVALID_OPTION.to_owned()) } else { Ok(Map::new()) }
}

/// `projectOptions`.
fn project_options(value: Option<&Value>, strict: bool) -> Result<Map<String, Value>, String> {
    let empty = Map::new();
    let source = plain_object(value).unwrap_or(&empty);
    let mut result = legacy_activation_options(source, strict)?;
    result.extend(legacy_blur_count_option(source, strict)?);
    if !source.contains_key("smallerAnkiCards")
        && let Some(flag) =
            source.get("experimental").and_then(|experimental| experimental.get("smallerAnkiCards")).filter(|flag| flag.is_boolean())
    {
        result.insert("smallerAnkiCards".to_owned(), flag.clone());
    }
    for key in defaults().keys() {
        let Some(raw) = source.get(key) else { continue };
        let normalized = normalise_field(key, raw);
        if strict && !valid_option_field(key, raw, &normalized) {
            return Err(INVALID_OPTION.to_owned());
        }
        result.insert(key.clone(), normalized);
    }
    if source.contains_key("customButtons") {
        let links = custom_links_from_buttons(&result["customButtons"]);
        result.insert("customLinks".to_owned(), links);
    } else if source.contains_key("customLinks") {
        let buttons = legacy_custom_buttons(&result["customLinks"]);
        result.insert("customButtons".to_owned(), buttons);
    }
    Ok(result)
}

/// `projectStoredOptions`.
pub(crate) fn project_stored(value: Option<&Value>) -> Map<String, Value> {
    project_options(value, false).unwrap_or_default()
}

/// `validateOptionsPatch`.
pub(crate) fn validate_patch(value: Option<&Value>) -> Result<Map<String, Value>, String> {
    if plain_object(value).is_none() {
        return Err("the options write request carried no object".to_owned());
    }
    project_options(value, true)
}

/// `normaliseOptions`.
pub(crate) fn normalise(value: Option<&Value>) -> Map<String, Value> {
    let mut options = defaults().clone();
    options.extend(project_stored(value));
    let carries_buttons = value.and_then(Value::as_object).is_some_and(|source| source.contains_key("customButtons"));
    let anki = normalise_anki(options.get("anki"));
    options.insert("anki".to_owned(), anki);
    let buttons =
        if carries_buttons { normalise_custom_buttons(&options["customButtons"]) } else { legacy_custom_buttons(&options["customLinks"]) };
    options.insert("customLinks".to_owned(), custom_links_from_buttons(&buttons));
    options.insert("customButtons".to_owned(), buttons);
    let searches = options["keybinds"].as_array().is_some_and(|binds| {
        binds.iter().any(|bind| bind["scopes"].as_array().is_some_and(|scopes| scopes.iter().any(|scope| scope == "search")))
    });
    if !searches && let Some(Value::Array(binds)) = options.get_mut("keybinds") {
        binds.extend(search_keybinds());
    }
    options
}

/// `ankiTemplateConfig`.
pub(crate) fn anki_template_config(anki: Option<&Value>, template_id: Option<&Value>) -> Option<Map<String, Value>> {
    let normalized = normalise_anki(anki);
    let templates = normalized["templates"].as_array()?;
    let template = match template_id {
        None | Some(Value::Null) => templates.first(),
        Some(id) => templates.iter().find(|template| strict_eq(template.get("id"), Some(id))),
    }?;
    let mut config = Map::new();
    config.insert("url".to_owned(), normalized["url"].clone());
    config.insert("apiKey".to_owned(), normalized["apiKey"].clone());
    for key in ANKI_TEMPLATE_CONFIG_KEYS {
        config.insert(key.to_owned(), template.get(key).cloned().unwrap_or(Value::Null));
    }
    Some(config)
}

/// `hasCapability`, with JavaScript's comparisons of whatever the counts are.
fn has_capability(dictionary: &Value, kind: &str) -> bool {
    let count = |key: &str| js::to_number(dictionary.get(key));
    let zero = |key: &str| dictionary.get(key).and_then(Value::as_f64) == Some(0.0);
    match kind {
        "freq" => count("frequencyCount") > 0.0,
        "pitch" => count("pitchCount") > 0.0,
        "kanji" => count("kanjiCount") > 0.0,
        _ => count("termCount") > 0.0 || (zero("frequencyCount") && zero("pitchCount") && zero("kanjiCount")),
    }
}

/// `dictionaries.find(entry => entry.title === title)`.
fn find_by_title<'a>(dictionaries: &'a [Value], title: Option<&Value>) -> Result<Option<&'a Value>, String> {
    for entry in dictionaries {
        if strict_eq(js::property(Some(entry), "title")?, title) {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

fn kanji_click_selection(value: &Value, dictionaries: &[Value], groups: &[Value]) -> Result<Value, String> {
    let wrapped;
    let selection = if let Some(title) = value.as_str() {
        wrapped = json!({ "title": title, "kind": "" });
        &wrapped
    } else {
        value
    };
    if field(selection, "kind").and_then(Value::as_str) == Some("tabGroup") {
        for group in groups {
            if strict_eq(js::property(Some(group), "id")?, field(selection, "id")) {
                return Ok(value.clone());
            }
        }
        return Ok(json!(""));
    }
    if !truthy(field(selection, "title")) {
        return Ok(value.clone());
    }
    let selected = find_by_title(dictionaries, field(selection, "title"))?;
    let requested = text(field(selection, "kind")).filter(|kind| matches!(*kind, "term" | "kanji"));
    let kind = requested.unwrap_or(if selected.is_some_and(|selected| has_capability(selected, "kanji")) { "kanji" } else { "term" });
    if !selected.is_some_and(|selected| has_capability(selected, kind)) {
        return Ok(json!(""));
    }
    Ok(if requested.is_some() { value.clone() } else { json!({ "title": field(selection, "title"), "kind": kind }) })
}

/// `normaliseDictionarySelections`.
pub(crate) fn normalise_dictionary_selections(value: &Value, dictionaries: &[Value], groups: &[Value]) -> Result<Value, String> {
    let Some(source) = value.as_object() else { return Ok(value.clone()) };
    let mut options = source.clone();
    let selected = find_by_title(dictionaries, options.get("frequencyDictionary"))?;
    if truthy(options.get("frequencyDictionary")) {
        let usable = selected
            .is_some_and(|selected| !strict_eq(selected.get("enabled"), Some(&Value::Bool(false))) && has_capability(selected, "freq"));
        if !usable {
            options.insert("frequencyDictionary".to_owned(), json!(""));
        }
    }
    if let Some(selection) = options.get("kanjiClickDictionary").cloned() {
        options.insert("kanjiClickDictionary".to_owned(), kanji_click_selection(&selection, dictionaries, groups)?);
    }
    Ok(Value::Object(options))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case `scripts/settings-cases.mjs` recorded from the extension's code.
    #[test]
    fn every_recorded_case_matches_the_extension() {
        let cases: Vec<Value> = serde_json::from_str(include_str!("testdata/options.json")).unwrap_or_default();
        assert!(cases.len() > 1000, "{} cases", cases.len());
        let mut failures = Vec::new();
        for case in &cases {
            let input = case.get("input");
            let got = match case["fn"].as_str().unwrap_or_default() {
                "validateOptionsPatch" => match validate_patch(input) {
                    Ok(patch) => json!({ "ok": patch }),
                    Err(error) => json!({ "error": error }),
                },
                "projectStoredOptions" => json!({ "ok": project_stored(input) }),
                "normaliseOptions" => json!({ "ok": normalise(input) }),
                "normaliseDictionarySelections" => {
                    let list = |key: &str| case[key].as_array().cloned().unwrap_or_default();
                    match normalise_dictionary_selections(input.unwrap_or(&Value::Null), &list("dictionaries"), &list("groups")) {
                        Ok(options) => json!({ "ok": options }),
                        Err(error) => json!({ "error": error }),
                    }
                }
                "parseCustomDictionary" => {
                    let (entries, errors) = super::super::custom::parse(input.and_then(Value::as_str).unwrap_or_default());
                    let revision =
                        super::super::custom::semantic_revision(&entries).map_or_else(|error| json!({ "error": error }), Value::from);
                    let entries: Vec<Value> = entries
                        .iter()
                        .map(|entry| json!({ "term": entry.term, "reading": entry.reading, "definition": entry.definition }))
                        .collect();
                    json!({ "ok": { "entries": entries, "errors": errors, "revision": revision } })
                }
                "ankiTemplateConfig" => json!({ "ok": anki_template_config(input, case.get("templateId")) }),
                other => panic!("unknown case {other}"),
            };
            let mut expected = case["expected"].clone();
            if let Some(changed) = expected.get("changed").and_then(Value::as_object) {
                let mut options = defaults().clone();
                options.extend(changed.clone());
                expected = json!({ "ok": options });
            }
            if !js::same_json(&got, &expected) {
                failures.push(format!(
                    "{}({}):\n  got      {got}\n  expected {}",
                    case["fn"],
                    input.map_or("undefined".into(), Value::to_string),
                    expected
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} cases differ:\n{}",
            failures.len(),
            cases.len(),
            failures.iter().take(8).cloned().collect::<Vec<_>>().join("\n")
        );
    }
}
