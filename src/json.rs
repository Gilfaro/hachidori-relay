// SPDX-License-Identifier: GPL-3.0-or-later
//! Helpers for looking at JSON without building a tree of it.
//!
//! Host replies can be tens of MiB, and most of that passes through the relay
//! untouched. They stay as [`RawValue`] slices, and only the fields the relay
//! looks at are decoded.

use std::borrow::Cow;
use std::collections::HashMap;

use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

/// For `#[serde(default, deserialize_with = "present", borrow)]` on an `Option<&RawValue>`
/// field. A key that is present gives `Some`, even when its value is `null`.
pub(crate) fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<&'de RawValue>, D::Error> {
    <&'de RawValue>::deserialize(deserializer).map(Some)
}

fn first_byte(raw: &RawValue) -> Option<u8> {
    raw.get().trim_start().bytes().next()
}

/// Whether the text is a JSON object. Callers check this before decoding into a struct,
/// because serde would also accept an array there.
pub(crate) fn is_object_text(text: &str) -> bool {
    text.trim_start().starts_with('{')
}

pub(crate) fn is_object(raw: &RawValue) -> bool {
    first_byte(raw) == Some(b'{')
}

pub(crate) fn is_array(raw: &RawValue) -> bool {
    first_byte(raw) == Some(b'[')
}

pub(crate) fn is_true(raw: &RawValue) -> bool {
    raw.get().trim() == "true"
}

/// The string, if the value is one.
pub(crate) fn as_str(raw: &RawValue) -> Option<Cow<'_, str>> {
    // serde has validated the value, so a leading quote means one whole string token.
    let body = raw.get().trim().strip_prefix('"')?.strip_suffix('"')?;
    // serde's own `Cow`/`String` paths always allocate, and for an escaped string they
    // unescape into a doubling scratch buffer and then copy that again. A string
    // without escapes is borrowed as it is; one with escapes is unescaped once, into a
    // buffer of the right size.
    if !body.contains('\\') {
        return Some(Cow::Borrowed(body));
    }
    unescape(body).or_else(|| String::deserialize(raw).ok()).map(Cow::Owned)
}

/// The contents of a validated JSON string token, unescaped. `None` for anything
/// serde would refuse, such as a lone surrogate.
fn unescape(body: &str) -> Option<String> {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let escape = *rest.as_bytes().get(at + 1)?;
        // The escape character is ASCII, so this is a character boundary.
        rest = &rest[at + 2..];
        let character = match escape {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let high = hex4(rest)?;
                rest = &rest[4..];
                if (0xD800..0xDC00).contains(&high) {
                    let low = hex4(rest.strip_prefix("\\u")?)?;
                    if !(0xDC00..0xE000).contains(&low) {
                        return None;
                    }
                    rest = &rest[6..];
                    char::from_u32(0x1_0000 + ((high - 0xD800) << 10) + (low - 0xDC00))?
                } else {
                    char::from_u32(high)?
                }
            }
            _ => return None,
        };
        out.push(character);
    }
    out.push_str(rest);
    Some(out)
}

fn hex4(text: &str) -> Option<u32> {
    let digits = text.get(..4)?;
    if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// The value as text: a string's contents, or any other value's JSON.
pub(crate) fn display(raw: &RawValue) -> String {
    as_str(raw).map_or_else(|| raw.get().trim().to_owned(), Cow::into_owned)
}

/// The top-level fields of a JSON object, with their values still raw. `None` if the
/// value is not an object. When a key repeats, the last value wins.
pub(crate) fn fields(raw: &RawValue) -> Option<HashMap<String, &RawValue>> {
    if !is_object(raw) {
        return None;
    }
    serde_json::from_str(raw.get()).ok()
}

/// The elements of a JSON array, still raw.
pub(crate) fn elements(raw: &RawValue) -> Option<Vec<&RawValue>> {
    if !is_array(raw) {
        return None;
    }
    <Vec<&RawValue>>::deserialize(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_matches_serde() {
        let cases = [
            r#""plain 大 text""#,
            r#""""#,
            r#""quote \" backslash \\ slash \/ controls \b\f\n\r\t""#,
            r#""\u00e9\u5927 and a pair \ud83d\ude00 and \u0000 and \uABCD""#,
            r#""{\"kind\":\"reply\",\"text\":\"\\u5927\"}""#,
            r#""ends with an escape \n""#,
            r#""\ud83d alone""#,
            r#""\ude00 alone""#,
            r#""\ud83d\u0041 bad pair""#,
        ];
        for case in cases {
            let raw = RawValue::from_string(case.to_owned()).unwrap_or_else(|error| panic!("{error}"));
            let ours = as_str(&raw).map(Cow::into_owned);
            assert_eq!(ours, serde_json::from_str::<String>(case).ok(), "{case}");
        }
        let plain = RawValue::from_string(r#""no escapes""#.to_owned()).unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(as_str(&plain), Some(Cow::Borrowed("no escapes"))), "borrowed without escapes");
    }

    fn raw(text: &str) -> Box<RawValue> {
        RawValue::from_string(text.to_owned()).unwrap_or_else(|error| panic!("{error}"))
    }

    #[test]
    fn kinds_are_read_from_the_first_byte() {
        assert!(is_object(&raw("{}")));
        assert!(is_array(&raw("[1]")));
        assert!(!is_array(&raw("\"[\"")));
        assert!(is_true(&raw("true")));
        assert!(!is_true(&raw("1")));
        assert!(is_object_text("  {\"a\":1}"));
        assert!(!is_object_text("[{}]"));
    }

    #[test]
    fn strings_decode_escapes_and_other_values_show_as_json() {
        assert_eq!(as_str(&raw(r#""a\"bé""#)).as_deref(), Some("a\"bé"));
        assert_eq!(as_str(&raw("5")), None);
        assert_eq!(display(&raw(r#""plain""#)), "plain");
        assert_eq!(display(&raw("null")), "null");
        assert_eq!(display(&raw(r#"{"a": 1}"#)), r#"{"a": 1}"#);
    }

    #[test]
    fn elements_stay_raw() {
        let list = raw(r#"[{"index": 0}, "x", 3]"#);
        let items = elements(&list).unwrap_or_default();
        assert_eq!(items.iter().map(|item| item.get()).collect::<Vec<_>>(), [r#"{"index": 0}"#, r#""x""#, "3"]);
        assert!(elements(&raw("{}")).is_none());
    }
}
