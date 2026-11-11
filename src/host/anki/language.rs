// SPDX-License-Identifier: GPL-3.0-or-later
//! The language a note is mined in (upstream docs/languages.md 10 and 12):
//! `noteLanguage` of `anki-resources.js`, with the parts of `createLanguage`
//! (theme-host.js) and of render/glossary.js's text-language helpers that a
//! note reads. It exists only once the records have profiles; without one
//! every note is the single-profile shape's.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

use icu_properties::props::{GeneralCategory, GeneralCategoryGroup, Script};
use icu_properties::{CodePointMapData, PropertyNamesShort};
use serde_json::{Map, Value};

use super::dom::{Element, Node, deep};
use crate::host::profile::JAPANESE;
use crate::host::settings::profiles;
use crate::host::{js, registry};

/// Unicode's right-to-left scripts (ISO 15924), as far as Chrome 128 knows them.
const RTL_SCRIPTS: [&str; 35] = [
    "Adlm", "Arab", "Armi", "Avst", "Chrs", "Cprt", "Elym", "Hatr", "Hebr", "Hung", "Khar", "Lydi", "Mand", "Mani", "Mend", "Merc", "Mero",
    "Narb", "Nbat", "Nkoo", "Orkh", "Ougr", "Palm", "Phli", "Phlp", "Phnx", "Prti", "Rohg", "Samr", "Sarb", "Sogd", "Sogo", "Syrc", "Thaa",
    "Yezi",
];
/// Codes that are Han alone, and every code that includes Han (7.3, 10.3).
const HAN_ONLY_SCRIPTS: [&str; 4] = ["Hani", "Hans", "Hant", "Hanb"];
const HAN_SCRIPTS: [&str; 6] = ["Hani", "Hans", "Hant", "Hanb", "Jpan", "Kore"];
const HAN_LANGUAGES: [&str; 3] = ["ja", "ko", "zh"];
/// Structured blocks for the paragraph rule (10.2).
const DIRECTION_BLOCK_TAGS: [&str; 10] = ["div", "p", "li", "ol", "ul", "table", "td", "th", "details", "summary"];

/// The ISO 15924 code of a character's script.
fn script(c: char) -> &'static str {
    PropertyNamesShort::<Script>::new().get(CodePointMapData::<Script>::new().get(c)).unwrap_or_default()
}

/// `\p{L}`.
fn is_letter(c: char) -> bool {
    GeneralCategoryGroup::Letter.contains(CodePointMapData::<GeneralCategory>::new().get(c))
}

/// `\p{M}`.
fn is_mark(c: char) -> bool {
    GeneralCategoryGroup::Mark.contains(CodePointMapData::<GeneralCategory>::new().get(c))
}

/// `RTL_LETTER`: a letter in a right-to-left script.
fn is_rtl_letter(c: char) -> bool {
    is_letter(c) && RTL_SCRIPTS.contains(&script(c))
}

/// `hasRtlText`.
pub(crate) fn has_rtl_text(text: &str) -> bool {
    text.chars().any(is_rtl_letter)
}

/// The letters of `text` and how many of them are right-to-left letters.
fn letter_counts(text: &str) -> (usize, usize) {
    text.chars().filter(|c| is_letter(*c)).fold((0, 0), |(all, rtl), c| (all + 1, rtl + usize::from(RTL_SCRIPTS.contains(&script(c)))))
}

/// `textDirection`, the majority rule (10.2): right to left when more than
/// half of the letters are right-to-left letters; text without letters has
/// no direction.
pub(crate) fn text_direction(text: &str) -> Option<&'static str> {
    let (letters, rtl) = letter_counts(text);
    if letters == 0 {
        return None;
    }
    Some(if rtl * 2 > letters { "rtl" } else { "ltr" })
}

fn script_direction(script: &str) -> &'static str {
    if RTL_SCRIPTS.contains(&script) { "rtl" } else { "ltr" }
}

/// `localeScript`: `new Intl.Locale(tag).maximize().script`, from a script
/// subtag the tag carries, else the platform's likely script for its
/// language (scripts/language-names.mjs). A region does not change it here.
fn locale_script(tag: &str) -> Option<String> {
    static LIKELY: OnceLock<HashMap<String, String>> = OnceLock::new();
    let likely = LIKELY.get_or_init(|| {
        let table: Value = serde_json::from_str(include_str!("../language-names.json")).unwrap_or_default();
        let mut likely = HashMap::new();
        for (script, codes) in table["scripts"].as_object().into_iter().flatten() {
            for code in codes.as_str().unwrap_or_default().split(' ') {
                likely.insert(code.to_owned(), script.clone());
            }
        }
        likely
    });
    let subtags: Vec<&str> = tag.split('-').collect();
    let alpha = |subtag: &str, lengths: &[usize]| lengths.contains(&subtag.len()) && subtag.bytes().all(|byte| byte.is_ascii_alphabetic());
    if !alpha(subtags[0], &[2, 3, 5, 6, 7, 8])
        || subtags[1..]
            .iter()
            .any(|subtag| subtag.is_empty() || subtag.len() > 8 || !subtag.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    {
        return None;
    }
    if let Some(script) = subtags.get(1).filter(|subtag| alpha(subtag, &[4])) {
        let mut script = script.to_ascii_lowercase();
        script[..1].make_ascii_uppercase();
        return Some(script);
    }
    likely.get(&subtags[0].to_ascii_lowercase()).cloned()
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
}

/// `targetLanguage`: a dictionary's target language (10.2, 10.3 steps 4 and
/// 5): the pack's scripts and direction when this build has one, else the
/// platform's script. `direction` is `None` when neither knows the language.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Target {
    pub(crate) tag: String,
    pub(crate) direction: Option<&'static str>,
    latin: Option<bool>,
    han: bool,
}

pub(crate) fn target_language(tag: Option<&str>) -> Option<Target> {
    let tag = tag.filter(|tag| !tag.is_empty())?;
    let primary = tag.split(['-', '_']).next().unwrap_or_default().to_lowercase();
    let pack = registry::get(&primary);
    let script = if pack.is_some() { None } else { locale_script(tag) };
    let scripts = pack.map(|pack| strings(pack.get("scripts"))).or_else(|| script.clone().map(|script| vec![script]));
    let mut direction = script.as_deref().map(script_direction);
    match pack.and_then(|pack| pack["direction"].as_str()) {
        Some("rtl") => direction = Some("rtl"),
        Some("ltr") => direction = Some("ltr"),
        _ => {}
    }
    Some(Target {
        tag: tag.to_owned(),
        direction,
        latin: scripts.as_ref().map(|scripts| scripts.iter().any(|code| code == "Latn")),
        han: HAN_LANGUAGES.contains(&primary.as_str())
            || scripts.is_some_and(|scripts| scripts.iter().any(|code| HAN_SCRIPTS.contains(&code.as_str()))),
    })
}

/// A pack's text pattern: one character class of characters, ranges and `\p{M}`.
#[derive(Debug, Default)]
struct CharClass {
    ranges: Vec<(char, char)>,
    marks: bool,
}

impl CharClass {
    /// The class, or `None` for a pattern this reader does not know.
    fn parse(pattern: &str) -> Option<Self> {
        let body = pattern.strip_prefix('[')?.strip_suffix(']')?;
        let chars: Vec<char> = body.chars().collect();
        let mut class = Self::default();
        let mut at = 0;
        // One member: a character, or `\p{M}` (`None`).
        let member = |at: &mut usize| -> Option<Option<char>> {
            let c = *chars.get(*at)?;
            *at += 1;
            if c != '\\' {
                return (c != '[' && c != ']').then_some(Some(c));
            }
            let escaped = *chars.get(*at)?;
            *at += 1;
            match escaped {
                'u' if chars.get(*at) == Some(&'{') => {
                    let end = chars[*at..].iter().position(|c| *c == '}')? + *at;
                    let hex: String = chars[*at + 1..end].iter().collect();
                    *at = end + 1;
                    Some(Some(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?))
                }
                'u' => {
                    let hex: String = chars.get(*at..*at + 4)?.iter().collect();
                    *at += 4;
                    Some(Some(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?))
                }
                'p' => {
                    let rest: String = chars.get(*at..*at + 3)?.iter().collect();
                    *at += 3;
                    (rest == "{M}").then_some(None)
                }
                c if c.is_ascii_alphanumeric() => None,
                c => Some(Some(c)),
            }
        };
        while at < chars.len() {
            let Some(first) = member(&mut at)? else {
                class.marks = true;
                continue;
            };
            if chars.get(at) == Some(&'-') && at + 1 < chars.len() {
                at += 1;
                let last = member(&mut at)??;
                if last < first {
                    return None;
                }
                class.ranges.push((first, last));
            } else {
                class.ranges.push((first, first));
            }
        }
        Some(class)
    }

    fn contains(&self, c: char) -> bool {
        self.ranges.iter().any(|(first, last)| (*first..=*last).contains(&c)) || (self.marks && is_mark(c))
    }
}

/// `textLanguage`: a pack as detection reads it: its tag, scripts and the
/// characters of its text pattern that are not Latin (10.3 step 3; Han counts
/// only for a pack whose every non-Latin script is a Han script).
struct TextInfo {
    bcp47: String,
    native: Option<CharClass>,
    /// Whether the native characters leave Han out.
    without_han: bool,
    latin_only: bool,
    han: bool,
}

impl TextInfo {
    fn of(pack: &Value) -> Self {
        let scripts = strings(pack.get("scripts"));
        let non_latin: Vec<&String> = scripts.iter().filter(|code| *code != "Latn").collect();
        let native =
            (!non_latin.is_empty()).then(|| pack.pointer("/text/pattern").and_then(Value::as_str).and_then(CharClass::parse)).flatten();
        let code = js::to_string(pack.get("code"));
        Self {
            bcp47: pack.get("bcp47").and_then(Value::as_str).filter(|tag| !tag.is_empty()).map_or(code, str::to_owned),
            native,
            without_han: !non_latin.iter().all(|code| HAN_ONLY_SCRIPTS.contains(&code.as_str())),
            latin_only: !scripts.is_empty() && non_latin.is_empty(),
            han: scripts.iter().any(|code| HAN_SCRIPTS.contains(&code.as_str())),
        }
    }

    fn native(&self, text: &str) -> bool {
        self.native.as_ref().is_some_and(|class| {
            text.chars().any(|c| {
                let script = script(c);
                script != "Latn" && !(self.without_han && script == "Hani") && class.contains(c)
            })
        })
    }
}

/// `hanAlone`: Han or CJK punctuation, and no letter of another script.
fn han_alone(text: &str) -> bool {
    let han = |c: char| script(c) == "Hani" || matches!(c, '\u{3000}'..='\u{303f}' | '\u{fe30}'..='\u{fe4f}' | '\u{ff01}'..='\u{ff65}');
    text.chars().any(han) && !text.chars().any(|c| is_letter(c) && script(c) != "Hani")
}

/// `scriptLanguage`: steps 2 to 4 of 10.3, which read only the text and the profile.
fn script_language(text: &str, profile: &TextInfo, target: Option<&Target>) -> Option<String> {
    let has = |wanted: &[&str]| text.chars().any(|c| is_letter(c) && wanted.contains(&script(c)));
    if has(&["Hira", "Kana"]) {
        return Some("ja".to_owned());
    }
    if has(&["Hang"]) {
        return Some("ko".to_owned());
    }
    if has(&["Bopo"]) {
        return Some("zh".to_owned());
    }
    if profile.native(text) {
        return Some(profile.bcp47.clone());
    }
    if profile.latin_only
        && target.is_some_and(|target| target.latin == Some(false))
        && text.chars().any(is_letter)
        && !text.chars().any(|c| is_letter(c) && script(c) != "Latn")
    {
        return Some(profile.bcp47.clone());
    }
    None
}

/// `glossaryTextLanguage`: 10.3 for one glossary string outside the Japanese
/// profile, steps 2 to 7, where step 5 is the dictionary's target language.
fn glossary_text_language(text: &str, profile: &TextInfo, target: Option<&Target>) -> Option<String> {
    if let Some(detected) = script_language(text, profile, target) {
        return Some(detected);
    }
    let alone = han_alone(text);
    if let Some(target) = target
        && (!alone || target.han)
    {
        return Some(target.tag.clone());
    }
    (alone && profile.han).then(|| profile.bcp47.clone())
}

/// `JAPANESE_TEXT_PATTERN`.
fn is_japanese(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{30ff}' | '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{f900}'..='\u{faff}'
        | '\u{ff01}'..='\u{ff1f}' | '\u{ff21}'..='\u{ff3f}' | '\u{ff41}'..='\u{ff9f}' | '\u{ffe0}'..='\u{ffee}'
        | '\u{20000}'..='\u{2a6df}' | '\u{2a700}'..='\u{2ee5f}' | '\u{2f800}'..='\u{2fa1f}' | '\u{30000}'..='\u{323af}')
}

/// `CHINESE_TEXT_PATTERN`.
fn is_chinese(c: char) -> bool {
    matches!(c, '\u{3100}'..='\u{312f}' | '\u{31a0}'..='\u{31bf}' | '\u{fe10}'..='\u{fe1f}' | '\u{fe50}'..='\u{fe6f}' | '\u{16fe0}'..='\u{16fff}')
}

/// `languageFromText`: today's detection.
pub(crate) fn language_from_text(text: &str) -> Option<&'static str> {
    if text.chars().any(is_japanese) {
        Some("ja")
    } else if text.chars().any(is_chinese) {
        Some("zh")
    } else {
        None
    }
}

/// `glossStrings`: the strings of gloss rows, text and structured content
/// without images.
fn gloss_strings(glossaries: &[&Value]) -> Vec<String> {
    let mut strings = Vec::new();
    let mut stack: Vec<Value> = Vec::new();
    for row in glossaries {
        match row.get("glossary") {
            Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
                Ok(parsed) => stack.push(parsed),
                Err(_) => strings.push(text.clone()),
            },
            Some(other) => stack.push(other.clone()),
            None => stack.push(Value::Null),
        }
    }
    while let Some(value) = stack.pop() {
        match value {
            Value::String(text) => strings.push(text),
            Value::Array(items) => stack.extend(items),
            Value::Object(mut record) => {
                if record.get("type").and_then(Value::as_str) == Some("text") {
                    stack.push(record.remove("text").unwrap_or(Value::Null));
                } else if record.get("tag").and_then(Value::as_str) != Some("img")
                    && record.get("type").and_then(Value::as_str) != Some("image")
                {
                    stack.push(record.remove("content").unwrap_or(Value::Null));
                }
            }
            _ => {}
        }
    }
    strings
}

/// `glossDirection`: the letters of one dictionary's gloss strings in a result.
fn gloss_direction(glossaries: &[&Value]) -> &'static str {
    if !glossaries.iter().any(|row| has_rtl_text(&js::to_string(row.get("glossary")))) {
        return "ltr";
    }
    text_direction(&gloss_strings(glossaries).join(" ")).unwrap_or("ltr")
}

/// `noteLanguage`'s language tags: `[A-Za-z0-9-]{1,35}`.
pub(crate) fn language_tag(tag: &str) -> bool {
    (1..=35).contains(&tag.len()) && tag.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// The language of the profile a note is mined in, knowing the target
/// language each of the request's dictionaries declares.
pub(crate) struct NoteLanguage {
    pub(crate) direction: &'static str,
    /// `capabilities.ruby`, with the profile's pack settings.
    pub(crate) ruby: bool,
    japanese: bool,
    info: TextInfo,
    targets: HashMap<String, Option<Target>>,
    pack: &'static Value,
    directions: RefCell<HashMap<String, &'static str>>,
}

impl NoteLanguage {
    /// `noteLanguage`: `None` in the single-profile shape, whose notes are
    /// today's. A Japanese-profile note takes no dictionary's target language
    /// as its body's default.
    pub(crate) fn of(profile: Option<&Value>, request: &Value) -> Option<Self> {
        let profile = profile.filter(|profile| profile.is_object())?;
        let language = profile["language"].as_str().unwrap_or_default();
        let pack = registry::get(language).or_else(|| registry::get(JAPANESE))?;
        let code = js::to_string(pack.get("code"));
        let pack_settings = profile.get("packSettings").and_then(Value::as_object).cloned().unwrap_or_default();
        let targets = if language == JAPANESE { None } else { request.get("dictionaryTargetLanguages").and_then(Value::as_object) };
        let targets = targets
            .into_iter()
            .flatten()
            .filter_map(|(title, tag)| Some((title.clone(), tag.as_str().filter(|tag| language_tag(tag))?)))
            .map(|(title, tag)| (title, target_language(Some(tag))))
            .collect();
        Some(Self {
            direction: if pack["direction"] == "rtl" { "rtl" } else { "ltr" },
            ruby: profiles::option_requirement_met(&Value::from("ruby"), Some(&code), &pack_settings),
            japanese: code == JAPANESE,
            info: TextInfo::of(pack),
            targets,
            pack,
            directions: RefCell::new(HashMap::new()),
        })
    }

    fn target(&self, title: &str) -> Option<&Target> {
        self.targets.get(title).and_then(Option::as_ref)
    }

    /// `dictionaryDirection`: its target language's, else the majority of the
    /// letters in its own gloss strings in the result.
    pub(crate) fn dictionary_direction(&self, title: &str, request: &Value) -> &'static str {
        if let Some(known) = self.target(title).and_then(|target| target.direction) {
            return known;
        }
        if let Some(known) = self.directions.borrow().get(title) {
            return known;
        }
        let rows: Vec<&Value> = request["term"]["glossaries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|row| row.get("dictionary").and_then(Value::as_str) == Some(title))
            .collect();
        let direction = if request["term"].is_object() { gloss_direction(&rows) } else { "ltr" };
        self.directions.borrow_mut().insert(title.to_owned(), direction);
        direction
    }

    /// `dictionaryLanguage`.
    pub(crate) fn dictionary_language(&self, title: &str) -> Option<String> {
        self.target(title).map(|target| target.tag.clone())
    }

    /// `detectGlossary`: 10.3 for one glossary string; the Japanese profile
    /// keeps today's detection.
    pub(crate) fn detect_glossary(&self, text: &str, title: &str) -> Option<String> {
        if self.japanese {
            return language_from_text(text).map(str::to_owned);
        }
        glossary_text_language(text, &self.info, self.target(title))
    }

    /// `partOfSpeechLabels`: the pack's labels, else the Japanese table for
    /// Japanese and none elsewhere (`None`).
    pub(crate) fn part_of_speech_labels(&self) -> Option<&'static Map<String, Value>> {
        self.pack.pointer("/host/anki/partOfSpeechLabels").and_then(Value::as_object)
    }

    pub(crate) fn is_japanese(&self) -> bool {
        self.japanese
    }
}

/// A block's letters (`counts` of `markBlockDirections`): its own text
/// outside nested blocks, and all of it; one per child element that has any.
struct Counts {
    own: usize,
    own_rtl: usize,
    all: usize,
    all_rtl: usize,
    children: Vec<Option<Self>>,
}

fn is_direction_block(element: &Element) -> bool {
    DIRECTION_BLOCK_TAGS.contains(&element.name.as_str())
}

fn counts(element: &Element) -> Counts {
    deep(|| {
        let mut counted = Counts { own: 0, own_rtl: 0, all: 0, all_rtl: 0, children: Vec::new() };
        for child in &element.children {
            match child {
                Node::Text(text) => {
                    let (letters, rtl) = letter_counts(text);
                    counted.own += letters;
                    counted.own_rtl += rtl;
                    counted.all += letters;
                    counted.all_rtl += rtl;
                }
                Node::Element(child) => {
                    let inner = counts(child);
                    counted.all += inner.all;
                    counted.all_rtl += inner.all_rtl;
                    if !is_direction_block(child) {
                        counted.own += inner.own;
                        counted.own_rtl += inner.own_rtl;
                    }
                    counted.children.push((inner.all > 0).then_some(inner));
                }
                Node::Raw(_) => {}
            }
        }
        counted
    })
}

impl Counts {
    fn direction(&self) -> Option<&'static str> {
        let (letters, rtl) = if self.own > 0 { (self.own, self.own_rtl) } else { (self.all, self.all_rtl) };
        (letters > 0).then_some(if rtl * 2 > letters { "rtl" } else { "ltr" })
    }
}

/// `markBlockDirections`, 10.2's paragraph rule over rendered glossary
/// content: a block takes the direction of its own letters, or of all its
/// letters when it has none of its own, and is marked with `dir` where that
/// differs from the direction it inherits. It gets `text-align: start` unless
/// a dictionary style on it or an ancestor inside the glossary aligns it. A
/// list that holds an item of the other direction gets `list_padding` as its
/// inline padding.
pub(crate) fn mark_block_directions(root: &mut Element, direction: &str, list_padding: &str) {
    let counted = counts(root);
    if counted.all == 0 {
        return;
    }
    mark(root, &counted, direction, false, true, list_padding);
}

/// One block of the walk; whether its list should be padded.
fn mark(element: &mut Element, counted: &Counts, inherited: &str, aligned_above: bool, root: bool, list_padding: &str) -> bool {
    deep(|| {
        let styled = aligned_above || !element.style_value("text-align").is_empty();
        let mut current = inherited.to_owned();
        let mut pad_parent = false;
        if !root
            && is_direction_block(element)
            && let Some(own) = counted.direction().filter(|own| *own != inherited)
        {
            element.set_attribute("dir", own);
            if !styled {
                element.set_style("text-align", "start");
            }
            own.clone_into(&mut current);
            pad_parent = element.name == "li";
        }
        let list = matches!(element.name.as_str(), "ol" | "ul");
        let mut pad = false;
        let mut children = counted.children.iter();
        for child in &mut element.children {
            let Node::Element(child) = child else { continue };
            if let Some(Some(inner)) = children.next() {
                pad |= mark(child, inner, &current, styled, false, list_padding) && list;
            }
        }
        if pad {
            element.set_style("padding-inline", list_padding);
        }
        pad_parent
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::glossary::{Layout, Renderer};
    use super::super::values;
    use super::*;

    /// The fields the extension's `buildAnkiResourceFields` made for
    /// upstream's language scenarios (scripts/anki-language-cases.mjs).
    #[test]
    fn notes_match_the_extension_in_every_profile() {
        let cases: Vec<Value> = serde_json::from_str(include_str!("testdata/languages.json")).unwrap();
        assert_eq!(cases.len(), 30);
        for case in &cases {
            let request = &case["request"];
            let templates: Vec<(String, Map<String, Value>)> = case["templates"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, template)| (name.clone(), template.as_object().unwrap().clone()))
                .collect();
            let language = NoteLanguage::of(case.get("profile"), request);
            let layout = Layout { compact: case["compact"] == true, compact_glossaries: false };
            let mut renderer = Renderer::new(
                request,
                layout,
                Box::new(|| Ok(vec![])),
                Box::new(|dictionary: &str, path: &str| Ok(format!("{dictionary}/{path}"))),
            )
            .in_language(language.as_ref());
            let mut definition = |wanted: &_| renderer.render(wanted);
            let fields = values::build_fields(request, &templates, &mut definition, "", language.as_ref()).unwrap();
            let label = format!("{} profile={} compact={}", case["name"], case["profile"], case["compact"]);
            for (name, value) in fields {
                assert_eq!(value, case["fields"][&name].as_str().unwrap(), "{label}: {name}");
            }
        }
    }

    #[test]
    fn directions_follow_the_majority_of_letters() {
        assert!(has_rtl_text("abc שלום"));
        assert!(!has_rtl_text("abc ١٢٣"), "Arabic digits are no letters");
        assert_eq!(text_direction("שלום abc"), Some("rtl"));
        assert_eq!(text_direction("a שלום bcdefg"), Some("ltr"));
        assert_eq!(text_direction("123"), None);
    }

    #[test]
    fn a_target_language_comes_from_its_pack_or_the_platform() {
        let arabic = target_language(Some("ar")).unwrap();
        assert_eq!((arabic.direction, arabic.latin, arabic.han), (Some("rtl"), Some(false), false));
        let hebrew = target_language(Some("he")).unwrap();
        assert_eq!((hebrew.direction, hebrew.latin), (Some("rtl"), Some(false)));
        assert!(target_language(Some("zh-Hant")).unwrap().han);
        assert_eq!(target_language(Some("sr-Latn")).unwrap().latin, Some(true));
        assert_eq!(target_language(Some("x_y")).unwrap().direction, None, "Intl.Locale refuses it");
        assert_eq!(target_language(Some("")), None);
    }

    #[test]
    fn every_pack_pattern_is_read() {
        for code in registry::codes() {
            let pack = registry::get(code).unwrap();
            if strings(pack.get("scripts")).iter().any(|script| script != "Latn") {
                assert!(TextInfo::of(pack).native.is_some(), "{code}");
            }
        }
    }

    #[test]
    fn glossary_text_takes_its_language_by_script_then_target() {
        let request = json!({ "dictionaryTargetLanguages": { "Wiktionary": "en", "Bad": "e n" } });
        let arabic = NoteLanguage::of(Some(&json!({ "language": "ar", "packSettings": {} })), &request).unwrap();
        assert_eq!((arabic.direction, arabic.ruby), ("rtl", false));
        assert_eq!(arabic.detect_glossary("كتاب", "Wiktionary").as_deref(), Some("ar"));
        assert_eq!(arabic.detect_glossary("book", "Wiktionary").as_deref(), Some("en"));
        assert_eq!(arabic.detect_glossary("ねこ", "Wiktionary").as_deref(), Some("ja"));
        assert_eq!(arabic.detect_glossary("book", "Bad"), None);
        assert_eq!(arabic.dictionary_language("Wiktionary").as_deref(), Some("en"));
        let english =
            NoteLanguage::of(Some(&json!({ "language": "en" })), &json!({ "dictionaryTargetLanguages": { "Ru": "ru" } })).unwrap();
        assert_eq!(english.detect_glossary("cat", "Ru").as_deref(), Some("en"), "Latin text beside a non-Latin target");
        let japanese = NoteLanguage::of(Some(&json!({ "language": "ja" })), &request).unwrap();
        assert!(japanese.ruby);
        assert_eq!(japanese.dictionary_language("Wiktionary"), None, "Japanese keeps today's markup");
        assert_eq!(japanese.detect_glossary("猫", "Wiktionary").as_deref(), Some("ja"));
        assert!(NoteLanguage::of(None, &request).is_none());
    }

    #[test]
    fn a_dictionary_without_target_takes_the_direction_of_its_glosses() {
        let request = json!({ "term": { "glossaries": [
            { "dictionary": "He", "glossary": "[\"חתול\", {\"type\": \"structured-content\", \"content\": \"cat חתול\"}]" },
            { "dictionary": "En", "glossary": "[\"cat\"]" },
        ] } });
        let language = NoteLanguage::of(Some(&json!({ "language": "yi" })), &request).unwrap();
        assert_eq!(language.dictionary_direction("He", &request), "rtl");
        assert_eq!(language.dictionary_direction("En", &request), "ltr");
    }

    #[test]
    fn blocks_of_the_other_direction_are_marked() {
        let mut root = Element::new("div");
        let mut list = Element::new("ul");
        let mut hebrew = Element::new("li");
        hebrew.append_text("שלום עולם");
        let mut english = Element::new("li");
        english.append_text("hello wonderful world");
        list.append(hebrew);
        list.append(english);
        root.append(list);
        mark_block_directions(&mut root, "ltr", "40px");
        assert_eq!(
            root.outer_html(),
            "<div><ul style=\"padding-inline: 40px;\"><li dir=\"rtl\" style=\"text-align: start;\">שלום עולם</li><li>hello wonderful world</li></ul></div>"
        );
    }
}
