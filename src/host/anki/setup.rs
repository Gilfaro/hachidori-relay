// SPDX-License-Identifier: GPL-3.0-or-later
//! Read-only Anki setup detection (`anki-setup.js`): recognises an installed Senren,
//! Lapis or Kiku note type, picks the one with the most notes and the deck holding
//! most of them, and proposes the preset mapping. Nothing here writes to Anki.

use serde_json::{Map, Value, json};

use super::gateway::{self, Discovery, Failure, Result};
use super::{Invoke, positive_id, templates};
use crate::host::js;

/// `FAMILY_PATTERN` (`/^(senren|lapis|kiku)(?![\p{L}\p{N}])/iu`), lower-cased.
pub(crate) fn family(model: &str) -> Option<&'static str> {
    let name = js::trim(model);
    for family in ["senren", "lapis", "kiku"] {
        let mut chars = name.chars();
        // Unicode case-insensitive matching folds the Kelvin sign to k and the long s to s.
        let matched = family.chars().all(|expected| {
            chars.next().is_some_and(|c| {
                let folded = match c {
                    '\u{212a}' => 'k',
                    '\u{17f}' => 's',
                    other => other.to_ascii_lowercase(),
                };
                folded == expected
            })
        });
        if matched && !chars.next().is_some_and(js::is_letter_or_number) {
            // `match[1].toLowerCase()`: the long s does not lower-case to s.
            let prefix: String = name.chars().take(family.len()).collect();
            return (prefix.to_lowercase() == family).then_some(family);
        }
    }
    None
}

fn family_label(family: &str) -> &'static str {
    match family {
        "senren" => "Senren",
        "lapis" => "Lapis",
        _ => "Kiku",
    }
}

fn id_list(value: &Value, what: &str) -> Result<Vec<i64>> {
    let invalid = || Failure::new(format!("AnkiConnect returned invalid {what}."));
    value.as_array().ok_or_else(invalid)?.iter().map(|id| positive_id(id).ok_or_else(invalid)).collect()
}

fn distinct(ids: &[i64]) -> usize {
    let mut seen: Vec<i64> = Vec::new();
    for id in ids {
        if !seen.contains(id) {
            seen.push(*id);
        }
    }
    seen.len()
}

/// `ankiSetupTemplates`: the preset's field Templates, if `model` has its shape.
pub(crate) fn setup_templates(
    family: &str,
    model: &str,
    deck: &str,
    fields: &[String],
    base: &Map<String, Value>,
) -> Option<Map<String, Value>> {
    let mut config = base.clone();
    config.insert("model".to_owned(), json!(model));
    config.insert("deck".to_owned(), json!(deck));
    let config = templates::apply_preset(&config, fields, family);
    let field_templates = config.get("fieldTemplates").and_then(Value::as_object)?;
    if !templates::preset_core_mapped(field_templates, family) {
        return None;
    }
    let resolved = templates::resolve(&config, fields);
    let discovery = Discovery {
        connected: true,
        model: model.to_owned(),
        decks: vec![deck.to_owned()],
        models: vec![model.to_owned()],
        fields: fields.to_vec(),
        errors: Vec::new(),
    };
    gateway::availability(&config, Some(&discovery), Some(&resolved)).is_empty().then(|| field_templates.clone())
}

/// `uniqueMaximum`: the entry with the most, and whether another has as many.
fn unique_maximum<T>(entries: &[(T, usize)]) -> (Option<&(T, usize)>, bool) {
    let mut best: Option<&(T, usize)> = None;
    let mut tied = false;
    for entry in entries {
        if entry.1 == 0 {
            continue;
        }
        match best {
            Some(current) if entry.1 == current.1 => tied = true,
            Some(current) if entry.1 < current.1 => {}
            _ => {
                best = Some(entry);
                tied = false;
            }
        }
    }
    (best, tied)
}

fn proposal(
    status: &str,
    detail: Option<&str>,
    model: Option<&str>,
    deck: Option<&str>,
    field_templates: Option<&Map<String, Value>>,
) -> Value {
    json!({ "status": status, "detail": detail, "model": model, "deck": deck, "fieldTemplates": field_templates })
}

fn attention(detail: impl Into<String>) -> Value {
    proposal("needs-attention", Some(&detail.into()), None, None, None)
}

/// `modelMap`.
pub(crate) fn model_map(value: &Value) -> Result<&Map<String, Value>> {
    value.as_object().ok_or_else(|| Failure::new("AnkiConnect returned an invalid note type list."))
}

/// `fieldList`.
pub(crate) fn field_list(value: &Value) -> Result<Vec<String>> {
    let invalid = || Failure::new("AnkiConnect returned an invalid field list.");
    value
        .as_array()
        .ok_or_else(invalid)?
        .iter()
        .map(|field| field.as_str().filter(|field| !field.is_empty()).map(str::to_owned).ok_or_else(invalid))
        .collect()
}

/// `verifyAnkiSetup`: the saved mapping, checked as Settings checks it.
pub(crate) fn verify(invoke: &Invoke<'_>, config: &Map<String, Value>) -> Result<Value> {
    let models = invoke("modelNamesAndIds", json!({}), None)?;
    let models = model_map(&models)?;
    let decks = invoke("deckNames", json!({}), None)?;
    let decks: Vec<String> = decks
        .as_array()
        .filter(|decks| decks.iter().all(Value::is_string))
        .ok_or_else(|| Failure::new("AnkiConnect returned an invalid deck list."))?
        .iter()
        .filter_map(|deck| deck.as_str().map(str::to_owned))
        .collect();
    let model = config.get("model").and_then(Value::as_str).unwrap_or_default();
    let fields =
        if models.contains_key(model) { field_list(&invoke("modelFieldNames", json!({ "modelName": model }), None)?)? } else { Vec::new() };
    let discovery =
        Discovery { connected: true, model: model.to_owned(), models: models.keys().cloned().collect(), decks, fields, errors: Vec::new() };
    let errors = gateway::availability(config, Some(&discovery), None);
    Ok(match errors.into_iter().next() {
        None => proposal("already-configured", None, Some(model), config.get("deck").and_then(Value::as_str), None),
        Some(error) => attention(error),
    })
}

/// A setup candidate: a note type, its ID, family and fields.
type Candidate<'a> = (String, i64, &'a str, Vec<String>);

/// `detectAnkiSetup`: a proposal for an unconfigured profile.
pub(crate) fn detect(invoke: &Invoke<'_>, base: &Map<String, Value>) -> Result<Value> {
    let models = invoke("modelNamesAndIds", json!({}), None)?;
    // `eligibleCandidates`: each note type, its ID, family and fields, and its note count.
    let mut eligible: Vec<(Candidate<'_>, usize)> = Vec::new();
    for (model, id) in model_map(&models)? {
        let (Some(family), Some(id)) = (family(model), positive_id(id)) else { continue };
        let fields = field_list(&invoke("modelFieldNames", json!({ "modelName": model }), None)?)?;
        if setup_templates(family, model, "Default", &fields, base).is_none() {
            continue;
        }
        let notes = id_list(&invoke("findNotes", json!({ "query": format!("mid:{id}") }), None)?, "note IDs")?;
        eligible.push(((model.clone(), id, family, fields), distinct(&notes)));
    }
    if eligible.is_empty() {
        return Ok(attention("No Senren, Lapis or Kiku note type with its expected fields was found."));
    }
    let (best, tied) = unique_maximum(&eligible);
    let Some(((model, id, family, fields), _)) = best else { return Ok(attention("The supported note types have no notes yet.")) };
    if tied {
        return Ok(attention("Two note types share the highest note count."));
    }
    let cards = id_list(&invoke("findCards", json!({ "query": format!("mid:{id} -deck:filtered") }), None)?, "card IDs")?;
    if cards.is_empty() {
        return Ok(attention(format!("{model} has no cards in an ordinary deck.")));
    }
    let decks = invoke("getDecks", json!({ "cards": cards }), None)?;
    let decks = decks.as_object().ok_or_else(|| Failure::new("AnkiConnect returned an invalid deck grouping."))?;
    let mut counted: Vec<(String, usize)> = Vec::new();
    for (deck, deck_cards) in decks {
        let deck_cards = id_list(deck_cards, "deck card IDs")?;
        let notes = id_list(&invoke("cardsToNotes", json!({ "cards": deck_cards }), None)?, "deck note IDs")?;
        counted.push((deck.clone(), distinct(&notes)));
    }
    let (deck, tied) = unique_maximum(&counted);
    let Some((deck, _)) = deck else { return Ok(attention(format!("{model} has no notes in an ordinary deck."))) };
    if tied {
        return Ok(attention(format!("Two decks share the most {model} notes.")));
    }
    Ok(match setup_templates(family, model, deck, fields, base) {
        None => attention(format!("{model} does not match the {} field layout.", family_label(family))),
        Some(field_templates) => proposal("configured", None, Some(model), Some(deck), Some(&field_templates)),
    })
}

/// `ankiSetupFailure`.
fn failure(error: &Failure) -> Value {
    let lower = error.message.to_lowercase();
    let unavailable = lower.contains("open anki with the ankiconnect add-on") || lower.contains("timed out");
    json!({ "status": if unavailable { "unavailable" } else { "needs-attention" }, "detail": error.message, "model": null, "deck": null })
}

/// `checkAnkiSetup`: one read-only conversation with Anki, `{proposal, outcome}`.
pub(crate) fn check(invoke: &Invoke<'_>, anki: &Map<String, Value>) -> Value {
    let unconfigured = anki.get("model").and_then(Value::as_str) == Some("");
    match if unconfigured { detect(invoke, anki) } else { verify(invoke, anki) } {
        Ok(proposal) => {
            let outcome =
                json!({ "status": proposal["status"], "detail": proposal["detail"], "model": proposal["model"], "deck": proposal["deck"] });
            json!({ "proposal": proposal, "outcome": outcome })
        }
        Err(error) => json!({ "proposal": null, "outcome": failure(&error) }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_lead_the_name_and_end_at_a_word_boundary() {
        assert_eq!(family(" Kiku v2"), Some("kiku"));
        assert_eq!(family("Lapis 1.4"), Some("lapis"));
        assert_eq!(family("SENREN"), Some("senren"));
        assert_eq!(family("Kikuchi"), None);
        assert_eq!(family("My Kiku"), None);
        assert_eq!(family("\u{212a}iku"), Some("kiku"));
    }

    #[test]
    fn the_unique_maximum_needs_a_positive_count() {
        assert_eq!(unique_maximum(&[("a", 0), ("b", 0)]), (None, false));
        assert_eq!(unique_maximum(&[("a", 2), ("b", 3)]).0, Some(&("b", 3)));
        assert!(unique_maximum(&[("a", 3), ("b", 3)]).1);
        assert!(!unique_maximum(&[("a", 3), ("b", 3), ("c", 4)]).1);
    }
}
