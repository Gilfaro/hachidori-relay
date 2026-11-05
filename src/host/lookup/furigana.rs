// SPDX-License-Identifier: GPL-3.0-or-later
//! A headword's furigana from its kanji's readings: a port of the extension's
//! `HDGlossary.kanjiFurigana` (`extension/render/glossary.js`), which
//! `engine-service.js` adds to every lookup reply as `term.furigana`.
//!
//! The extension indexes strings in UTF-16 code units, so this does too: every
//! offset below counts code units, and text is held as `Vec<u16>`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

use serde::Deserialize;
use serde_json::{Value, json};

/// KANJIDIC's readings, as the extension vendors them (`vendor/kanjidic`).
const KANJIDIC: &str = include_str!("../../../data/kanjidic/kanji-readings.json");
/// Jitendex's kanwa readings and compounds (`vendor/kanwa`).
const KANWA: &str = include_str!("../../../data/kanwa/kanwa-readings.json");

type Text = Vec<u16>;

fn utf16(text: &str) -> Text {
    text.encode_utf16().collect()
}

const DAKUTEN_KANA: &str = "かきくけこさしすせそたちつてとはひふへほ";
const HANDAKUTEN_KANA: &str = "はひふへほ";
const U_ROW_KANA: &str = "うくぐすつぬぶむる";
const I_ROW_KANA: &str = "いきぎしちにびみり";
const SOKUON_KANA: &str = "つちくきり";

/// JavaScript's `text.startsWith(prefix, at)`.
fn starts_with_at(text: &[u16], prefix: &[u16], at: usize) -> bool {
    text[at.min(text.len())..].starts_with(prefix)
}

/// JavaScript's `text.substring(start, end)`: clamped, and swapped when reversed.
fn substring(text: &[u16], start: usize, end: usize) -> Text {
    let (start, end) = (start.min(text.len()), end.min(text.len()));
    let (start, end) = if start > end { (end, start) } else { (start, end) };
    text[start..end].to_vec()
}

/// JavaScript's `text.slice(start, end)` for non-negative bounds.
fn slice(text: &[u16], start: usize, end: usize) -> Text {
    let (start, end) = (start.min(text.len()), end.min(text.len()));
    if start >= end { Vec::new() } else { text[start..end].to_vec() }
}

fn concat(parts: &[&[u16]]) -> Text {
    parts.concat()
}

fn contains_unit(set: &str, unit: Option<u16>) -> bool {
    unit.is_some_and(|unit| set.encode_utf16().any(|candidate| candidate == unit))
}

/// The extension's `toHiragana`: katakana ァ–ヶ as hiragana.
fn to_hiragana(text: &[u16]) -> Text {
    text.iter().map(|&unit| if (0x30a1..=0x30f6).contains(&unit) { unit - 0x60 } else { unit }).collect()
}

/// Yomitan's isCodePointKana: the Hiragana and Katakana blocks.
fn is_kana(character: char) -> bool {
    ('\u{3040}'..='\u{30ff}').contains(&character)
}

/// An insertion-ordered set of forms, as a JavaScript `Set` iterates.
#[derive(Default)]
struct Forms(Vec<Text>);

impl Forms {
    fn add(&mut self, form: Text) {
        if !self.0.contains(&form) {
            self.0.push(form);
        }
    }
}

/// `kanjiReadingForms`: the ways a kanji can be read inside a word, from its
/// readings in KANJIDIC's notation (`す.く`).
fn kanji_reading_forms(readings: &str) -> Vec<Text> {
    let mut forms = Forms::default();
    for reading in readings.split(' ') {
        let mut parts = reading.split('.');
        let stem = utf16(parts.next().unwrap_or_default());
        let okurigana = utf16(parts.next().unwrap_or_default());
        forms.add(stem.clone());
        forms.add(concat(&[&stem, &okurigana]));
        for length in 1..okurigana.len() {
            forms.add(concat(&[&stem, &okurigana[..length]]));
        }
        let row = okurigana.last().and_then(|&last| U_ROW_KANA.encode_utf16().position(|unit| unit == last));
        if let Some(i_row) = row.and_then(|row| I_ROW_KANA.encode_utf16().nth(row)) {
            forms.add(concat(&[&stem, &okurigana[..okurigana.len() - 1], &[i_row]]));
        }
    }
    for form in forms.0.clone() {
        let first = form.first().copied();
        if contains_unit(DAKUTEN_KANA, first) {
            forms.add(concat(&[&[form[0] + 1], &form[1..]]));
        }
        if contains_unit(HANDAKUTEN_KANA, first) {
            forms.add(concat(&[&[form[0] + 2], &form[1..]]));
        }
        // ち and つ also voice as じ and ず.
        let zi_zu = match first {
            Some(0x3061) => Some(0x3058),
            Some(0x3064) => Some(0x305a),
            _ => None,
        };
        if let Some(voiced) = zi_zu {
            forms.add(concat(&[&[voiced], &form[1..]]));
        }
    }
    for form in forms.0.clone() {
        if contains_unit(SOKUON_KANA, form.last().copied()) {
            forms.add(concat(&[&form[..form.len() - 1], &utf16("っ")]));
        }
    }
    forms.0
}

/// `createKanjiReadings`: each kanji's reading forms, derived on first use. With
/// `base`, a kanji's forms are base's and then those of its own readings.
struct KanjiReadings {
    table: HashMap<String, String>,
    base: Option<&'static KanjiReadings>,
    cache: Mutex<HashMap<String, &'static [Text]>>,
}

impl KanjiReadings {
    fn new(table: HashMap<String, String>, base: Option<&'static Self>) -> Self {
        Self { table, base, cache: Mutex::new(HashMap::new()) }
    }

    fn forms(&self, character: &str) -> &'static [Text] {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(forms) = cache.get(character) {
            return forms;
        }
        let forms: Vec<Text> = match (self.table.get(character), self.base) {
            (None, None) => Vec::new(),
            (None, Some(base)) => base.forms(character).to_vec(),
            (Some(readings), None) => kanji_reading_forms(readings),
            (Some(readings), Some(base)) => {
                let mut forms = Forms(base.forms(character).to_vec());
                for form in kanji_reading_forms(readings) {
                    forms.add(form);
                }
                forms.0
            }
        };
        // The tables live for the process, so each kanji's forms are leaked once.
        let forms: &'static [Text] = Box::leak(forms.into_boxed_slice());
        cache.insert(character.to_owned(), forms);
        forms
    }
}

/// One reading of a compound: the reading, and where each character's share
/// starts in it (`None` after the first character of one read as a whole).
struct CompoundReading {
    reading: Text,
    shares: Vec<Option<usize>>,
}

struct Compound {
    characters: Vec<String>,
    readings: Vec<CompoundReading>,
}

/// `parseKanjiCompound`.
fn parse_compound(compound: &str, readings: &str) -> Compound {
    let characters: Vec<String> = compound.chars().map(String::from).collect();
    let readings = readings
        .split(' ')
        .map(|reading| {
            let parts: Vec<Text> = reading.split('|').map(utf16).collect();
            let shares = if parts.len() == 1 {
                (0..characters.len()).map(|index| (index == 0).then_some(0)).collect()
            } else {
                let mut start = 0;
                parts
                    .iter()
                    .map(|part| {
                        start += part.len();
                        Some(start - part.len())
                    })
                    .collect()
            };
            CompoundReading { reading: parts.concat(), shares }
        })
        .collect();
    Compound { characters, readings }
}

/// `createKanwaReadings`: what Jitendex's kanwa data adds to KANJIDIC.
struct Kanwa {
    readings: KanjiReadings,
    compounds: HashMap<String, Vec<Compound>>,
}

impl Kanwa {
    fn adds(&self, character: &str) -> bool {
        self.readings.table.contains_key(character) || self.compounds.contains_key(character)
    }

    /// The known compounds that start at `start` in `characters`.
    fn compounds_at<'a>(&'a self, characters: &[String], start: usize) -> Vec<&'a Compound> {
        self.compounds.get(&characters[start]).map_or_else(Vec::new, |group| {
            group
                .iter()
                .filter(|compound| {
                    compound.characters.iter().enumerate().all(|(index, character)| characters.get(start + index) == Some(character))
                })
                .collect()
        })
    }
}

struct Tables {
    kanji: &'static KanjiReadings,
    kanwa: Kanwa,
}

#[derive(Deserialize)]
struct KanjidicFile {
    readings: HashMap<String, String>,
}

#[derive(Deserialize)]
struct KanwaFile {
    readings: HashMap<String, String>,
    compounds: HashMap<String, String>,
}

/// The readings tables, parsed on first use and kept for the process.
fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let kanjidic: KanjidicFile = serde_json::from_str(KANJIDIC).unwrap_or(KanjidicFile { readings: HashMap::new() });
        let kanwa: KanwaFile = serde_json::from_str(KANWA).unwrap_or(KanwaFile { readings: HashMap::new(), compounds: HashMap::new() });
        let kanji: &'static KanjiReadings = Box::leak(Box::new(KanjiReadings::new(kanjidic.readings, None)));
        let mut compounds: HashMap<String, Vec<Compound>> = HashMap::new();
        for (compound, readings) in &kanwa.compounds {
            let first = compound.chars().next().map(String::from).unwrap_or_default();
            compounds.entry(first).or_default().push(parse_compound(compound, readings));
        }
        Tables { kanji, kanwa: Kanwa { readings: KanjiReadings::new(kanwa.readings, Some(kanji)), compounds } }
    })
}

/// How a run of kanji reads (`readKanjiRun`, `readKanwaRun`): `None` when no
/// split reads, `Some([])` when more than one does, else where each character's
/// share starts and then the reading's length.
type Bounds = Option<Vec<Option<usize>>>;

/// `readKanjiRun` over `characters` (code points) and `reading`.
fn read_kanji_run(characters: &[String], reading: &[u16], readings: &KanjiReadings) -> Bounds {
    // Where the characters so far can end in the reading, each with where the
    // last one's share starts there, or None where more than one split ends there.
    let mut ends: Vec<(usize, Option<usize>)> = vec![(0, Some(0))];
    let mut steps: Vec<Vec<(usize, Option<usize>)>> = Vec::new();
    let mut forms: &[Text] = &[];
    for character in characters {
        if character != "々" {
            forms = readings.forms(character);
        }
        let mut next: Vec<(usize, Option<usize>)> = Vec::new();
        for &(end, start) in &ends {
            for form in forms {
                if !starts_with_at(reading, form, end) {
                    continue;
                }
                let share_end = end + form.len();
                if let Some(entry) = next.iter_mut().find(|(at, _)| *at == share_end) {
                    entry.1 = None;
                } else {
                    next.push((share_end, start.map(|_| end)));
                }
            }
        }
        if next.is_empty() {
            return None;
        }
        ends.clone_from(&next);
        steps.push(next);
    }
    let last = ends.iter().find(|(end, _)| *end == reading.len())?;
    if last.1.is_none() {
        return Some(Vec::new());
    }
    let mut bounds = vec![Some(0); steps.len() + 1];
    bounds[steps.len()] = Some(reading.len());
    for index in (0..steps.len()).rev() {
        let next = bounds[index + 1];
        bounds[index] = steps[index].iter().find(|(end, _)| Some(*end) == next).and_then(|(_, start)| *start);
    }
    Some(bounds)
}

/// A split `readKanwaRun` keeps: how many characters it reads by compounds, and
/// each character's share (`None` for the whole when more than one split ties).
#[derive(Clone)]
struct Split {
    count: usize,
    shares: Option<Vec<Option<usize>>>,
}

/// `betterSplit`.
fn better_split(found: Option<Split>, count: usize, shares: Vec<Option<usize>>, rest: Option<Split>) -> Option<Split> {
    let Some(rest) = rest else { return found };
    let split = Split { count: count + rest.count, shares: rest.shares.map(|rest| shares.into_iter().chain(rest).collect()) };
    let Some(found) = found else { return Some(split) };
    if split.count > found.count {
        return Some(split);
    }
    let same = matches!((&split.shares, &found.shares), (Some(left), Some(right)) if left == right);
    if split.count < found.count || same {
        return Some(found);
    }
    Some(Split { count: found.count, shares: None })
}

/// `readKanwaRun`: a run that no split reads by KANJIDIC, read by kanwa.
fn read_kanwa_run(characters: &[String], reading: &[u16], kanwa: &Kanwa) -> Bounds {
    if !characters.iter().any(|character| kanwa.adds(character)) {
        return None;
    }
    let starting: Vec<Vec<&Compound>> = (0..characters.len()).map(|index| kanwa.compounds_at(characters, index)).collect();
    let mut forms: &[Text] = &[];
    let character_forms: Vec<&[Text]> = characters
        .iter()
        .map(|character| {
            if character != "々" {
                forms = kanwa.readings.forms(character);
            }
            forms
        })
        .collect();
    let mut best: HashMap<(usize, usize), Option<Split>> = HashMap::new();
    let split = read_from(0, 0, characters.len(), reading, &starting, &character_forms, &mut best)?;
    Some(split.shares.map_or_else(Vec::new, |shares| shares.into_iter().chain([Some(reading.len())]).collect()))
}

fn read_from(
    index: usize,
    end: usize,
    count: usize,
    reading: &[u16],
    starting: &[Vec<&Compound>],
    character_forms: &[&[Text]],
    best: &mut HashMap<(usize, usize), Option<Split>>,
) -> Option<Split> {
    if index == count {
        return (end == reading.len()).then(|| Split { count: 0, shares: Some(Vec::new()) });
    }
    if let Some(known) = best.get(&(index, end)) {
        return known.clone();
    }
    let mut found = None;
    for compound in &starting[index] {
        let length = compound.characters.len();
        for CompoundReading { reading: whole, shares } in &compound.readings {
            if !starts_with_at(reading, whole, end) {
                continue;
            }
            let rest = read_from(index + length, end + whole.len(), count, reading, starting, character_forms, best);
            found = better_split(found, length, shares.iter().map(|share| share.map(|share| end + share)).collect(), rest);
        }
    }
    for form in character_forms[index] {
        if starts_with_at(reading, form, end) {
            let rest = read_from(index + 1, end + form.len(), count, reading, starting, character_forms, best);
            found = better_split(found, 0, vec![Some(end)], rest);
        }
    }
    best.insert((index, end), found.clone());
    found
}

/// One furigana segment: text, and its reading (empty for kana).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Segment {
    text: Text,
    reading: Text,
}

struct Group {
    is_kana: bool,
    text: Text,
    characters: Vec<String>,
    normalized: Text,
}

/// `getFuriganaKanaSegments`.
fn kana_segments(text: &[u16], reading: &[u16]) -> Vec<Segment> {
    let same = |index: usize| reading.get(index) == text.get(index);
    let mut segments = Vec::new();
    let mut start = 0;
    let mut state = same(0);
    for index in 1..text.len() {
        let next = same(index);
        if state == next {
            continue;
        }
        segments.push(Segment {
            text: substring(text, start, index),
            reading: if state { Vec::new() } else { substring(reading, start, index) },
        });
        state = next;
        start = index;
    }
    segments.push(Segment {
        text: text[start.min(text.len())..].to_vec(),
        reading: if state { Vec::new() } else { substring(reading, start, text.len()) },
    });
    segments
}

/// `segmentizeFurigana`.
fn segmentize(
    reading: &[u16],
    normalized: &[u16],
    groups: &[Group],
    start: usize,
    readings: Option<&KanjiReadings>,
) -> Option<Vec<Segment>> {
    let Some(group) = groups.get(start) else { return reading.is_empty().then(Vec::new) };
    let remaining = groups.len() - start;
    let length = group.text.len();
    if group.is_kana {
        if !normalized.starts_with(&group.normalized) {
            return None;
        }
        let mut segments = segmentize(
            &substring(reading, length, reading.len()),
            &substring(normalized, length, normalized.len()),
            groups,
            start + 1,
            readings,
        )?;
        let head = if reading.starts_with(&group.text) {
            vec![Segment { text: group.text.clone(), reading: Vec::new() }]
        } else {
            kana_segments(&group.text, reading)
        };
        segments.splice(0..0, head);
        return Some(segments);
    }
    let mut result = None;
    let mut index = reading.len();
    while index >= length {
        let reads = readings.is_none_or(|readings| read_kanji_run(&group.characters, &substring(normalized, 0, index), readings).is_some());
        let segments = if reads {
            segmentize(
                &substring(reading, index, reading.len()),
                &substring(normalized, index, normalized.len()),
                groups,
                start + 1,
                readings,
            )
        } else {
            None
        };
        if let Some(mut segments) = segments {
            if result.is_some() {
                return None;
            }
            segments.insert(0, Segment { text: group.text.clone(), reading: substring(reading, 0, index) });
            result = Some(segments);
        }
        if remaining == 1 || index == 0 {
            break;
        }
        index -= 1;
    }
    result
}

/// `distributeFurigana`: `None` where no split, or more than one, spells the reading.
fn distribute(expression: &str, reading: &str, readings: Option<&KanjiReadings>) -> Option<Vec<Segment>> {
    if reading.is_empty() || reading == expression {
        return Some(vec![Segment { text: utf16(expression), reading: Vec::new() }]);
    }
    let mut groups: Vec<Group> = Vec::new();
    for character in expression.chars() {
        let kana = is_kana(character);
        let mut buffer = [0; 2];
        let encoded = character.encode_utf16(&mut buffer);
        match groups.last_mut() {
            Some(group) if group.is_kana == kana => {
                group.text.extend_from_slice(encoded);
                group.characters.push(character.to_string());
            }
            _ => groups.push(Group {
                is_kana: kana,
                text: encoded.to_vec(),
                characters: vec![character.to_string()],
                normalized: Vec::new(),
            }),
        }
    }
    for group in &mut groups {
        if group.is_kana {
            group.normalized = to_hiragana(&group.text);
        }
    }
    let reading = utf16(reading);
    segmentize(&reading, &to_hiragana(&reading), &groups, 0, readings)
}

/// `splitKanjiRuns`: each run of two or more kanji read one kanji at a time
/// where exactly one split reads. `None` where no run splits.
fn split_kanji_runs(segments: &[Segment], tables: &Tables) -> Option<Vec<Segment>> {
    let mut split = Vec::new();
    for segment in segments {
        let characters: Vec<String> = String::from_utf16_lossy(&segment.text).chars().map(String::from).collect();
        if characters.len() < 2 || segment.reading.is_empty() {
            split.push(segment.clone());
            continue;
        }
        let reading = to_hiragana(&segment.reading);
        let bounds = read_kanji_run(&characters, &reading, tables.kanji).or_else(|| read_kanwa_run(&characters, &reading, &tables.kanwa));
        let Some(bounds) = bounds.filter(|bounds| !bounds.is_empty()) else {
            split.push(segment.clone());
            continue;
        };
        let mut start = 0;
        for end in 1..=characters.len() {
            let Some(to) = bounds[end] else { continue };
            let from = bounds[start].unwrap_or(0);
            split.push(Segment { text: utf16(&characters[start..end].concat()), reading: slice(&segment.reading, from, to) });
            start = end;
        }
    }
    (split.len() != segments.len()).then_some(split)
}

/// `kanjiFurigana`: the split a headword's kanji readings give it, or `None`
/// where that is the split the renderer makes alone.
fn kanji_furigana(expression: &str, reading: &str) -> Option<Vec<Segment>> {
    let tables = tables();
    if let Some(local) = distribute(expression, reading, None) {
        return split_kanji_runs(&local, tables);
    }
    let segments = distribute(expression, reading, Some(tables.kanji))?;
    Some(split_kanji_runs(&segments, tables).unwrap_or(segments))
}

/// `withFurigana` for one term: its furigana as the reply carries it, if any.
pub(crate) fn furigana(expression: &str, reading: &str) -> Option<Value> {
    let segments = kanji_furigana(expression, reading)?;
    Some(Value::Array(
        segments
            .into_iter()
            .map(|segment| json!({"text": String::from_utf16_lossy(&segment.text), "reading": String::from_utf16_lossy(&segment.reading)}))
            .collect(),
    ))
}

/// One segment of a split, as `(text, reading)`; the reading is empty for kana.
pub(crate) type Piece = (String, String);

fn pieces(segments: Vec<Segment>) -> Vec<Piece> {
    segments.into_iter().map(|segment| (String::from_utf16_lossy(&segment.text), String::from_utf16_lossy(&segment.reading))).collect()
}

/// `segmentFurigana`: the renderer's own split, or the whole word under its reading.
fn segment_furigana(expression: &[u16], reading: &[u16]) -> Vec<Piece> {
    let (expression, reading) = (String::from_utf16_lossy(expression), String::from_utf16_lossy(reading));
    distribute(&expression, &reading, None).map_or_else(|| vec![(expression, reading)], pieces)
}

/// `termFurigana`: the reply's `furigana` where it spells the expression, else the renderer's split.
pub(crate) fn term_furigana(term: &Value) -> Vec<Piece> {
    let expression = term.get("expression").and_then(Value::as_str).unwrap_or_default();
    let reading = term.get("reading").and_then(Value::as_str).unwrap_or_default();
    let given: Option<Vec<Piece>> = term.get("furigana").and_then(Value::as_array).filter(|list| !list.is_empty()).and_then(|list| {
        list.iter().map(|segment| Some((segment.get("text")?.as_str()?.to_owned(), segment.get("reading")?.as_str()?.to_owned()))).collect()
    });
    match given {
        Some(given) if given.iter().map(|(text, _)| text.as_str()).collect::<String>() == expression => given,
        _ => segment_furigana(&utf16(expression), &utf16(reading)),
    }
}

/// `matchedFurigana` (Yomitan's `distributeFuriganaInflected`): the furigana of
/// `matched`, the text a lookup matched for `term`.
pub(crate) fn matched_furigana(term: &Value, matched: &str) -> Vec<Piece> {
    let expression = term.get("expression").and_then(Value::as_str).unwrap_or_default();
    if matched == expression {
        return term_furigana(term);
    }
    let (expression, matched) = (utf16(expression), utf16(matched));
    let reading = utf16(term.get("reading").and_then(Value::as_str).unwrap_or_default());
    let stem_length =
        |text: &[u16]| text.iter().zip(&matched).take_while(|(left, right)| to_hiragana(&[**left]) == to_hiragana(&[**right])).count();
    let stem = stem_length(&expression);
    let reading_stem = stem_length(&reading);
    let whole = |reading: &[u16]| vec![(String::from_utf16_lossy(&matched), String::from_utf16_lossy(reading))];
    if reading_stem > 0 && reading_stem >= stem {
        return whole(&[]);
    }
    let ending = &expression[stem..];
    if stem == 0 || !reading.ends_with(ending) {
        return whole(&reading);
    }
    let mut split = segment_furigana(&expression[..stem], &reading[..reading.len() - ending.len()]);
    split.push((String::from_utf16_lossy(&matched[stem..]), String::new()));
    split
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(expression: &str, reading: &str) -> Option<Vec<(String, String)>> {
        kanji_furigana(expression, reading).map(|segments| {
            segments
                .into_iter()
                .map(|segment| (String::from_utf16_lossy(&segment.text), String::from_utf16_lossy(&segment.reading)))
                .collect()
        })
    }

    #[allow(clippy::unnecessary_wraps, reason = "compared with split's Option")]
    fn pairs(list: &[(&str, &str)]) -> Option<Vec<(String, String)>> {
        Some(list.iter().map(|(text, reading)| ((*text).to_owned(), (*reading).to_owned())).collect())
    }

    #[test]
    fn runs_of_kanji_are_read_one_kanji_at_a_time() {
        assert_eq!(
            split("二十四時間", "にじゅうよじかん"),
            pairs(&[("二", "に"), ("十", "じゅう"), ("四", "よ"), ("時", "じ"), ("間", "かん")])
        );
        assert_eq!(split("日本", "にほん"), pairs(&[("日", "に"), ("本", "ほん")]));
        assert_eq!(split("今日中", "きょうじゅう"), pairs(&[("今日", "きょう"), ("中", "じゅう")]));
    }

    #[test]
    fn a_split_the_renderer_makes_alone_is_left_out() {
        assert_eq!(split("食べる", "たべる"), None);
        assert_eq!(split("猫", "ねこ"), None);
        assert_eq!(split("ねこ", ""), None);
        assert_eq!(split("景色", "けしき"), None);
    }

    /// Every case `scripts/furigana-cases.mjs` recorded from the extension's own
    /// kanjiFurigana gets the same answer here.
    #[test]
    fn every_recorded_case_matches_the_extension() {
        let cases: Vec<(String, String, Value)> = serde_json::from_str(include_str!("testdata/furigana.json")).unwrap_or_default();
        assert!(cases.len() > 1000);
        let wrong: Vec<_> = cases
            .iter()
            .filter(|(expression, reading, expected)| furigana(expression, reading).unwrap_or(Value::Null) != *expected)
            .map(|(expression, reading, expected)| (expression, reading, expected, furigana(expression, reading)))
            .collect();
        assert!(wrong.is_empty(), "{} of {} differ, first {:?}", wrong.len(), cases.len(), wrong.iter().take(5).collect::<Vec<_>>());
    }
}
