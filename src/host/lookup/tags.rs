// SPDX-License-Identifier: GPL-3.0-or-later
//! Definition tags as the extension expands them (`engine-service.js`
//! `expandDefinitionTags`, after Yomitan's `_expandTagGroups` and `_groupTags`).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use icu_collator::options::CollatorOptions;
use icu_collator::{Collator, CollatorBorrowed};
use serde_json::{Map, Value, json};

/// One dictionary's tag bank: each name's first row (Yomitan's findTagMetaBulk).
pub(crate) type Bank = HashMap<String, Map<String, Value>>;

/// Each dictionary's bank, from the engine's `hdw_tags` reply.
pub(crate) fn banks(tags_json: &str) -> Result<HashMap<String, Bank>, String> {
    let parsed: Value = serde_json::from_str(tags_json).map_err(|error| format!("hdw_tags returned malformed JSON: {error}"))?;
    let mut banks = HashMap::new();
    for dictionary in parsed.as_array().into_iter().flatten() {
        let title = dictionary.get("dictionary").and_then(Value::as_str).unwrap_or_default().to_owned();
        let mut bank = Bank::new();
        for tag in dictionary.get("tags").and_then(Value::as_array).into_iter().flatten() {
            if let (Some(name), Some(row)) = (tag.get("name").and_then(Value::as_str), tag.as_object()) {
                bank.entry(name.to_owned()).or_insert_with(|| row.clone());
            }
        }
        banks.insert(title, bank);
    }
    Ok(banks)
}

/// Yomitan's `_splitField` (`HDGlossary.parseTagList`): names split on U+0020 only.
pub(crate) fn tag_list(value: Option<&str>) -> Vec<&str> {
    value.unwrap_or_default().split(' ').filter(|name| !name.is_empty()).collect()
}

/// `Intl.Collator("en-US")`: CLDR root collation at tertiary strength.
fn collator() -> &'static CollatorBorrowed<'static> {
    static COLLATOR: OnceLock<CollatorBorrowed<'static>> = OnceLock::new();
    COLLATOR.get_or_init(|| {
        Collator::try_new(icu_collator::CollatorPreferences::default(), CollatorOptions::default())
            .unwrap_or_else(|_| unreachable!("the root collation is compiled in"))
    })
}

fn number(row: Option<&Map<String, Value>>, key: &str) -> Value {
    row.and_then(|row| row.get(key)).filter(|value| !value.is_null()).cloned().unwrap_or_else(|| json!(0))
}

/// The expanded tags for one glossary's `definitionTags`: a name is looked up by
/// its part before ":", one the bank lacks is category "default", a repeated name
/// is listed once, and the list is sorted by order, then name.
pub(crate) fn expand(definition_tags: Option<&str>, bank: Option<&Bank>) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut tags: Vec<(f64, String, Value)> = Vec::new();
    for name in tag_list(definition_tags) {
        if !seen.insert(name) {
            continue;
        }
        let key = name.split_once(':').map_or(name, |(head, _)| head);
        let row = bank.and_then(|bank| bank.get(key));
        let category =
            row.and_then(|row| row.get("category")).and_then(Value::as_str).filter(|category| !category.is_empty()).unwrap_or("default");
        let order = number(row, "order");
        let notes = row.and_then(|row| row.get("notes")).filter(|value| !value.is_null()).cloned().unwrap_or_else(|| json!(""));
        let sort = order.as_f64().unwrap_or(0.0);
        let tag = json!({"name": name, "category": category, "order": order, "score": number(row, "score"), "notes": notes});
        tags.push((sort, name.to_owned(), tag));
    }
    tags.sort_by(|(left_order, left, _), (right_order, right, _)| {
        left_order.partial_cmp(right_order).unwrap_or(Ordering::Equal).then_with(|| collator().compare(left, right))
    });
    tags.into_iter().map(|(_, _, tag)| tag).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_follow_the_bank_and_sort_by_order_then_name() {
        let banks = banks(
            r#"[{"dictionary":"D","tags":[{"name":"n","category":"partOfSpeech","order":-3,"score":0,"notes":"noun"},
                {"name":"n","category":"ignored","order":9,"score":0,"notes":""},
                {"name":"P","category":"popular","order":-10,"score":10,"notes":"popular"}]}]"#,
        );
        let banks = banks.unwrap_or_default();
        let expanded = expand(Some("vs n n P B a"), banks.get("D"));
        let names: Vec<_> = expanded.iter().map(|tag| tag["name"].as_str().unwrap_or_default()).collect();
        assert_eq!(names, ["P", "n", "a", "B", "vs"]);
        assert_eq!(expanded[1], json!({"name": "n", "category": "partOfSpeech", "order": -3, "score": 0, "notes": "noun"}));
        assert_eq!(expanded[2], json!({"name": "a", "category": "default", "order": 0, "score": 0, "notes": ""}));
    }

    /// Names sort as the extension's `Intl.Collator("en-US")` sorts them
    /// (recorded by `scripts/furigana-cases.mjs`).
    #[test]
    fn names_sort_as_the_extension_sorts_them() {
        let expected: Vec<String> = serde_json::from_str(include_str!("testdata/tag-order.json")).unwrap_or_default();
        assert!(expected.len() > 100);
        let mut sorted = expected.clone();
        sorted.reverse();
        sorted.sort_by(|left, right| collator().compare(left, right));
        assert_eq!(sorted, expected);
    }
}
