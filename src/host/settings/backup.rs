// SPDX-License-Identifier: GPL-3.0-or-later
//! The snapshot a dictionary download carries: `readBackupPayload`'s snapshot of
//! what is stored, cut down to one dictionary as `hd_api_dictionary_open` cuts it.

use serde_json::{Map, Value, json};

use super::{custom, options, worker};
use crate::host::js;

const UPDATE_SCHEDULES: [&str; 5] = ["off", "hourly", "daily", "weekly", "monthly"];

/// `normaliseGroupName`.
fn group_name(value: Option<&Value>) -> String {
    let name = icu_normalizer::ComposingNormalizerBorrowed::new_nfkc().normalize(value.and_then(Value::as_str).unwrap_or_default());
    let mut collapsed = String::with_capacity(name.len());
    for word in js::trim(&name).split(js::is_space).filter(|word| !word.is_empty()) {
        if !collapsed.is_empty() {
            collapsed.push(' ');
        }
        collapsed.push_str(word);
    }
    collapsed
}

/// `normaliseDictionaryGroups`.
fn groups(value: Option<&Value>, dictionaries: &[Value]) -> Value {
    let Some(groups) = value.and_then(Value::as_array) else { return json!([]) };
    let installed: Vec<&Value> = dictionaries.iter().filter_map(|dictionary| dictionary.get("id")).collect();
    let kept = groups.iter().filter_map(|group| {
        let id = group.get("id").and_then(Value::as_str).unwrap_or_default();
        let name = group_name(group.get("name"));
        if id.is_empty() || name.is_empty() {
            return None;
        }
        let mut seen: Vec<&Value> = Vec::new();
        let ids: Vec<Value> = group
            .get("dictionaryIds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|id| {
                let fresh = installed.iter().any(|installed| js::strict_eq(Some(installed), Some(id)))
                    && !seen.iter().any(|seen| js::strict_eq(Some(seen), Some(id)));
                if fresh {
                    seen.push(id);
                }
                fresh
            })
            .cloned()
            .collect();
        Some(json!({ "id": id, "name": name, "dictionaryIds": ids }))
    });
    Value::Array(kept.collect())
}

/// `normaliseUpdateSettings`.
fn update_settings(value: Option<&Value>) -> Value {
    let field = |name: &str| value.and_then(|value| value.get(name));
    let schedule = field("schedule").and_then(Value::as_str).filter(|schedule| UPDATE_SCHEDULES.contains(schedule)).unwrap_or("off");
    json!({
        "revision": js::number(js::safe_integer(field("revision")).filter(|revision| *revision >= 0.0).unwrap_or(0.0)),
        "schedule": schedule,
        "lastCheckedAt": field("lastCheckedAt").filter(|checked| checked.is_string()).cloned().unwrap_or(Value::Null),
    })
}

/// The snapshot `hd_api_dictionary_open` archives with `dictionary`: everything
/// stored (`stored`, by storage key), with only that dictionary, the groups cut down
/// to it, the custom source only for the custom dictionary, and no marked words.
pub(crate) fn dictionary_snapshot(stored: &Map<String, Value>, dictionary: &Value) -> Result<Value, String> {
    let read = |key: &str| stored.get(key).filter(|value| !value.is_null());
    let mut state = read("dictionaryState").and_then(Value::as_object).cloned().unwrap_or_default();
    let dictionaries = [dictionary.clone()];
    state.insert("groups".to_owned(), groups(state.get("groups"), &dictionaries));
    state.insert("dictionaries".to_owned(), Value::Array(dictionaries.into()));
    let mut options = options::project_stored(read("options"));
    options.insert("revision".to_owned(), js::number(worker::options_revision(read("options"))));
    let document = if dictionary.get("id").and_then(Value::as_str) == Some(custom::CUSTOM_DICTIONARY_ID) {
        custom::normalise_document(read("customDictionarySource"))?
    } else {
        custom::empty_document()
    };
    Ok(json!({
        "state": state,
        "options": options,
        "document": document,
        "updates": update_settings(read("dictionaryUpdates")),
        "lookupStats": read("lookupStats").cloned().unwrap_or_else(|| json!({ "generation": null, "revision": 0 })),
        "wordStatusOverrides": { "revision": 0, "known": [], "ignored": [] },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_download_carries_one_dictionary_and_its_groups() {
        let entry = json!({ "id": "a", "title": "A" });
        let stored = json!({
            "dictionaryState": { "schemaVersion": 1, "revision": 4, "dictionaries": [entry, { "id": "b" }],
                "groups": [{ "id": "g", "name": " Ｇ  one ", "dictionaryIds": ["a", "b", "a"] }, { "id": "", "name": "x" }] },
            "dictionaryUpdates": { "revision": 2, "schedule": "yearly" },
            "customDictionarySource": { "revision": 3 },
        });
        let snapshot = dictionary_snapshot(stored.as_object().unwrap_or(&Map::new()), &entry).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(snapshot["state"]["dictionaries"], json!([entry]));
        assert_eq!(snapshot["state"]["revision"], 4);
        assert_eq!(snapshot["state"]["groups"], json!([{ "id": "g", "name": "G one", "dictionaryIds": ["a"] }]));
        assert_eq!(snapshot["updates"], json!({ "revision": 2, "schedule": "off", "lastCheckedAt": null }));
        assert_eq!(snapshot["document"], custom::empty_document());
        assert_eq!(snapshot["lookupStats"], json!({ "generation": null, "revision": 0 }));
    }
}
