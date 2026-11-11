//! Adding and removing a language (`addProfile` and `removeProfile`,
//! docs/languages.md 5.2, 6.3, 16.3 and 16.5). Under the mutation lock the
//! engine loads what enters the loaded set, the worker's `hd_profile_cas`
//! writes options and `dictionaryState` together, and the engine then
//! publishes.
//!
//! The extension reads its records back after a lost `hd_profile_cas` reply
//! (`profileCasReadback`). The relay's engine asks its worker in-process, so no
//! reply is lost and nothing is read back.

use serde_json::{Map, Value, json};

use super::{STORAGE_ATTEMPTS, Session, coded, error_or, is_true, list, paths, public_state};
use crate::host::engine::{languages, object, text};
use crate::host::profile::language_code;
use crate::host::registry;
use crate::host::settings::custom;
use crate::host::settings::profiles::{self, PROFILE_REMOVED, PROFILES_SCHEMA};
use crate::host::settings::worker::Storage;

/// What an added language leaves (`addedProfileState`): its profile, holding its
/// personal package when the library has one, and whether a package enters the
/// loaded set.
struct Added {
    next: Value,
    union: Vec<Value>,
    personal_id: Option<String>,
    base_revision: Value,
    entering: bool,
}

/// A removal request (`removedLanguage`).
struct Removal {
    erase: bool,
    profile_id: Option<String>,
    language: String,
    confirmed: Option<Value>,
}

/// `addedLanguage`.
fn added_language(message: &Value) -> super::Result<String> {
    let language = text(message.get("language"));
    if !language_code(language) {
        return Err("the language request named no language".to_owned());
    }
    if message.get("copyFrom").is_some_and(|copy| !copy.is_string()) {
        return Err("the language request named an invalid profile to copy".to_owned());
    }
    Ok(language.to_owned())
}

/// `removedLanguage`.
fn removed_language(message: &Value) -> super::Result<Removal> {
    let erase = message.get("erase") == Some(&Value::Bool(true));
    let profile_id = match message.get("profileId") {
        None => None,
        Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
        Some(_) => return Err("the language request named an invalid profile".to_owned()),
    };
    let language = profile_id.clone().unwrap_or_else(|| text(message.get("language")).to_owned());
    if !language_code(&language) || (profile_id.is_none() && !erase) {
        return Err("the language request named no language".to_owned());
    }
    // An erase carries the document revision its confirmation read; a newer
    // document is kept.
    let confirmed = message.get("baseDocumentRevision").cloned();
    if erase && !confirmed.as_ref().and_then(|revision| crate::host::js::whole(Some(revision))).is_some_and(|revision| revision >= 0.0) {
        return Err("the erase carried no valid document revision".to_owned());
    }
    Ok(Removal { erase, profile_id, language, confirmed })
}

/// `changedAfterConfirmation`.
fn changed_after_confirmation(document: &Value) -> Map<String, Value> {
    object(json!({ "ok": false, "stale": true, "document": document,
        "error": "The personal dictionary changed after you confirmed, so it was kept. Review it and try again." }))
}

/// `profileFailure`.
fn profile_failure(reply: &Map<String, Value>, fallback: &str) -> Map<String, Value> {
    let state = reply.get("state").map_or(&Value::Null, public_state);
    object(json!({ "ok": false, "conflict": is_true(reply, "conflict"), "error": error_or(reply, fallback), "state": state }))
}

/// The languages a committed change leaves (`adoptProfileRecords`'s `recordLanguages`).
fn record_languages(reply: &Map<String, Value>) -> Value {
    reply.get("languages").filter(|languages| languages.is_array()).cloned().unwrap_or_else(|| json!([]))
}

/// `removedProfileState`: the records without the profile, and with an erase
/// without the language's personal package.
fn removed_profile_state(stored: &Value, removal: &Removal) -> Value {
    let personal = custom::identity(&removal.language).map(|identity| identity.id);
    let dictionaries: Vec<Value> = list(stored.get("dictionaries"))
        .iter()
        .filter(|row| !removal.erase || personal.as_deref().is_none_or(|id| row.get("id").and_then(Value::as_str) != Some(id)))
        .cloned()
        .collect();
    let kept: Vec<&Value> = dictionaries.iter().filter_map(|row| row.get("id")).collect();
    let profiles: Map<String, Value> = stored["profiles"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(id, _)| removal.profile_id.as_deref() != Some(id.as_str()))
        .map(|(id, profile)| {
            let mut profile = profile.as_object().cloned().unwrap_or_default();
            let entries: Vec<Value> = list(profile.get("dictionaries"))
                .iter()
                .filter(|entry| entry.get("id").is_some_and(|id| kept.contains(&id)))
                .cloned()
                .collect();
            profile.insert("dictionaries".to_owned(), Value::from(entries));
            (id.clone(), Value::Object(profile))
        })
        .collect();
    json!({ "schemaVersion": PROFILES_SCHEMA, "revision": stored["revision"], "dictionaries": dictionaries, "profiles": profiles })
}

impl<S: Storage> Session<'_, S> {
    /// `withTargetLanguages`.
    fn with_target_languages(&self, mut state: Value) -> Value {
        let rows: Vec<Value> = list(state.get("dictionaries")).iter().map(|row| self.with_target_language(row)).collect();
        state["dictionaries"] = Value::from(rows);
        state
    }

    /// `addedProfileState`.
    fn added_profile_state(&self, state: &Value, language: &str) -> Added {
        let stored = match state.get("stored") {
            Some(stored) => stored.clone(),
            None => self.with_target_languages(profiles::dictionary_state_with_profiles((!state.is_null()).then_some(state))),
        };
        let personal_id = custom::identity(language).map(|identity| identity.id);
        let own = match &personal_id {
            Some(id) if list(stored.get("dictionaries")).iter().any(|row| row.get("id").and_then(Value::as_str) == Some(id.as_str())) => {
                json!([{ "id": id, "enabled": true }])
            }
            _ => json!([]),
        };
        let mut profiles_map = stored["profiles"].as_object().cloned().unwrap_or_default();
        profiles_map.insert(language.to_owned(), json!({ "dictionaries": own, "groups": [] }));
        let next = json!({ "schemaVersion": PROFILES_SCHEMA, "revision": stored["revision"], "dictionaries": stored["dictionaries"],
            "profiles": profiles_map });
        let union = profiles::all_packages(Some(&next));
        let before: Vec<&str> = list(state.get("dictionaries"))
            .iter()
            .filter(|row| row.get("enabled") != Some(&Value::Bool(false)))
            .map(|row| text(row.get("path")))
            .collect();
        let entering = union.iter().any(|row| row.get("enabled") != Some(&Value::Bool(false)) && !before.contains(&text(row.get("path"))));
        let base_revision = state.get("revision").filter(|revision| !revision.is_null()).cloned().unwrap_or(json!(0));
        Added { next, union, personal_id, base_revision, entering }
    }

    /// What a committed change leaves loaded when no package entered the set: the
    /// committed records, with or without profiles (`adoptProfileRecords`).
    fn adopt_records(&mut self, converted: bool) {
        let committed = self.engine.store.get(crate::host::store::DICTIONARY_STATE).unwrap_or(Value::Null);
        self.loaded.profile_records = !converted;
        self.loaded.state = Some(committed);
    }

    /// `addProfile`.
    pub(super) fn add_profile(&mut self, message: &Value) -> super::Result<Map<String, Value>> {
        let language = added_language(message)?;
        languages::load(self.native, &language).map_err(|error| format!("{} could not be loaded. {error}", registry::name(&language)))?;
        self.loaded.language_failures.retain(|failure| failure["language"] != language.as_str());
        for attempt in 0..STORAGE_ATTEMPTS {
            let state = self.dictionary_state()?;
            let plan = self.added_profile_state(&state, &language);
            let (candidate, reply) = self.commit_added_profile(&state, &plan, &language, message)?;
            if is_true(&reply, "ok") {
                match candidate {
                    Some(candidate) => self.publish(candidate),
                    None => self.adopt_records(is_true(&reply, "converted")),
                }
                self.loaded.profile_records = !is_true(&reply, "converted");
                let committed = reply.get("state").map_or(&Value::Null, public_state);
                return Ok(object(json!({ "state": committed, "languages": record_languages(&reply) })));
            }
            if plan.entering {
                self.restore()?;
            }
            if !is_true(&reply, "conflict") || attempt + 1 >= STORAGE_ATTEMPTS {
                return Ok(profile_failure(&reply, "the language could not be added"));
            }
        }
        Err("the dictionary state kept changing while the language was added".to_owned())
    }

    /// `commitAddedProfile`: loads the packages that enter the set, then commits.
    /// A failure restores the committed set.
    fn commit_added_profile(
        &mut self,
        state: &Value,
        plan: &Added,
        language: &str,
        message: &Value,
    ) -> super::Result<(Option<crate::host::engine::Candidate>, Map<String, Value>)> {
        let attempted = (|| {
            let candidate = if plan.entering {
                let committed: Vec<Value> = list(state.get("dictionaries"))
                    .iter()
                    .filter(|row| {
                        row.get("enabled") != Some(&Value::Bool(false))
                            || row.get("id").and_then(Value::as_str) != plan.personal_id.as_deref()
                    })
                    .cloned()
                    .collect();
                Some(self.preload(&plan.union, &paths(&committed))?)
            } else {
                None
            };
            let mut fields = object(json!({ "action": "add", "language": language, "baseRevision": plan.base_revision,
                "state": { "dictionaries": plan.next["dictionaries"], "profiles": plan.next["profiles"] } }));
            if let Some(copy) = message.get("copyFrom") {
                fields.insert("copyFrom".to_owned(), copy.clone());
            }
            Ok::<_, String>((candidate, self.ask("hd_profile_cas", Value::Object(fields))))
        })();
        attempted.inspect_err(|_| {
            if plan.entering {
                // `restoreAfterFailedLoad`: a failed restore is reported by hd_status.
                let _ = self.restore();
            }
        })
    }

    /// `removeProfile`: the profile's settings and list go, its packages stay in
    /// the library, and the engine unloads what left the set once the commit is
    /// in. With `erase` the same commit empties the language's personal source and
    /// removes its package, counts and marks.
    pub(super) fn remove_profile(&mut self, message: &Value) -> super::Result<Map<String, Value>> {
        let removal = removed_language(message)?;
        for attempt in 0..STORAGE_ATTEMPTS {
            let stored = self.stored_for_removal(&removal)?;
            let next = removed_profile_state(&stored, &removal);
            // `eraseConfirmation`: an erase removes the personal source its
            // confirmation read; a newer one is kept and the removal stops.
            let document_revision = if removal.erase {
                let snapshot = self.custom_storage(Some(&removal.language))?;
                if snapshot.revision() != removal.confirmed.as_ref() {
                    return Ok(changed_after_confirmation(&snapshot.document));
                }
                removal.confirmed.clone()
            } else {
                None
            };
            let mut fields = object(json!({ "action": "remove", "language": removal.language, "erase": removal.erase,
                "baseRevision": stored["revision"],
                "state": { "dictionaries": next["dictionaries"], "profiles": next["profiles"] } }));
            if let Some(id) = &removal.profile_id {
                fields.insert("profileId".to_owned(), Value::from(id.as_str()));
            }
            if let Some(revision) = document_revision {
                fields.insert("baseDocumentRevision".to_owned(), revision);
            }
            let reply = self.ask("hd_profile_cas", Value::Object(fields));
            // `removalOutcome`.
            if is_true(&reply, "ok") {
                return self.adopt_removed_profile(&reply, &stored, &next, removal.erase);
            }
            if is_true(&reply, "stale") {
                let mut stale = changed_after_confirmation(reply.get("document").unwrap_or(&Value::Null));
                stale.insert("state".to_owned(), reply.get("state").map_or(&Value::Null, public_state).clone());
                return Ok(stale);
            }
            if !is_true(&reply, "conflict") || attempt + 1 >= STORAGE_ATTEMPTS {
                return Ok(profile_failure(&reply, "the language could not be removed"));
            }
        }
        Err("the dictionary state kept changing while the language was removed".to_owned())
    }

    /// `storedForRemoval`.
    fn stored_for_removal(&self, removal: &Removal) -> super::Result<Value> {
        let state = self.dictionary_state()?;
        let Some(stored) = state.get("stored") else { return Err("The last language cannot be removed.".to_owned()) };
        if removal.profile_id.as_ref().is_some_and(|id| !stored["profiles"].get(id).is_some_and(Value::is_object)) {
            return Err(coded(PROFILE_REMOVED, "profile-missing"));
        }
        Ok(stored.clone())
    }

    /// `adoptRemovedProfile`: once a removal commits, the engine loads the
    /// committed set; an erase then removes the package it dropped.
    fn adopt_removed_profile(
        &mut self,
        reply: &Map<String, Value>,
        stored: &Value,
        next: &Value,
        erase: bool,
    ) -> super::Result<Map<String, Value>> {
        self.restore()?;
        if erase {
            let kept = paths(list(next.get("dictionaries")));
            let dropped: Vec<&str> = paths(list(stored.get("dictionaries"))).into_iter().filter(|path| !kept.contains(path)).collect();
            self.cleanup(&dropped);
        }
        let committed = reply.get("state").map_or(&Value::Null, public_state);
        let mut answer = object(json!({ "state": committed }));
        if !is_true(reply, "converted") {
            answer.insert("languages".to_owned(), record_languages(reply));
        }
        Ok(answer)
    }
}
