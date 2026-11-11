// SPDX-License-Identifier: GPL-3.0-or-later
//! The language registry Hachidori's extension ships (`vendor/languages/registry.js`,
//! `globalThis.HDLanguages`): each language pack's descriptor as its
//! scripts/sync-languages.mjs generates it from the Hoshidicts packs. The relay
//! keeps a copy (`registry.json`, written by scripts/registry.mjs) for what the
//! profile layer reads of a language: its capabilities, settings, scripts, Anki
//! and audio hints, and its personal dictionary's identity.

use std::sync::OnceLock;

use serde_json::{Map, Value};

use super::js;

fn packs() -> &'static [Value] {
    static PACKS: OnceLock<Vec<Value>> = OnceLock::new();
    PACKS.get_or_init(|| {
        let registry: Value = serde_json::from_str(include_str!("registry.json")).unwrap_or_default();
        registry["packs"].as_array().cloned().unwrap_or_default()
    })
}

/// `HDLanguages.get(code)`: the pack's descriptor, or `None` for a language
/// this build has no pack for.
pub(crate) fn get(code: &str) -> Option<&'static Value> {
    packs().iter().find(|pack| pack["code"] == code)
}

/// `languageName` (language-names.js): the pack's English name, else the
/// platform's name for a language this build has no pack for, else the code.
pub(crate) fn name(code: &str) -> String {
    get(code)
        .and_then(|pack| pack["name"].as_str())
        .filter(|name| !name.is_empty())
        .or_else(|| platform_name(code))
        .unwrap_or(code)
        .to_owned()
}

/// `languageName` of whatever a request carries: a value that is no string is
/// named as JavaScript prints it (`Intl.DisplayNames` refuses it).
pub(crate) fn name_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(code)) => name(code),
        other => js::to_string(other),
    }
}

/// `Intl.DisplayNames(["en"], { type: "language" }).of(code)` for a bare
/// language subtag, from the table scripts/language-names.mjs writes. Tags with
/// a region or script are not in it and are named by their code.
fn platform_name(code: &str) -> Option<&'static str> {
    static NAMES: OnceLock<Map<String, Value>> = OnceLock::new();
    let names = NAMES.get_or_init(|| {
        let table: Value = serde_json::from_str(include_str!("language-names.json")).unwrap_or_default();
        table["names"].as_object().cloned().unwrap_or_default()
    });
    if !(2..=3).contains(&code.len()) || !code.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return None;
    }
    names.get(&code.to_ascii_lowercase()).and_then(Value::as_str)
}

/// `HDLanguages.codes`, in the registry's order.
pub(crate) fn codes() -> impl Iterator<Item = &'static str> {
    packs().iter().filter_map(|pack| pack["code"].as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_describes_exactly_the_shipped_packs() {
        let registry: Vec<&str> = codes().collect();
        assert_eq!(registry, crate::host::engine::languages::codes(), "scripts/registry.mjs and the Hoshidicts packs disagree");
        assert_eq!(
            get("en").and_then(|pack| pack.pointer("/personalDictionary/title")),
            Some(&Value::from("Hachidori Custom Dictionary (English)"))
        );
        assert_eq!(get("zz"), None);
        assert_eq!(
            [name("en"), name("EN"), name("eng"), name("hi"), name("zz"), name("")],
            ["English", "English", "English", "Hindi", "zz", ""]
        );
        assert_eq!([name_of(None), name_of(Some(&Value::from(3)))], ["undefined", "3"]);
    }
}
