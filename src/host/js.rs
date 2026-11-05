// SPDX-License-Identifier: GPL-3.0-or-later
//! JavaScript semantics over JSON values, for the parts of the extension the relay
//! ports: what `typeof`, truthiness, `Number()`, `===`, `String.prototype.trim` and
//! `sameJsonValue` do to a value that came from `JSON.parse`.

use serde_json::{Map, Value};

/// `Number.MAX_SAFE_INTEGER`.
pub(crate) const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// `typeof value`; a missing value is `undefined`.
pub(crate) fn type_of(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null | Value::Array(_) | Value::Object(_)) => "object",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
    }
}

/// Truthiness.
pub(crate) fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `value && typeof value === "object" && !Array.isArray(value)`.
pub(crate) fn plain_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

/// `left === right`. Two arrays or objects from different places are never the same.
pub(crate) fn strict_eq(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (left, right) {
        (None, None) | (Some(Value::Null), Some(Value::Null)) => true,
        (Some(Value::Bool(left)), Some(Value::Bool(right))) => left == right,
        (Some(Value::Number(left)), Some(Value::Number(right))) => left.as_f64() == right.as_f64(),
        (Some(Value::String(left)), Some(Value::String(right))) => left == right,
        _ => false,
    }
}

/// `sameJsonValue` (`json-value.js`): structural equality, numbers by value.
pub(crate) fn same_json(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len() && left.iter().zip(right).all(|(left, right)| same_json(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len() && left.iter().all(|(key, value)| right.get(key).is_some_and(|other| same_json(value, other)))
        }
        _ => strict_eq(Some(left), Some(right)),
    }
}

/// `Number.isInteger(value) && value >= 0`, as the number.
pub(crate) fn whole(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|number| number.fract() == 0.0 && number.is_finite() && *number >= 0.0)
}

/// `Number.isSafeInteger(value)`.
pub(crate) fn safe_integer(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|number| number.fract() == 0.0 && number.abs() <= MAX_SAFE_INTEGER)
}

/// A number as `JSON.stringify` would write it: integers without a fraction.
pub(crate) fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER {
        #[allow(clippy::cast_possible_truncation, reason = "a safe integer")]
        let integer = value as i64;
        Value::from(integer)
    } else {
        serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

/// The ECMAScript `WhiteSpace` and `LineTerminator` characters `trim` removes.
pub(crate) fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `String.prototype.trim`.
pub(crate) fn trim(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// `/[\u0000-\u001f\u007f]/u.test(text)`.
pub(crate) fn has_control(text: &str) -> bool {
    text.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}')
}

/// A string's `length`: its UTF-16 code units.
pub(crate) fn length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `Number(value)` for a JSON value.
pub(crate) fn to_number(value: Option<&Value>) -> f64 {
    match value {
        None | Some(Value::Object(_)) => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(flag)) => f64::from(u8::from(*flag)),
        Some(Value::Number(number)) => number.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(text)) => string_to_number(text),
        // An array converts through its string: [] is "", [x] is String(x).
        Some(Value::Array(items)) => match items.as_slice() {
            [] | [Value::Null] => 0.0,
            [Value::String(text)] => string_to_number(text),
            [Value::Number(number)] => number.as_f64().unwrap_or(f64::NAN),
            [inner @ Value::Array(_)] => to_number(Some(inner)),
            _ => f64::NAN,
        },
    }
}

/// `Number(string)`: trimmed decimal, hex/octal/binary literals, ±Infinity.
pub(crate) fn string_to_number(text: &str) -> f64 {
    let trimmed = trim(text);
    if trimmed.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = trimmed.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            // Digit by digit, rounding as Number() does for long literals.
            return digits.chars().fold(0.0, |total, c| total * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0)));
        }
    }
    match trimmed {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ if trimmed.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) => trimmed.parse().unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

/// `String(value)` for a JSON value, as template literals and `String()` print it.
pub(crate) fn to_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_owned(),
        Some(Value::Null) => "null".to_owned(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(Value::Number(number)) => number.as_f64().map_or_else(|| number.to_string(), number_to_string),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => {
            items.iter().map(|item| if item.is_null() { String::new() } else { to_string(Some(item)) }).collect::<Vec<_>>().join(",")
        }
        Some(Value::Object(_)) => "[object Object]".to_owned(),
    }
}

/// `Number.prototype.toString()`: the shortest round-trip digits, in exponent form
/// from 1e21 and below 1e-6.
pub(crate) fn number_to_string(number: f64) -> String {
    if number.is_nan() {
        return "NaN".to_owned();
    }
    if number.is_infinite() {
        return if number > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    if number == 0.0 {
        return "0".to_owned();
    }
    if (1e-6..1e21).contains(&number.abs()) {
        return number.to_string();
    }
    let written = format!("{number:e}");
    let (mantissa, exponent) = written.split_once('e').unwrap_or((&written, "0"));
    let exponent: i32 = exponent.parse().unwrap_or_default();
    format!("{mantissa}e{}{}", if exponent < 0 { '-' } else { '+' }, exponent.unsigned_abs())
}

/// The `TypeError` V8 throws reading `key` of `null` or `undefined`.
pub(crate) fn null_property(value: Option<&Value>, key: &str) -> String {
    format!("Cannot read properties of {} (reading '{key}')", if value.is_none() { "undefined" } else { "null" })
}

/// `value.key` where `value` must not be `null` (V8's `TypeError` otherwise).
pub(crate) fn property<'a>(value: Option<&'a Value>, key: &str) -> Result<Option<&'a Value>, String> {
    match value {
        None | Some(Value::Null) => Err(null_property(value, key)),
        Some(Value::Object(fields)) => Ok(fields.get(key)),
        Some(_) => Ok(None),
    }
}

/// `new Date(time).toISOString()`, or `None` where it throws (an invalid time).
pub(crate) fn iso_time(time: f64) -> Option<String> {
    if !time.is_finite() || time.abs() > 8.64e15 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, reason = "TimeClip: a whole number within ±8.64e15")]
    let time = time.trunc() as i64;
    let (days, in_day) = (time.div_euclid(86_400_000), time.rem_euclid(86_400_000));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    let year = match year {
        0..=9999 => format!("{year:04}"),
        ..0 => format!("-{:06}", -year),
        _ => format!("+{year:06}"),
    };
    let (seconds, millis) = (in_day / 1000, in_day % 1000);
    Some(format!("{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z", seconds / 3600, seconds % 3600 / 60, seconds % 60))
}

/// `Date.now()`.
pub(crate) fn now() -> f64 {
    let elapsed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    #[allow(clippy::cast_precision_loss, reason = "milliseconds since 1970 fit an f64 exactly")]
    let now = elapsed.as_millis() as f64;
    now
}

/// `new Date().toISOString()`.
pub(crate) fn now_iso() -> String {
    iso_time(now()).unwrap_or_default()
}

/// `left < right` and `left > right` for strings, as `(left > right) - (left < right)`:
/// JavaScript compares UTF-16 code units.
pub(crate) fn compare(left: &str, right: &str) -> std::cmp::Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

/// `/[\p{L}\p{N}]/u.test(c)`.
pub(crate) fn is_letter_or_number(c: char) -> bool {
    use icu_properties::CodePointMapData;
    use icu_properties::props::{GeneralCategory, GeneralCategoryGroup};
    let category = CodePointMapData::<GeneralCategory>::new().get(c);
    GeneralCategoryGroup::Letter.contains(category) || GeneralCategoryGroup::Number.contains(category)
}

/// `String.prototype.slice(start, end)` in UTF-16 code units, for indexes that are
/// whatever `Number` makes of them (NaN is 0, a negative index counts from the end).
pub(crate) fn slice(text: &str, start: f64, end: Option<f64>) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "clamped to the string's length"
    )]
    let index = |value: f64| -> usize {
        let length = units.len() as f64;
        let value = if value.is_nan() { 0.0 } else { value.trunc() };
        (if value < 0.0 { (length + value).max(0.0) } else { value.min(length) }) as usize
    };
    let from = index(start);
    let to = end.map_or(units.len(), index);
    if from >= to { String::new() } else { String::from_utf16_lossy(&units[from..to]) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn times_print_as_to_iso_string_prints_them() {
        assert_eq!(iso_time(0.0).as_deref(), Some("1970-01-01T00:00:00.000Z"));
        assert_eq!(iso_time(1_700_000_000_123.9).as_deref(), Some("2023-11-14T22:13:20.123Z"));
        assert_eq!(iso_time(-1.0).as_deref(), Some("1969-12-31T23:59:59.999Z"));
        assert_eq!(iso_time(8.64e15).as_deref(), Some("+275760-09-13T00:00:00.000Z"));
        assert_eq!(iso_time(-8.64e15).as_deref(), Some("-271821-04-20T00:00:00.000Z"));
        assert_eq!(iso_time(-62_198_755_200_000.0).as_deref(), Some("-000001-01-01T00:00:00.000Z"));
        assert_eq!(iso_time(f64::NAN), None);
        assert_eq!(iso_time(8.64e15 + 1.0), None);
        let now = now_iso();
        assert!(now.len() == 24 && now.starts_with("20") && now.ends_with('Z'), "{now}");
    }

    #[test]
    fn numbers_convert_as_javascript_converts_them() {
        for (value, expected) in [
            (json!(" 12 "), 12.0),
            (json!("\u{3000}7\u{feff}"), 7.0),
            (json!("0x1F"), 31.0),
            (json!("0x"), f64::NAN),
            (json!("1e3"), 1000.0),
            (json!(""), 0.0),
            (json!([]), 0.0),
            (json!(["4"]), 4.0),
            (json!([1, 2]), f64::NAN),
            (json!(true), 1.0),
            (json!(null), 0.0),
            (json!({}), f64::NAN),
            (json!("-Infinity"), f64::NEG_INFINITY),
        ] {
            let got = to_number(Some(&value));
            assert!(got.to_bits() == expected.to_bits() || (got.is_nan() && expected.is_nan()), "{value}: {got}");
        }
        assert!(to_number(None).is_nan());
    }

    #[test]
    fn values_compare_and_print_as_javascript_does() {
        assert!(same_json(&json!({"a": [1, {"b": 2.0}]}), &json!({"a": [1.0, {"b": 2}]})));
        assert!(!same_json(&json!({"a": 1}), &json!({"a": 1, "b": null})));
        assert!(!strict_eq(Some(&json!([])), Some(&json!([]))));
        assert!(strict_eq(None, None));
        assert_eq!(to_string(Some(&json!([1, null, "x"]))), "1,,x");
        assert_eq!(to_string(Some(&json!(3.0))), "3");
        assert_eq!(trim("\u{feff} a \u{3000}"), "a");
        assert_eq!(length("𠮷"), 2);
        assert_eq!(number(5.0), json!(5));
        assert_eq!(property(Some(&json!(null)), "title"), Err("Cannot read properties of null (reading 'title')".to_owned()));
    }
}
