// SPDX-License-Identifier: GPL-3.0-or-later
//! The language profile an Anki request is for (upstream docs/languages.md 5.4
//! and 12): once the records have profiles, a request names its profile, and
//! the Template ids it carries become keys of that profile's Templates
//! (`profileTemplateKey`), so the configuration cache and digests are the
//! profile's own. A plain id is the single-profile shape.

use serde_json::{Map, Value, json};

use crate::host::js;
use crate::host::settings::profiles;

/// `PROFILE_TEMPLATE_SEPARATOR`: Template ids hold no control characters, so a
/// key cannot be mistaken for one.
const SEPARATOR: char = '\u{0}';

/// `profileTemplateKey`.
pub(super) fn template_key(profile_id: &str, template_id: Option<&Value>) -> Value {
    let template = template_id.filter(|id| !id.is_null()).map(|id| js::to_string(Some(id))).unwrap_or_default();
    Value::from(format!("{SEPARATOR}{profile_id}{SEPARATOR}{template}"))
}

/// `profileTemplateTarget`: the profile and Template a key names, or no
/// profile and the id itself.
pub(super) fn template_target(key: Option<&Value>) -> (Option<String>, Option<Value>) {
    let Some(text) = key.and_then(Value::as_str).filter(|text| text.starts_with(SEPARATOR)) else { return (None, key.cloned()) };
    let mut parts = text.split(SEPARATOR).skip(1);
    let profile = parts.next().map(str::to_owned);
    let template = parts.next().filter(|id| !id.is_empty()).map(Value::from);
    (profile, template)
}

/// `requestProfile`: its Template key's profile, or the one a request that
/// reads no Template names.
pub(super) fn request_profile(request: &Value) -> Option<String> {
    template_target(request.get("templateId")).0.or_else(|| request.get("profileId").and_then(Value::as_str).map(str::to_owned))
}

/// `withAnkiProfile`: the message with its Template ids made keys of the
/// profile it names, and its request carrying the profile.
pub(super) fn with_anki_profile(mut message: Value) -> Result<Value, String> {
    let Some(named) = message.get("profileId") else { return Ok(message) };
    let Some(profile) = named.as_str().filter(|id| !id.is_empty()).map(str::to_owned) else {
        return Err("the Anki request named an invalid profile".to_owned());
    };
    let kind = message.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
    let scoped = |request: &mut Value| {
        if let Some(request) = request.as_object_mut() {
            let key = template_key(&profile, request.get("templateId"));
            request.insert("templateId".to_owned(), key);
            request.insert("profileId".to_owned(), Value::from(profile.as_str()));
        }
    };
    let Some(fields) = message.as_object_mut() else { return Ok(message) };
    if fields.contains_key("templateId") || kind == "hd_anki_status" || kind == "hd_anki_screenshot" {
        let key = template_key(&profile, fields.get("templateId"));
        fields.insert("templateId".to_owned(), key);
    }
    if let Some(request) = fields.get_mut("request") {
        scoped(request);
    } else if kind == "hd_anki_browse"
        && let Some(expression) = fields.get("expression").filter(|expression| expression.is_string()).cloned()
    {
        let mut request = json!({ "expression": expression });
        scoped(&mut request);
        fields.insert("request".to_owned(), request);
    }
    if let Some(Value::Array(requests)) = fields.get_mut("requests") {
        requests.iter_mut().for_each(scoped);
    }
    Ok(message)
}

/// `ankiLanguageFamilies`: none for a request that names no language or a
/// language whose pack lists note-type families (Japanese), else an empty list.
pub(super) fn language_families(language: Option<&str>) -> Option<Vec<Value>> {
    let language = language?;
    (!profiles::option_requirement_met(&Value::from("ankiNoteTypeFamilies"), Some(language), &Map::new())).then(Vec::new)
}

/// `ankiLanguageConfig`: a Template's configuration in a profile's language.
pub(super) fn language_config(mut config: Map<String, Value>, language: Option<&str>) -> Map<String, Value> {
    if let Some(families) = language_families(language) {
        config.insert("noteTypeFamilies".to_owned(), Value::Array(families));
    }
    config
}

/// `inLanguage`: with the language of `profile` (`{language, packSettings}`).
pub(super) fn in_language(config: Map<String, Value>, profile: Option<&Value>) -> Map<String, Value> {
    language_config(config, profile.and_then(|profile| profile["language"].as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_names_its_templates_by_key() {
        assert_eq!(template_key("en", Some(&json!("t"))), json!("\u{0}en\u{0}t"));
        assert_eq!(template_key("en", None), json!("\u{0}en\u{0}"));
        assert_eq!(template_target(Some(&json!("\u{0}en\u{0}t"))), (Some("en".to_owned()), Some(json!("t"))));
        assert_eq!(template_target(Some(&json!("\u{0}en\u{0}"))), (Some("en".to_owned()), None));
        assert_eq!(template_target(Some(&json!("t"))), (None, Some(json!("t"))));
        assert_eq!(template_target(None), (None, None));
        assert_eq!(request_profile(&json!({ "templateId": "\u{0}ar\u{0}t", "profileId": "en" })), Some("ar".to_owned()));
        assert_eq!(request_profile(&json!({ "profileId": "en" })), Some("en".to_owned()));
    }

    #[test]
    fn with_anki_profile_scopes_the_template_ids_and_requests() {
        let status = with_anki_profile(json!({ "type": "hd_anki_status", "profileId": "en" })).unwrap();
        assert_eq!(status["templateId"], "\u{0}en\u{0}");
        let view = with_anki_profile(json!({ "type": "hd_anki_view", "profileId": "en", "request": { "templateId": "t" } })).unwrap();
        assert_eq!(view["request"], json!({ "templateId": "\u{0}en\u{0}t", "profileId": "en" }));
        assert!(view.get("templateId").is_none());
        let browse = with_anki_profile(json!({ "type": "hd_anki_browse", "profileId": "en", "expression": "cat" })).unwrap();
        assert_eq!(browse["request"], json!({ "expression": "cat", "templateId": "\u{0}en\u{0}", "profileId": "en" }));
        let plain = json!({ "type": "hd_anki_status", "templateId": "t" });
        assert_eq!(with_anki_profile(plain.clone()).unwrap(), plain);
        assert!(with_anki_profile(json!({ "type": "hd_anki_status", "profileId": "" })).is_err());
    }

    #[test]
    fn only_a_language_without_families_names_none() {
        assert_eq!(language_families(None), None);
        assert_eq!(language_families(Some("ja")), None);
        assert_eq!(language_families(Some("en")), Some(Vec::new()));
        let config = Map::from_iter([("model".to_owned(), json!("Basic"))]);
        assert_eq!(language_config(config.clone(), Some("ja")), config);
        assert_eq!(language_config(config, Some("en"))["noteTypeFamilies"], json!([]));
    }
}
