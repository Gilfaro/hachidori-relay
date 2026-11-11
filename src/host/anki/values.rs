// SPDX-License-Identifier: GPL-3.0-or-later
//! Marker values: the extension's `buildAnkiFields` (`anki-values.js`), with its
//! pitch graphs (`anki-pitch.js`) and the pitch and frequency helpers of
//! `HDGlossary`. Only the values the selected templates use are built.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::duplicates::Fields;
use super::gateway::{Failure, Result};
use super::glossary::{Wanted, definition_tags, tag_list};
use super::language::{NoteLanguage, has_rtl_text};
use super::templates::{escape_html, marker_names, render};
use crate::host::js;
use crate::host::lookup::furigana::{Piece, matched_furigana, term_furigana};

const GLOSSARY_MARKER_PREFIX: &str = "single-glossary-";
const GLOSSARY_VARIANTS: [(&str, Wanted); 4] = [
    ("-brief", Wanted { dictionary: None, first_only: false, brief: true, no_dictionary: false, plain: false }),
    ("-no-dictionary", Wanted { dictionary: None, first_only: false, brief: false, no_dictionary: true, plain: false }),
    ("-plain", Wanted { dictionary: None, first_only: false, brief: false, no_dictionary: false, plain: true }),
    ("-plain-no-dictionary", Wanted { dictionary: None, first_only: false, brief: false, no_dictionary: true, plain: true }),
];
const PARTS_OF_SPEECH: [(&str, &str); 7] = [
    ("v1", "Ichidan verb"),
    ("v5", "Godan verb"),
    ("vk", "Kuru verb"),
    ("vs", "Suru verb"),
    ("vz", "Zuru verb"),
    ("adj-i", "I-adjective"),
    ("n", "Noun"),
];
const VALUE_ALIASES: [(&str, &str); 6] = [
    ("definition", "glossary"),
    ("main-definition", "glossary-first"),
    ("jpmn-primary-definition", "glossary-first"),
    ("frequency", "frequencies"),
    ("pitch-accent-positions", "pitch-position"),
    ("pitch-categories", "pitch-accent-categories"),
];

/// `escapeAnkiHtml(value)`, with `String(value)`.
fn escape(value: Option<&Value>) -> String {
    escape_html(&js::to_string(value))
}

fn array(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn text(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or_default()
}

/// `uniqueTokens`: the words of each value, split on spaces and commas, once each.
fn unique_tokens<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        for token in value.split([' ', ',']).filter(|token| !token.is_empty()) {
            if !out.iter().any(|seen| seen == token) {
                out.push(token.to_owned());
            }
        }
    }
    out
}

/// `dictionaryMarker`: a dictionary title as a marker suffix.
pub(crate) fn dictionary_marker(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let c = if c == '_' || js::is_space(c) { '-' } else { c };
        if (c == '-' || js::is_letter_or_number(c)) && !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    out.trim_matches('-').to_lowercase()
}

fn normalised_dictionary_marker(name: Option<&Value>) -> String {
    match name {
        Some(Value::String(name)) => dictionary_marker(&icu_normalizer::ComposingNormalizerBorrowed::new_nfkc().normalize(name)),
        _ => String::new(),
    }
}

fn is_id(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn dictionary_id_marker(id: Option<&Value>) -> String {
    match id {
        Some(Value::String(id)) if is_id(id) => format!("id--{id}"),
        _ => String::new(),
    }
}

fn alias(request: &Value, dictionary: &str) -> String {
    match request["dictionaryAliases"].get(dictionary) {
        Some(alias) => js::to_string(Some(alias)),
        None => dictionary.to_owned(),
    }
}

// --- Furigana ---

/// `furiganaHtml`.
fn furigana_html(segments: &[Piece], plain: bool) -> String {
    segments
        .iter()
        .enumerate()
        .map(|(index, (text, reading))| {
            let (text, reading) = (escape_html(text), escape_html(reading));
            if reading.is_empty() {
                text
            } else if plain {
                format!("{}{text}[{reading}]", if index > 0 { " " } else { "" })
            } else {
                format!("<ruby>{text}<rt>{reading}</rt></ruby>")
            }
        })
        .collect()
}

#[expect(clippy::cast_precision_loss, reason = "UTF-16 offsets of a sentence")]
fn units(value: usize) -> f64 {
    value as f64
}

/// `spanFurigana`: each `hd_segment` span read as its first headword.
fn span_furigana(text: &str, spans: &[Value]) -> Vec<Piece> {
    let mut segments = Vec::new();
    let mut position = 0.0;
    for span in spans {
        let start = js::to_number(span.get("start"));
        let end = start + js::to_number(span.get("length"));
        if start > position {
            segments.push((js::slice(text, position, Some(start)), String::new()));
        }
        let candidate = span["candidates"].get(0).unwrap_or(&Value::Null);
        segments.extend(matched_furigana(candidate, &js::slice(text, start, Some(end))));
        position = end;
    }
    if position < units(js::length(text)) {
        segments.push((js::slice(text, position, None), String::new()));
    }
    segments
}

// --- Frequencies ---

/// `frequencyNumber`: the leading digits of the display value, else the value.
fn frequency_number(frequency: &Value) -> Value {
    let digits: String = text(frequency.get("displayValue")).chars().take_while(char::is_ascii_digit).collect();
    let display: f64 = digits.parse().unwrap_or(0.0);
    if display > 0.0 { js::number(display) } else { frequency.get("value").cloned().unwrap_or(Value::Null) }
}

/// `frequencyModeMatches`.
fn mode_matches(mode: Option<&Value>, requested: &str) -> bool {
    let mode = text(mode);
    !matches!(mode, "rank-based" | "occurrence-based") || mode == requested
}

/// `frequencyMean`: each dictionary's first value, floored; `None` where there is none.
#[expect(clippy::cast_precision_loss, reason = "a count of dictionaries")]
fn frequency_mean(groups: &[&Value], harmonic: bool) -> Option<f64> {
    let mut numbers = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for group in groups {
        if !seen.insert(js::to_string(group.get("dictionary"))) {
            continue;
        }
        if let Some(first) = array(group.get("frequencies")).first()
            && let Some(value) = frequency_number(first).as_f64()
            && value.is_finite()
            && value > 0.0
        {
            numbers.push(value);
        }
    }
    if numbers.is_empty() {
        return None;
    }
    let total: f64 = numbers.iter().map(|value| if harmonic { 1.0 / value } else { *value }).sum();
    let count = numbers.len() as f64;
    Some(if harmonic { count / total } else { total / count }.floor())
}

fn frequency_aggregate(term: &Value, mode: &str, harmonic: bool) -> String {
    let groups: Vec<&Value> =
        array(term.get("frequencies")).iter().filter(|group| mode_matches(group.get("frequencyMode"), mode)).collect();
    match frequency_mean(&groups, harmonic) {
        Some(mean) => js::number_to_string(mean),
        None if mode == "rank-based" => "9999999".to_owned(),
        None => "0".to_owned(),
    }
}

/// `frequencyHtml`: under the right-to-left rules of docs/languages.md 12 the
/// list aligns to its start and a name with a right-to-left character sits in
/// its own element.
fn frequency_html(request: &Value, groups: &[&Value], rtl_rules: bool) -> String {
    let items: Vec<String> = groups
        .iter()
        .flat_map(|group| {
            let name = alias(request, &js::to_string(group.get("dictionary")));
            let dictionary = if rtl_rules && has_rtl_text(&name) {
                format!("<span dir=\"auto\">{}</span>", escape_html(&name))
            } else {
                escape_html(&name)
            };
            array(group.get("frequencies")).iter().map(move |value| {
                let shown = value.get("displayValue").filter(|shown| !shown.is_null()).or_else(|| value.get("value"));
                format!("<li>{dictionary}: {}</li>", escape(shown))
            })
        })
        .collect();
    let align = if rtl_rules { "start" } else { "left" };
    if items.is_empty() { String::new() } else { format!("<ul style=\"text-align: {align};\">{}</ul>", items.concat()) }
}

fn single_frequency(request: &Value, dictionary: &str, numeric: bool, rtl_rules: bool) -> String {
    let groups: Vec<&Value> = array(request["term"].get("frequencies"))
        .iter()
        .filter(|group| text(group.get("dictionary")) == dictionary && !array(group.get("frequencies")).is_empty())
        .collect();
    if numeric {
        let value = groups.first().map_or_else(|| js::number(0.0), |group| frequency_number(&group["frequencies"][0]));
        return if js::to_number(Some(&value)) > 0.0 { js::to_string(Some(&value)) } else { String::new() };
    }
    frequency_html(request, &groups, rtl_rules)
}

// --- Pitch accent ---

const PITCH_SMALL_KANA: &str = "ゃゅょぁぃぅぇぉゎャュョァィゥェォヮ";

fn is_mark(c: char) -> bool {
    use icu_properties::CodePointMapData;
    use icu_properties::props::{GeneralCategory, GeneralCategoryGroup};
    GeneralCategoryGroup::Mark.contains(CodePointMapData::<GeneralCategory>::new().get(c))
}

/// `splitPitchAccentMorae`.
fn split_morae(reading: &str) -> Vec<String> {
    let mut morae: Vec<String> = Vec::new();
    for c in icu_normalizer::ComposingNormalizerBorrowed::new_nfc().normalize(reading).chars() {
        match morae.last_mut() {
            Some(last) if PITCH_SMALL_KANA.contains(c) || is_mark(c) => last.push(c),
            _ => morae.push(c.to_string()),
        }
    }
    morae
}

/// `pitchAccentPositions`: a pattern of levels, or the downstep position.
#[derive(Clone, Copy)]
enum Positions<'a> {
    Pattern(&'a str),
    Position(Option<&'a Value>),
}

impl<'a> Positions<'a> {
    fn of(pitch: &'a Value) -> Self {
        match pitch.get("pattern") {
            Some(Value::String(pattern)) if !pattern.is_empty() => Self::Pattern(pattern),
            _ => Self::Position(pitch.get("position")),
        }
    }

    /// `pattern[index]`, a UTF-16 unit.
    fn level(pattern: &str, index: usize) -> Option<u16> {
        pattern.encode_utf16().nth(index)
    }

    /// `isMoraPitchHigh`.
    #[expect(clippy::cast_precision_loss, reason = "a mora index")]
    fn high(self, index: usize) -> bool {
        match self {
            Self::Pattern(pattern) => Self::level(pattern, index) == Some(u16::from(b'H')),
            Self::Position(position) => {
                let number = position.and_then(Value::as_f64);
                match number {
                    Some(0.0) => index > 0,
                    Some(1.0) => index < 1,
                    _ => index > 0 && (index as f64) < js::to_number(position),
                }
            }
        }
    }

    /// `isPitchForMorae`.
    #[expect(clippy::cast_precision_loss, reason = "a mora count")]
    fn fits(self, count: usize) -> bool {
        match self {
            Self::Pattern(pattern) => {
                pattern.bytes().all(|byte| byte == b'H' || byte == b'L') && (count..=count + 1).contains(&pattern.len())
            }
            Self::Position(position) => {
                position.and_then(Value::as_f64).is_some_and(|value| value.fract() == 0.0 && value >= 0.0 && value <= count as f64)
            }
        }
    }

    /// `pitchAccentDownstep`.
    fn downstep(self) -> String {
        match self {
            Self::Pattern(pattern) => {
                let bytes: Vec<u16> = pattern.encode_utf16().collect();
                let (high, low) = (u16::from(b'H'), u16::from(b'L'));
                let mut downsteps: Vec<String> = (1..bytes.len())
                    .filter(|index| bytes[index - 1] == high && bytes[*index] == low)
                    .map(|index| index.to_string())
                    .collect();
                if downsteps.is_empty() {
                    downsteps.push(if bytes.first() == Some(&low) { "0" } else { "-1" }.to_owned());
                }
                downsteps.join(",")
            }
            Self::Position(position) => js::to_string(position),
        }
    }
}

/// `buildPitchAccentMorae`: each mora and whether it is high.
fn build_morae(reading: &str, positions: Positions<'_>) -> Option<Vec<(String, bool)>> {
    let morae = split_morae(reading);
    if morae.is_empty() || !positions.fits(morae.len()) {
        return None;
    }
    Some(morae.into_iter().enumerate().map(|(index, mora)| (mora, positions.high(index))).collect())
}

fn is_han(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{f900}'..='\u{faff}' | '\u{20000}'..='\u{2fa1f}')
}

fn ends_as(rule: &str, expression: &str) -> bool {
    let last = expression.chars().last();
    match rule {
        "v1" | "v5" | "vk" => last.is_some_and(|c| "うくぐすずつづぬふぶむゆるウクグスズツヅヌフブムユル".contains(c)),
        "vs" | "vz" => {
            let mut chars = expression.chars().rev();
            let (last, before) = (chars.next(), chars.next());
            ["する", "為る", "ずる"].iter().any(|ending| expression.ends_with(ending))
                || (last.is_some_and(|c| c == 'す' || c == 'ず') && before.is_some_and(is_han))
        }
        "adj-i" => last.is_some_and(|c| c == 'い' || c == 'イ'),
        _ => false,
    }
}

/// `pitchAccentCategory`.
#[expect(clippy::float_cmp, reason = "JavaScript's strict equality of whole numbers")]
fn pitch_category(reading: &str, pitch: &Value, classes: &[String], expression: &str) -> Option<&'static str> {
    let positions = Positions::of(pitch);
    let downstep = positions.downstep();
    let position = js::string_to_number(downstep.split(',').next().unwrap_or_default());
    if position == 0.0 {
        return Some("heiban");
    }
    if position.is_nan() || position < 0.0 {
        return None;
    }
    if let Positions::Pattern(pattern) = positions {
        let units: Vec<u16> = pattern.encode_utf16().collect();
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a positive mora index")]
        let from = (position as usize).min(units.len());
        if units[from..].contains(&u16::from(b'H')) {
            return None;
        }
    }
    let has = |rule: &str| classes.iter().any(|class| class == rule);
    if !(has("vs") && has("n")) && ["v1", "v5", "vk", "vs", "vz", "adj-i"].iter().any(|rule| has(rule) && ends_as(rule, expression)) {
        return Some("kifuku");
    }
    if position == 1.0 {
        return Some("atamadaka");
    }
    #[expect(clippy::cast_precision_loss, reason = "a mora count")]
    let count = split_morae(reading).len() as f64;
    Some(if position >= count { "odaka" } else { "nakadaka" })
}

fn reading_of(term: &Value) -> &str {
    let reading = text(term.get("reading"));
    if reading.is_empty() { text(term.get("expression")) } else { reading }
}

fn pitch_html(term: &Value) -> String {
    array(term.get("pitches"))
        .iter()
        .filter_map(|group| {
            let mut descriptions: Vec<String> = array(group.get("pitches"))
                .iter()
                .map(|pitch| {
                    let mut details = Vec::new();
                    for (key, label) in [("nasal", "nasal"), ("devoice", "devoice")] {
                        let list = array(pitch.get(key));
                        if !list.is_empty() {
                            details.push(format!(
                                "{label} {}",
                                list.iter().map(|item| js::to_string(Some(item))).collect::<Vec<_>>().join(",")
                            ));
                        }
                    }
                    let base = if js::truthy(pitch.get("pattern")) {
                        js::to_string(pitch.get("pattern"))
                    } else {
                        format!("position {}", js::to_string(pitch.get("position")))
                    };
                    if details.is_empty() { base } else { format!("{base} ({})", details.join("; ")) }
                })
                .collect();
            descriptions.extend(array(group.get("transcriptions")).iter().map(|item| js::to_string(Some(item))));
            (!descriptions.is_empty()).then(|| {
                let list: Vec<String> = descriptions.iter().map(|description| escape_html(description)).collect();
                format!("<b>{}</b>: {}", escape(group.get("dictionary")), list.join(", "))
            })
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("<br>")
}

fn pitch_categories(term: &Value) -> String {
    let reading = reading_of(term);
    let classes = unique_tokens([text(term.get("rules"))]);
    let expression = text(term.get("expression"));
    let mut out: Vec<&str> = Vec::new();
    for group in array(term.get("pitches")) {
        for pitch in array(group.get("pitches")) {
            if let Some(category) = pitch_category(reading, pitch, &classes, expression)
                && !out.contains(&category)
            {
                out.push(category);
            }
        }
    }
    out.join(",")
}

/// V8's `Math.hypot`: the scaled, compensated sum, so lengths match the extension's.
fn hypot(dx: f64, dy: f64) -> f64 {
    let max = dx.abs().max(dy.abs());
    if max == 0.0 {
        return 0.0;
    }
    let (mut sum, mut compensation) = (0.0, 0.0);
    for value in [dx, dy] {
        let scaled = value.abs() / max;
        let summand = scaled * scaled - compensation;
        let preliminary = sum + summand;
        compensation = (preliminary - sum) - summand;
        sum = preliminary;
    }
    sum.sqrt() * max
}

fn n(value: f64) -> String {
    js::number_to_string(value)
}

fn graph_line(from: (f64, f64), to: (f64, f64), radius: f64) -> String {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let scale = radius / hypot(dx, dy);
    format!("M{} {}L{} {}", n(from.0 + dx * scale), n(from.1 + dy * scale), n(to.0 - dx * scale), n(to.1 - dy * scale))
}

/// `pitchGraph`.
#[expect(clippy::cast_precision_loss, reason = "mora indexes")]
fn pitch_graph(reading: &str, pitch: &Value, kana: bool) -> String {
    let positions = Positions::of(pitch);
    let Some(morae) = build_morae(reading, positions) else { return String::new() };
    let mut levels: Vec<bool> = morae.iter().map(|(_, high)| *high).collect();
    levels.push(match positions {
        Positions::Pattern(pattern) if kana => {
            let units: Vec<u16> = pattern.encode_utf16().collect();
            units.get(morae.len()).or(units.last()) == Some(&u16::from(b'H'))
        }
        _ => positions.high(morae.len()),
    });
    let (step, height, radius) = if kana { (35.0, 80.0, 5.0) } else { (50.0, 100.0, 15.0) };
    let (high_y, low_y) = if kana { (10.0, 35.0) } else { (25.0, 75.0) };
    let points: Vec<(f64, f64)> =
        levels.iter().enumerate().map(|(index, high)| (step * (index as f64 + 0.5), if *high { high_y } else { low_y })).collect();
    let width = step * points.len() as f64;
    let shown = if js::truthy(pitch.get("pattern")) { js::to_string(pitch.get("pattern")) } else { js::to_string(pitch.get("position")) };
    let label = escape_html(&format!("{reading}: pitch accent {shown}"));
    let mut svg = vec![format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" class=\"pronunciation-graph\" viewBox=\"0 0 {} {}\" role=\"img\" aria-label=\"{label}\" \
         focusable=\"false\" style=\"display: inline-block; vertical-align: middle; width: {}em; height: {}em; max-width: 100%;\"><title>{label}</title>",
        n(width),
        n(height),
        n(width / height * if kana { 3.0 } else { 2.0 }),
        if kana { 3 } else { 2 },
    )];
    for index in 0..points.len() - 1 {
        let tail = index == morae.len() - 1;
        svg.push(format!(
            "<path d=\"{}\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"{}\"{}/>",
            graph_line(points[index], points[index + 1], radius),
            if kana { "1.5" } else { "5" },
            if tail && !kana { " stroke-dasharray=\"5 5\"" } else { "" }
        ));
    }
    for (index, (x, y)) in points[..points.len() - 1].iter().enumerate() {
        let downstep = !kana && levels[index] && !levels[index + 1];
        svg.push(format!(
            "<circle class=\"pronunciation-graph-dot\" cx=\"{}\" cy=\"{}\" r=\"{}\" fill=\"{}\"{}/>",
            n(*x),
            n(*y),
            n(radius),
            if downstep { "none" } else { "currentColor" },
            if downstep { " stroke=\"currentColor\" stroke-width=\"5\"" } else { "" }
        ));
        if downstep {
            svg.push(format!("<circle cx=\"{}\" cy=\"{}\" r=\"5\" fill=\"currentColor\"/>", n(*x), n(*y)));
        }
    }
    let (tail_x, tail_y) = points[points.len() - 1];
    let tail = format!(
        "class=\"pronunciation-graph-tail\" data-pitch=\"{}\" fill=\"none\" stroke=\"currentColor\"",
        if levels[levels.len() - 1] { "high" } else { "low" }
    );
    if kana {
        svg.push(format!("<circle {tail} cx=\"{}\" cy=\"{}\" r=\"4\" stroke-width=\"2\"/>", n(tail_x), n(tail_y)));
        for (index, (mora, _)) in morae.iter().enumerate() {
            svg.push(format!(
                "<text x=\"{}\" y=\"70\" text-anchor=\"middle\"{} style=\"font: 20px sans-serif; fill: currentColor;\">{}</text>",
                n(points[index].0),
                if mora.chars().count() > 1 { " textLength=\"30\" lengthAdjust=\"spacingAndGlyphs\"" } else { "" },
                escape_html(mora)
            ));
        }
    } else {
        svg.push(format!("<path {tail} d=\"M{} {}l15 -26h-30Z\" stroke-width=\"5\"/>", n(tail_x), n(tail_y + 13.0)));
    }
    svg.push("</svg>".to_owned());
    svg.concat()
}

/// `ankiPitchGraphs`.
fn pitch_graphs(term: &Value, kana: bool) -> String {
    let reading = reading_of(term);
    array(term.get("pitches"))
        .iter()
        .map(|group| {
            array(group.get("pitches"))
                .iter()
                .map(|pitch| pitch_graph(reading, pitch, kana))
                .filter(|graph| !graph.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("<br>")
}

// --- Glossary markers named by dictionary ---

struct Descriptor {
    dictionary: String,
    legacy: String,
    alias: String,
    id: String,
}

/// `dynamicGlossaries`: what each `{single-glossary-…}` marker asks for.
fn dynamic_glossaries(request: &Value, requested: &[String]) -> HashMap<String, Wanted> {
    let mut exact: HashMap<String, String> = HashMap::new();
    let mut variants: HashMap<String, Vec<(String, Wanted)>> = HashMap::new();
    let mut bases: HashSet<String> = HashSet::new();
    for name in requested {
        let key = name[GLOSSARY_MARKER_PREFIX.len()..].to_owned();
        exact.insert(key.clone(), name.clone());
        bases.insert(key.clone());
        for (ending, options) in &GLOSSARY_VARIANTS {
            if let Some(base) = key.strip_suffix(ending) {
                bases.insert(base.to_owned());
                variants.entry(base.to_owned()).or_default().push((name.clone(), options.clone()));
            }
        }
    }
    let mut dictionaries: Vec<String> = Vec::new();
    for glossary in array(request["term"].get("glossaries")) {
        let dictionary = js::to_string(glossary.get("dictionary"));
        if !dictionaries.contains(&dictionary) {
            dictionaries.push(dictionary);
        }
    }
    let aliases = request["dictionaryAliases"].as_object();
    let wants_id = bases.iter().any(|key| key.strip_prefix("id--").is_some_and(is_id));
    let descriptors: Vec<Descriptor> = dictionaries
        .into_iter()
        .map(|dictionary| Descriptor {
            legacy: dictionary_marker(&dictionary),
            alias: aliases
                .and_then(|aliases| aliases.get(&dictionary))
                .map_or_else(String::new, |alias| normalised_dictionary_marker(Some(alias))),
            id: if wants_id { dictionary_id_marker(request["dictionaryIds"].get(&dictionary)) } else { String::new() },
            dictionary,
        })
        .collect();
    let mut owners: HashMap<String, Option<String>> = HashMap::new();
    let mut claim = |key: &str, dictionary: &str| {
        if key.is_empty() || !bases.contains(key) {
            return;
        }
        match owners.get(key) {
            None => {
                owners.insert(key.to_owned(), Some(dictionary.to_owned()));
            }
            Some(owner) if owner.as_deref() != Some(dictionary) => {
                owners.insert(key.to_owned(), None);
            }
            Some(_) => {}
        }
    };
    for descriptor in &descriptors {
        claim(&descriptor.legacy, &descriptor.dictionary);
        if descriptor.alias != descriptor.legacy {
            claim(&descriptor.alias, &descriptor.dictionary);
        }
    }
    let identity = |descriptor: &Descriptor, phase: usize| -> String {
        match phase {
            0 => descriptor.legacy.clone(),
            1 if owners.get(&descriptor.alias).is_some_and(|owner| owner.as_deref() == Some(descriptor.dictionary.as_str())) => {
                descriptor.alias.clone()
            }
            2 => descriptor.id.clone(),
            _ => String::new(),
        }
    };
    let mut resolved: HashMap<String, Wanted> = HashMap::new();
    for phase in 0..3 {
        for descriptor in &descriptors {
            let key = identity(descriptor, phase);
            if let Some(name) = exact.get(&key).filter(|_| !key.is_empty())
                && !resolved.contains_key(name)
            {
                resolved.insert(name.clone(), Wanted { dictionary: Some(descriptor.dictionary.clone()), ..Wanted::default() });
            }
        }
        for descriptor in &descriptors {
            let key = identity(descriptor, phase);
            if key.is_empty() {
                continue;
            }
            for (name, options) in variants.get(&key).into_iter().flatten() {
                if !resolved.contains_key(name) {
                    resolved.insert(name.clone(), Wanted { dictionary: Some(descriptor.dictionary.clone()), ..options.clone() });
                }
            }
        }
    }
    resolved
}

/// `dynamicFrequencies`.
fn dynamic_frequencies(request: &Value) -> HashMap<String, (String, bool)> {
    let mut variants = HashMap::new();
    for dictionary in array(request.get("frequencyDictionaries")) {
        let dictionary = js::to_string(Some(dictionary));
        let key = dictionary_marker(&dictionary);
        if key.is_empty() {
            continue;
        }
        variants.insert(format!("single-frequency-number-{key}"), (dictionary.clone(), true));
        variants.insert(format!("single-frequency-{key}"), (dictionary, false));
    }
    variants
}

/// Renders one definition marker.
pub(crate) type Definition<'a> = dyn FnMut(&Wanted) -> Result<String> + 'a;

struct Builder<'a, 'b> {
    request: &'a Value,
    term: &'a Value,
    definition: &'a mut Definition<'b>,
    audio: &'a str,
    glossary_markers: Vec<String>,
    glossaries: Option<HashMap<String, Wanted>>,
    frequencies: Option<HashMap<String, (String, bool)>>,
    values: HashMap<String, String>,
    /// The language of the profile the note is mined in; without one every
    /// value is today's.
    language: Option<&'a NoteLanguage>,
}

impl Builder<'_, '_> {
    fn kanji(&self) -> Vec<&Value> {
        array(self.term.get("glossaries"))
            .iter()
            .filter_map(|glossary| glossary.get("kanji").filter(|kanji| js::truthy(Some(kanji))))
            .collect()
    }

    /// `kanjiTokens`.
    fn kanji_tokens(value: Option<&Value>) -> Vec<String> {
        match value {
            Some(Value::Array(items)) => items.iter().map(|item| js::to_string(Some(item))).collect(),
            _ => tag_list(value),
        }
    }

    fn kanji_readings(&self, key: &str) -> String {
        let mut out: Vec<String> = Vec::new();
        for entry in self.kanji() {
            for token in Self::kanji_tokens(entry.get(key)) {
                if !out.contains(&token) {
                    out.push(token);
                }
            }
        }
        out.iter().map(|token| escape_html(token)).collect::<Vec<_>>().join(", ")
    }

    /// The sentence before, at and after the match, escaped.
    fn parts(&self) -> [String; 3] {
        let sentence = js::to_string(self.request.get("sentence"));
        let offset = js::to_number(self.request.get("matchOffset"));
        let end = offset + units(js::length(text(self.request.get("matched"))));
        [js::slice(&sentence, 0.0, Some(offset)), js::slice(&sentence, offset, Some(end)), js::slice(&sentence, end, None)]
            .map(|part| escape_html(&part))
    }

    fn sentence(&self) -> String {
        let [prefix, body, suffix] = self.parts();
        format!("{prefix}<b>{body}</b>{suffix}")
    }

    /// A note mined in a right-to-left profile follows the right-to-left rules.
    fn rtl_rules(&self) -> bool {
        self.language.is_some_and(|language| language.direction == "rtl")
    }

    /// The sentence keeps its own direction in any card; the cloze parts stay
    /// bare, because isolating each one would break the sentence's order.
    fn isolated_sentence(&self) -> String {
        if self.rtl_rules() { format!("<span dir=\"auto\">{}</span>", self.sentence()) } else { self.sentence() }
    }

    /// Without ruby the furigana markers give the plain headword.
    fn ruby(&self) -> bool {
        self.language.is_none_or(|language| language.ruby)
    }

    /// `partOfSpeechLabels`: a language's tags with its pack's labels; the
    /// Japanese pack carries the table above.
    fn part_of_speech(&self, tag: &str) -> String {
        let label = match self.language.map(|language| (language.part_of_speech_labels(), language.is_japanese())) {
            None | Some((None, true)) => PARTS_OF_SPEECH.iter().find(|(key, _)| *key == tag).map(|(_, label)| (*label).to_owned()),
            Some((Some(labels), _)) => labels.get(tag).map(|label| js::to_string(Some(label))),
            Some((None, false)) => None,
        };
        escape_html(label.as_deref().unwrap_or(tag))
    }

    fn sentence_furigana(&self, plain: bool) -> String {
        let Some(segments) = self.request.get("sentenceSegments").filter(|segments| js::truthy(Some(segments))) else {
            return self.sentence();
        };
        let sentence = js::to_string(self.request.get("sentence"));
        let offset = js::to_number(self.request.get("matchOffset"));
        let end = offset + units(js::length(text(self.request.get("matched"))));
        let prefix = furigana_html(&span_furigana(&js::slice(&sentence, 0.0, Some(offset)), array(segments.get(0))), plain);
        let body = furigana_html(&matched_furigana(self.term, &js::slice(&sentence, offset, Some(end))), plain);
        let suffix = furigana_html(&span_furigana(&js::slice(&sentence, end, None), array(segments.get(1))), plain);
        format!("{prefix}<b>{body}</b>{suffix}")
    }

    fn first_dictionary(&self) -> String {
        array(self.term.get("glossaries"))
            .first()
            .map(|glossary| glossary.get("dictionary"))
            .map_or_else(String::new, |dictionary| if js::truthy(dictionary) { js::to_string(dictionary) } else { String::new() })
    }

    fn page_url(&self) -> String {
        match self.request.get("pageUrl") {
            Some(Value::String(url)) => escape_html(url),
            _ => String::new(),
        }
    }

    fn unavailable(&self, kind: &str) -> bool {
        array(self.request.get("captureUnavailable")).iter().any(|item| item.as_str() == Some(kind))
    }

    fn capture(&self, key: &str) -> Option<String> {
        let filename = self.request.get(key).and_then(|capture| capture.get("filename"));
        js::truthy(filename).then(|| escape(filename))
    }

    fn screenshot(&self) -> String {
        match self.capture("screenshot") {
            Some(filename) if !self.unavailable("screenshot") => format!("<img src=\"{filename}\">"),
            _ => String::new(),
        }
    }

    fn definition(&mut self, wanted: &Wanted) -> Result<String> {
        (self.definition)(wanted)
    }

    /// A value of the marker table, or `None` for a name it does not have.
    #[expect(clippy::too_many_lines, reason = "the extension's marker table, one arm per marker")]
    fn table(&mut self, name: &str) -> Result<Option<String>> {
        let (request, term) = (self.request, self.term);
        let glossaries = array(term.get("glossaries"));
        let first = |wanted: Wanted| Wanted { first_only: true, ..wanted };
        Ok(Some(match name {
            "expression" => escape(term.get("expression")),
            "reading" => escape(term.get("reading")),
            "character" => {
                if self.kanji().is_empty() {
                    String::new()
                } else {
                    escape(term.get("expression"))
                }
            }
            "onyomi" => self.kanji_readings("onyomi"),
            "kunyomi" => self.kanji_readings("kunyomi"),
            "stroke-count" => {
                let value = self
                    .kanji()
                    .into_iter()
                    .flat_map(|entry| array(entry.get("stats")))
                    .find(|stat| stat.get("name").and_then(Value::as_str) == Some("strokes"))
                    .and_then(|stat| stat.get("value"))
                    .filter(|value| !value.is_null());
                value.map_or_else(String::new, |value| escape(Some(value)))
            }
            // Without ruby the furigana markers give the plain headword.
            "furigana" | "furigana-plain" if !self.ruby() => escape(term.get("expression")),
            "furigana" => furigana_html(&term_furigana(term), false),
            "furigana-plain" => furigana_html(&term_furigana(term), true),
            "dictionary" => escape_html(&self.first_dictionary()),
            "dictionary-alias" => escape_html(&alias(request, &self.first_dictionary())),
            "glossary" => self.definition(&Wanted::default())?,
            "glossary-brief" => self.definition(&Wanted { brief: true, ..Wanted::default() })?,
            "glossary-no-dictionary" => self.definition(&Wanted { no_dictionary: true, ..Wanted::default() })?,
            "glossary-plain" => self.definition(&Wanted { plain: true, ..Wanted::default() })?,
            "glossary-plain-no-dictionary" => self.definition(&Wanted { plain: true, no_dictionary: true, ..Wanted::default() })?,
            "glossary-first" => self.definition(&first(Wanted::default()))?,
            "glossary-first-brief" => self.definition(&first(Wanted { brief: true, ..Wanted::default() }))?,
            "glossary-first-no-dictionary" => self.definition(&first(Wanted { no_dictionary: true, ..Wanted::default() }))?,
            "conjugation" => {
                let steps: Vec<String> = array(request.get("trace")).iter().map(|step| escape(step.get("name"))).collect();
                let joined = steps.join(" « ");
                if joined.is_empty() { escape(term.get("rules")) } else { joined }
            }
            "part-of-speech" => {
                let tags = unique_tokens(
                    std::iter::once(text(term.get("rules"))).chain(glossaries.iter().map(|glossary| text(glossary.get("termTags")))),
                );
                let joined = tags.iter().map(|tag| self.part_of_speech(tag)).collect::<Vec<_>>().join(", ");
                if joined.is_empty() { "Unknown".to_owned() } else { joined }
            }
            "tags" => {
                let mut sources: Vec<String> = Vec::new();
                for glossary in glossaries {
                    sources.extend(definition_tags(glossary));
                    sources.push(text(glossary.get("termTags")).to_owned());
                    if let Some(kanji) = glossary.get("kanji").filter(|kanji| js::truthy(Some(kanji))) {
                        sources.extend(Self::kanji_tokens(kanji.get("tags")));
                    }
                }
                unique_tokens(sources.iter().map(String::as_str))
                    .iter()
                    .map(|tag| {
                        let tag = escape_html(tag);
                        format!("<span class=\"tag\" data-details=\"{tag}\">{tag}</span>")
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            }
            "phonetic-transcriptions" => {
                let items: Vec<String> = array(term.get("pitches"))
                    .iter()
                    .flat_map(|group| array(group.get("transcriptions")))
                    .filter(|value| js::truthy(Some(value)))
                    .map(|value| {
                        format!(
                            "<li class=\"pronunciation\" data-pronunciation-type=\"phonetic-transcription\">{}</li>",
                            escape(Some(value))
                        )
                    })
                    .collect();
                if items.is_empty() { String::new() } else { format!("<ul>{}</ul>", items.concat()) }
            }
            "popup-selection-text" => escape(request.get("popupSelectionText")).replace('\n', "<br>\n"),
            "search-query" => escape(request.get("searchQuery")),
            "document-title" => escape(request.get("documentTitle")),
            // A video's title and channel or episode, else the page title.
            "miscinfo" => escape(request.get("miscInfo").filter(|info| js::truthy(Some(info))).or_else(|| request.get("documentTitle"))),
            "url" => {
                let address = self.page_url();
                if address.is_empty() { address } else { format!("<a href=\"{address}\">{address}</a>") }
            }
            "url-plain" => self.page_url(),
            "sentence" => self.isolated_sentence(),
            "sentence-furigana" => self.sentence_furigana(false),
            "sentence-furigana-plain" => self.sentence_furigana(true),
            "cloze-prefix" => self.parts()[0].clone(),
            "cloze-body" => self.parts()[1].clone(),
            "cloze-suffix" => self.parts()[2].clone(),
            "frequencies" => frequency_html(request, &array(term.get("frequencies")).iter().collect::<Vec<_>>(), self.rtl_rules()),
            "frequency-harmonic-rank" => frequency_aggregate(term, "rank-based", true),
            "frequency-harmonic-occurrence" => frequency_aggregate(term, "occurrence-based", true),
            "frequency-average-rank" => frequency_aggregate(term, "rank-based", false),
            "frequency-average-occurrence" => frequency_aggregate(term, "occurrence-based", false),
            "pitch" => pitch_html(term),
            "pitch-position" => {
                let mut out: Vec<String> = Vec::new();
                for group in array(term.get("pitches")) {
                    for pitch in array(group.get("pitches")) {
                        let downstep = Positions::of(pitch).downstep();
                        if !out.contains(&downstep) {
                            out.push(downstep);
                        }
                    }
                }
                out.join(", ")
            }
            "pitch-accent-graphs" => pitch_graphs(term, false),
            "pitch-accent-graphs-jj" => pitch_graphs(term, true),
            "pitch-accent-categories" => pitch_categories(term),
            "audio" => self.audio.to_owned(),
            "screenshot" => self.screenshot(),
            "sentence-audio" => match self.capture("sentenceAudio") {
                Some(filename) if !self.unavailable("sentence-audio") => format!("[sound:{filename}]"),
                _ => String::new(),
            },
            "gif" => match self.capture("gif") {
                None => self.screenshot(),
                Some(_) if self.unavailable("gif") => String::new(),
                Some(filename) => format!("<img src=\"{filename}\">"),
            },
            _ => return Ok(None),
        }))
    }

    /// `valueFor`.
    fn value_for(&mut self, name: &str) -> Result<String> {
        if let Some((_, target)) = VALUE_ALIASES.iter().find(|(key, _)| *key == name) {
            return self.value_for(target);
        }
        if let Some(value) = self.values.get(name) {
            return Ok(value.clone());
        }
        let value = match self.table(name)? {
            Some(value) => value,
            None if name.starts_with(GLOSSARY_MARKER_PREFIX) => {
                let glossaries = self.glossaries.get_or_insert_with(|| dynamic_glossaries(self.request, &self.glossary_markers));
                match glossaries.get(name).cloned() {
                    Some(wanted) => self.definition(&wanted)?,
                    None => String::new(),
                }
            }
            None if name.starts_with("single-frequency-") => {
                let frequencies = self.frequencies.get_or_insert_with(|| dynamic_frequencies(self.request));
                match frequencies.get(name).cloned() {
                    Some((dictionary, numeric)) => single_frequency(self.request, &dictionary, numeric, self.rtl_rules()),
                    None => String::new(),
                }
            }
            None => String::new(),
        };
        self.values.insert(name.to_owned(), value.clone());
        Ok(value)
    }
}

/// `buildAnkiFields(request, templates, {definition, audio, language})`: each
/// field's rendered Template, in the note type's order.
pub(crate) fn build_fields(
    request: &Value,
    templates: &[(String, Map<String, Value>)],
    definition: &mut Definition<'_>,
    audio: &str,
    language: Option<&NoteLanguage>,
) -> Result<Fields> {
    let planned: Vec<(&str, &str, Vec<String>)> = templates
        .iter()
        .map(|(field, template)| {
            let value = text(template.get("value"));
            let mut names = marker_names(value);
            let mut seen = HashSet::new();
            names.retain(|name| seen.insert(name.clone()));
            (field.as_str(), value, names)
        })
        .collect();
    let mut glossary_markers: Vec<String> = Vec::new();
    for name in planned.iter().flat_map(|(_, _, names)| names) {
        if name.starts_with(GLOSSARY_MARKER_PREFIX) && !glossary_markers.contains(name) {
            glossary_markers.push(name.clone());
        }
    }
    let mut builder = Builder {
        request,
        term: &request["term"],
        definition,
        audio,
        glossary_markers,
        glossaries: None,
        frequencies: None,
        values: HashMap::new(),
        language,
    };
    let mut fields = Vec::with_capacity(planned.len());
    for (field, template, names) in planned {
        let mut markers = HashMap::new();
        for name in names {
            let value = builder.value_for(&name)?;
            markers.insert(name, value);
        }
        fields.push((field.to_owned(), render(template, &markers).map_err(Failure::new)?));
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn fields(request: &Value, templates: &[(&str, &str)]) -> Vec<(String, String)> {
        let templates: Vec<(String, Map<String, Value>)> = templates
            .iter()
            .map(|(field, value)| ((*field).to_owned(), json!({ "value": value }).as_object().cloned().unwrap_or_default()))
            .collect();
        let mut definition = |wanted: &Wanted| Ok(format!("[{wanted:?}]"));
        build_fields(request, &templates, &mut definition, "", None).unwrap()
    }

    #[test]
    fn builds_the_marker_table() {
        let request = json!({
            "sentence": "昨日<食べた>", "matchOffset": 3, "matched": "食べた", "trace": [{ "name": "past" }], "pageUrl": "https://a.b/?x&y",
            "dictionaryAliases": { "Jitendex.org [2024]": "JMdict" }, "frequencyDictionaries": ["JPDB v2"],
            "term": {
                "expression": "食べる", "reading": "たべる", "rules": "v1",
                "glossaries": [{ "dictionary": "Jitendex.org [2024]", "termTags": "P ichi", "definitionTags": "v1 vt" }],
                "frequencies": [
                    { "dictionary": "JPDB v2", "frequencyMode": "rank-based", "frequencies": [{ "value": 1200, "displayValue": "1200㋕" }] },
                    { "dictionary": "Other", "frequencies": [{ "value": 300 }] },
                ],
                "pitches": [{ "dictionary": "NHK", "pitches": [{ "position": 2, "nasal": [], "devoice": [3] }], "transcriptions": [] }],
            },
        });
        let built = fields(
            &request,
            &[
                ("A", "{expression}[{reading}] {furigana-plain}"),
                ("B", "{sentence}|{cloze-body}|{conjugation}|{part-of-speech}"),
                ("C", "{frequency-harmonic-rank} {frequency-average-occurrence} {single-frequency-number-jpdb-v2}"),
                ("D", "{pitch} {pitch-position} {pitch-categories}"),
                ("E", "{single-glossary-jitendexorg-2024-brief}{single-glossary-jmdict}"),
                ("F", "{url}"),
                ("G", "{search-query}"),
            ],
        );
        let value = |field: &str| built.iter().find(|(name, _)| name == field).map(|(_, value)| value.clone()).unwrap();
        assert_eq!(value("A"), "食べる[たべる] 食[た]べる");
        assert_eq!(value("B"), "昨日&lt;<b>食べた</b>&gt;|食べた|past|Ichidan verb, P, ichi");
        assert_eq!(value("C"), "480 300 1200");
        assert_eq!(value("D"), "<b>NHK</b>: position 2 (devoice 3) 2 kifuku");
        assert!(value("E").contains("brief: true") && value("E").contains("Jitendex.org [2024]"));
        assert_eq!(value("F"), "<a href=\"https://a.b/?x&amp;y\">https://a.b/?x&amp;y</a>");
        assert_eq!(value("G"), "undefined");
    }

    #[test]
    fn categorises_and_draws_pitch() {
        let pitch = json!({ "position": 0 });
        assert_eq!(pitch_category("さくら", &json!({ "position": 0 }), &[], "さくら"), Some("heiban"));
        assert_eq!(pitch_category("さくら", &json!({ "position": 3 }), &[], "さくら"), Some("odaka"));
        assert_eq!(pitch_category("きょうと", &json!({ "position": 1 }), &[], "京都"), Some("atamadaka"));
        assert_eq!(pitch_category("に", &json!({ "pattern": "HLLLHLL" }), &[], "x"), None);
        assert_eq!(split_morae("きょうと"), ["きょ", "う", "と"]);
        assert_eq!(Positions::of(&json!({ "pattern": "LHHL" })).downstep(), "3");
        let graph = pitch_graph("あめ", &pitch, false);
        assert!(graph.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\" class=\"pronunciation-graph\" viewBox=\"0 0 150 100\""));
        assert!(graph.contains("width: 3em; height: 2em;"));
        assert!(graph.contains("<path d=\"M35.60660171779821 64.39339828220179L64.39339828220179 35.60660171779821\""));
        assert_eq!(dictionary_marker("Jitendex.org [2024]"), "jitendexorg-2024");
        assert_eq!(dictionary_marker("__A  b__"), "a-b");
    }
}
