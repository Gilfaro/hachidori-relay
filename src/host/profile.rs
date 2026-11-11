// SPDX-License-Identifier: GPL-3.0-or-later
//! What every layer reads of language profiles: the Japanese profile's id, the
//! form of a language code, and the opt-in request's type (upstream
//! docs/languages.md 5.2 and 17.3).

/// `JAPANESE_PROFILE_ID`: the only profile of records without profiles.
pub(crate) const JAPANESE: &str = "ja";
/// `customProfileLanguage`'s refusal on records without profiles.
pub(crate) const NO_LANGUAGE_PROFILE: &str = "This language has no profile on this Hachidori.";

/// `LANGUAGE_PROFILES_OPT_IN`: a profile-aware browser's request to receive the
/// records as stored.
pub(crate) const OPT_IN: &str = "hd_language_profiles_opt_in";

/// `LANGUAGE_CODE`.
pub(crate) fn language_code(code: &str) -> bool {
    (2..=3).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase())
}
