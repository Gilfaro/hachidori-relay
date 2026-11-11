// SPDX-License-Identifier: GPL-3.0-or-later
//! `frequency-mode.js` and `engine-service.js`' `correctRankFrequencyMode`: a
//! frequency list that declares `occurrence-based` but holds ranks (Hachidori
//! #684) is stored as `rank-based`, so its averages and Auto direction treat it
//! as one.

use std::fs::{self, File};
use std::io::BufReader;
use std::path::Path;

use serde_json::Value;

/// `META_BANK`: `term_meta_bank_N.json` or `kanji_meta_bank_N.json` at the root.
fn meta_bank(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("term_meta_bank_").or_else(|| name.strip_prefix("kanji_meta_bank_")) else {
        return false;
    };
    rest.strip_suffix(".json").is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

/// `row?.[n]`: an array's element, or an object's member named by the index.
fn member(row: &Value, index: usize) -> Option<&Value> {
    match row {
        Value::Array(items) => items.get(index),
        Value::Object(fields) => fields.get(&index.to_string()),
        _ => None,
    }
}

/// `frequencyValue`: a freq row's number in each Yomitan shape: n, {value} or
/// either under {reading, frequency}.
fn frequency_value(data: &Value) -> Option<f64> {
    let frequency = match data {
        Value::Object(fields) if fields.contains_key("frequency") => &fields["frequency"],
        _ => data,
    };
    let value = match frequency {
        Value::Object(fields) => fields.get("value")?,
        Value::Array(_) => return None,
        other => other,
    };
    value.as_f64().filter(|value| value.is_finite())
}

/// `archiveHoldsRanks`: in an occurrence count, words seen once are a large
/// share of the list (Zipf's law); in a rank list only the top word, with its
/// few readings, has 1. So a list whose smallest value is 1, held by at most 1%
/// of its freq rows, is read as ranks. Values compare exactly, as `===` does.
#[expect(clippy::float_cmp, reason = "JavaScript's strict equality, as `value === 1` compares")]
pub(super) fn archive_holds_ranks(archive: &Path) -> Result<bool, String> {
    let file = File::open(archive).map_err(|error| error.to_string())?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(|error| error.to_string())?;
    let (mut rows, mut ones, mut smallest) = (0_u64, 0_u64, f64::INFINITY);
    for index in 0..zip.len() {
        let entry = zip.by_index(index).map_err(|error| error.to_string())?;
        if entry.is_dir() || !entry.name().is_ok_and(|name| meta_bank(&name)) {
            continue;
        }
        // One bank's JSON is in memory at once, as the extension reads them.
        let bank: Value = serde_json::from_reader(BufReader::new(entry)).map_err(|error| error.to_string())?;
        let Value::Array(bank) = bank else {
            return Err("a frequency bank is not a list".to_owned());
        };
        for row in &bank {
            if member(row, 1).and_then(Value::as_str) != Some("freq") {
                continue;
            }
            let Some(value) = member(row, 2).and_then(frequency_value) else { continue };
            rows += 1;
            if value == 1.0 {
                ones += 1;
            }
            smallest = smallest.min(value);
        }
    }
    Ok(smallest == 1.0 && ones * 100 <= rows)
}

/// `correctRankFrequencyMode`: the generated index in `dir` keeps the archive's
/// `frequencyMode`, unless it declares occurrence counts that are ranks.
pub(super) fn correct_rank_frequency_mode(archive: &Path, dir: &Path) -> Result<(), String> {
    let path = dir.join("index.json");
    let mut index: Value =
        serde_json::from_str(&fs::read_to_string(&path).map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
    if index.get("frequencyMode").and_then(Value::as_str) != Some("occurrence-based") || !archive_holds_ranks(archive)? {
        return Ok(());
    }
    index["frequencyMode"] = Value::from("rank-based");
    fs::write(&path, index.to_string()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::json;

    use super::*;
    use crate::host::store;

    fn archive(dir: &Path, banks: &[(&str, Value)]) -> std::path::PathBuf {
        let path = dir.join("archive.zip");
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        for (name, bank) in banks {
            zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(bank.to_string().as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    fn ranks(count: u64) -> Value {
        Value::Array((1..=count).map(|rank| json!([format!("w{rank}"), "freq", rank])).collect())
    }

    #[test]
    fn bank_names_are_root_meta_banks() {
        assert!(meta_bank("term_meta_bank_1.json") && meta_bank("kanji_meta_bank_12.json"));
        assert!(!meta_bank("term_meta_bank_.json") && !meta_bank("x/term_meta_bank_1.json") && !meta_bank("term_bank_1.json"));
    }

    #[test]
    fn values_in_each_yomitan_shape() {
        assert_eq!(frequency_value(&json!(5)), Some(5.0));
        assert_eq!(frequency_value(&json!({ "value": 6, "displayValue": "6" })), Some(6.0));
        assert_eq!(frequency_value(&json!({ "reading": "a", "frequency": 7 })), Some(7.0));
        assert_eq!(frequency_value(&json!({ "reading": "a", "frequency": { "value": 8 } })), Some(8.0));
        assert_eq!(frequency_value(&json!("9")), None);
        assert_eq!(frequency_value(&json!({ "frequency": null })), None);
    }

    #[test]
    fn a_list_with_few_ones_is_ranks() {
        let dir = store::tests::scratch("frequency-ranks");
        fs::create_dir_all(&dir).unwrap();
        assert!(archive_holds_ranks(&archive(dir.as_path(), &[("term_meta_bank_1.json", ranks(200))])).unwrap());
        let mut counts = ranks(200);
        counts.as_array_mut().unwrap().extend((0..50).map(|n| json!([format!("rare{n}"), "freq", 1])));
        assert!(!archive_holds_ranks(&archive(dir.as_path(), &[("term_meta_bank_1.json", counts)])).unwrap());
        let from_two = Value::Array((2..=200).map(|rank| json!(["w", "freq", rank])).collect());
        assert!(!archive_holds_ranks(&archive(dir.as_path(), &[("term_meta_bank_1.json", from_two)])).unwrap());
        assert!(!archive_holds_ranks(&archive(dir.as_path(), &[("term_bank_1.json", ranks(200))])).unwrap());
    }

    #[test]
    fn only_a_declared_occurrence_list_is_rewritten() {
        let dir = store::tests::scratch("frequency-index");
        fs::create_dir_all(&dir).unwrap();
        let zip = archive(dir.as_path(), &[("term_meta_bank_1.json", ranks(200))]);
        for (declared, stored) in [("occurrence-based", "rank-based"), ("rank-based", "rank-based")] {
            fs::write(dir.as_path().join("index.json"), json!({ "title": "F", "frequencyMode": declared }).to_string()).unwrap();
            correct_rank_frequency_mode(&zip, dir.as_path()).unwrap();
            let index: Value = serde_json::from_str(&fs::read_to_string(dir.as_path().join("index.json")).unwrap()).unwrap();
            assert_eq!(index["frequencyMode"], stored);
            assert_eq!(index["title"], "F");
        }
    }
}
