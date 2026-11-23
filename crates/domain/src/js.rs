//! JavaScript's semantics where the TypeScript server's output or input rules depend on them:
//! truthiness and `||`, `??` skipping `null` and absent alike, `Number(text)`, `String(x)`,
//! `JSON.parse` and `JSON.stringify`, `trim()`, `.length` and `encodeURIComponent`. Upstream
//! bodies are untrusted and loosely shaped, so every reader answers `None` instead of failing.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::sync::LazyLock;

use serde_json::{Map, Number, Value};

/// `record(value)[key]`: a field of an object; `None` for a non-object or a missing key.
#[must_use]
pub fn field<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    value?.as_object()?.get(key)
}

/// `a ?? b ?? …` over fields of `value`: the first that is present and not `null`.
#[must_use]
pub fn first<'a>(value: Option<&'a Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .find_map(|key| field(value, key).filter(|found| !found.is_null()))
}

/// `value ?? fallback` for one value.
#[must_use]
pub fn nullish(value: Option<&Value>) -> Option<&Value> {
    value.filter(|found| !found.is_null())
}

/// `Object.keys(record(value)).length > 0`.
#[must_use]
pub fn non_empty_record(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_object)
        .is_some_and(|object| !object.is_empty())
}

/// `record(value)`: the object, or an empty one.
#[must_use]
pub fn record(value: Option<&Value>) -> &Map<String, Value> {
    static EMPTY: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
    match value {
        Some(Value::Object(map)) => map,
        _ => &EMPTY,
    }
}

/// `text(value)`: a string, or nothing.
#[must_use]
pub fn text(value: Option<&Value>) -> Option<&str> {
    value?.as_str()
}

/// Whether JavaScript treats `value` as true; an absent value is `undefined`.
#[must_use]
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `value || fallback`.
#[must_use]
pub fn or<'a>(value: Option<&'a Value>, fallback: Option<&'a Value>) -> Option<&'a Value> {
    if truthy(value) { value } else { fallback }
}

/// `number(value)`: a finite number from a JSON number or a non-empty numeric string.
#[must_use]
pub fn number(value: Option<&Value>) -> Option<f64> {
    let parsed = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) if !text.is_empty() => string_to_number(text),
        _ => return None,
    };
    parsed.is_finite().then_some(parsed)
}

/// `Number(text)`: trimmed decimal, `0x`/`0o`/`0b` integers, `Infinity`; blank is 0, anything
/// else `NaN`.
#[must_use]
pub fn string_to_number(text: &str) -> f64 {
    let text = trim(text);
    if text.is_empty() {
        return 0.0;
    }
    let radix = match text.get(..2) {
        Some("0x" | "0X") => 16,
        Some("0o" | "0O") => 8,
        Some("0b" | "0B") => 2,
        _ => 10,
    };
    if radix != 10 {
        return radix_integer(text.get(2..).unwrap_or_default(), radix);
    }
    match text {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        // Rust's grammar over these characters is JavaScript's StrDecimalLiteral.
        _ if text
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E')) =>
        {
            text.parse().unwrap_or(f64::NAN)
        }
        _ => f64::NAN,
    }
}

/// The digits of a `0x`, `0o` or `0b` literal, rounded once to the nearest double as
/// `Number()` rounds them; `NaN` for no digits or a stray character.
fn radix_integer(digits: &str, radix: u32) -> f64 {
    if digits.is_empty() {
        return f64::NAN;
    }
    let bits = radix.trailing_zeros();
    let mut value = 0_u128;
    let mut dropped = 0_u32;
    let mut inexact = false;
    for c in digits.chars() {
        let Some(digit) = c.to_digit(radix) else {
            return f64::NAN;
        };
        if value >> (128 - bits) == 0 {
            value = value << bits | u128::from(digit);
        } else {
            dropped = dropped.saturating_add(1);
            inexact |= digit != 0;
        }
    }
    // Once digits are dropped `value` holds over 120 significant bits, far more than a double's
    // 53, so a set lowest bit stands in for the nonzero digits dropped below it when rounding.
    #[expect(
        clippy::cast_precision_loss,
        reason = "the cast rounds to nearest, ties to even, as Number() does"
    )]
    let rounded = (value | u128::from(inexact)) as f64;
    let scale = i32::try_from(dropped.saturating_mul(bits)).unwrap_or(i32::MAX);
    rounded * 2_f64.powi(scale)
}

/// `unwrap(value)`: some management endpoints wrap a JSON document inside a JSON string.
///
/// # Errors
/// The wrapped string is not JSON.
pub fn unwrap(value: &Value) -> Result<Cow<'_, Value>, serde_json::Error> {
    match value {
        Value::String(text) => serde_json::from_str(text).map(Cow::Owned),
        other => Ok(Cow::Borrowed(other)),
    }
}

/// `String(value)`, with `None` for `undefined`.
#[must_use]
pub fn string(value: Option<&Value>) -> Cow<'_, str> {
    match value {
        None => Cow::Borrowed("undefined"),
        Some(Value::Null) => Cow::Borrowed("null"),
        Some(Value::Bool(true)) => Cow::Borrowed("true"),
        Some(Value::Bool(false)) => Cow::Borrowed("false"),
        Some(Value::Number(number)) => Cow::Owned(number_string(number.as_f64().unwrap_or(0.0))),
        Some(Value::String(text)) => Cow::Borrowed(text),
        Some(Value::Array(items)) => Cow::Owned(
            items
                .iter()
                .map(|item| match item {
                    Value::Null => Cow::Borrowed(""),
                    other => string(Some(other)),
                })
                .collect::<Vec<_>>()
                .join(","),
        ),
        Some(Value::Object(_)) => Cow::Borrowed("[object Object]"),
    }
}

/// `Number.prototype.toString()`: the shortest round-trip digits, in positional notation from
/// 1e-6 up to 1e21 and in exponent notation (`1e+21`, `1.5e-7`) outside it.
#[must_use]
pub fn number_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value == 0.0 {
        return "0".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exponent: i64 = exponent.parse().unwrap_or(0);
    let count = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    let point = exponent + 1;
    let body = if count <= point && point <= 21 {
        format!(
            "{digits}{}",
            "0".repeat(usize::try_from(point - count).unwrap_or(0))
        )
    } else if 0 < point && point <= 21 {
        let (whole, fraction) = digits.split_at(usize::try_from(point).unwrap_or(0));
        format!("{whole}.{fraction}")
    } else if -6 < point && point <= 0 {
        format!(
            "0.{}{digits}",
            "0".repeat(usize::try_from(-point).unwrap_or(0))
        )
    } else {
        let (lead, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{lead}{fraction}e{sign}{}", exponent.unsigned_abs())
    };
    format!("{sign}{body}")
}

/// `JSON.stringify(value)`: keys in their order, numbers as JavaScript writes them.
#[must_use]
pub fn stringify(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Number(number) => {
            let number = number.as_f64().unwrap_or(f64::NAN);
            // JSON has no NaN or infinities; JavaScript writes them as null.
            if number.is_finite() {
                out.push_str(&number_string(number));
            } else {
                out.push_str("null");
            }
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write(item, out);
            }
            out.push('}');
        }
        Value::Null | Value::Bool(_) | Value::String(_) => out.push_str(&value.to_string()),
    }
}

/// Whether JavaScript's `\s` and `trim()` count `c` as white space. Rust's `is_whitespace`
/// differs on U+0085 and U+FEFF.
#[must_use]
pub fn is_space(c: char) -> bool {
    c == '\u{feff}' || (c != '\u{85}' && c.is_whitespace())
}

/// `String.prototype.trim()`.
#[must_use]
pub fn trim(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// `JSON.parse`: `None` for invalid JSON. Objects take JavaScript's key order (array-index keys
/// first, ascending, then the rest as written) and whole numbers below 2^53 become integers, so
/// a merged file is written back the way the TypeScript server wrote it.
#[must_use]
pub fn parse(content: &str) -> Option<Value> {
    serde_json::from_str(content).ok().map(javascript)
}

fn javascript(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let (mut indices, names): (Vec<_>, Vec<_>) = object
                .into_iter()
                .map(|(key, value)| (key, javascript(value)))
                .partition(|(key, _)| array_index(key).is_some());
            indices.sort_by_key(|(key, _)| array_index(key));
            Value::Object(indices.into_iter().chain(names).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(javascript).collect()),
        Value::Number(number) => Value::Number(match number.as_f64().and_then(whole) {
            Some(integer) if number.is_f64() => Number::from(integer),
            _ => number,
        }),
        other => other,
    }
}

/// A canonical array index (`"0"`, `"42"`, below 2^32 − 1), which JavaScript orders first.
fn array_index(key: &str) -> Option<u32> {
    let canonical = key == "0"
        || (!key.starts_with('0') && !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit()));
    key.parse::<u32>()
        .ok()
        .filter(|&index| canonical && index != u32::MAX)
}

/// `value` as the integer JavaScript writes without a fraction: a whole value below 2^53 in
/// magnitude, past which f64 no longer holds every integer and JavaScript writes exponents.
#[must_use]
pub fn whole(value: f64) -> Option<i64> {
    const SAFE: f64 = 9_007_199_254_740_992.0;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a whole value below 2^53 converts exactly"
    )]
    (value.fract() == 0.0 && value.abs() < SAFE).then_some(value as i64)
}

/// `JSON.stringify(value, null, 2)`.
#[must_use]
pub fn pretty(value: &Value) -> String {
    format!("{value:#}")
}

/// `JSON.stringify(text)`: a JSON string literal, escaped as JavaScript escapes it.
#[must_use]
pub fn quote(text: &str) -> String {
    stringify(&Value::from(text))
}

/// A string's length in UTF-16 code units, as zod's `.min()` and `.max()` count it.
#[must_use]
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `encodeURIComponent`: every byte but `A–Z a–z 0–9 - _ . ! ~ * ' ( )` as `%XX`.
#[must_use]
pub fn encode_uri_component(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            // Writing to a String cannot fail.
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn numbers_read_as_javascript_number_does() {
        // Expected values are `Number(text)` of each text in Bun and Node.
        for (text, expected) in [
            ("100", 100.0),
            ("-5", -5.0),
            (" 12.5 ", 12.5),
            ("1e3", 1000.0),
            ("1E5", 100_000.0),
            ("1e+5", 100_000.0),
            ("1.e5", 100_000.0),
            (".5", 0.5),
            ("+.5", 0.5),
            ("5.", 5.0),
            ("00012", 12.0),
            ("   ", 0.0),
            ("\u{feff}7\u{feff}", 7.0),
            ("\u{2028} 8 \u{3000}", 8.0),
            ("0x10", 16.0),
            ("0b101", 5.0),
            ("0o17", 15.0),
            // Wider literals round once, to nearest and ties to even, as the spec and Node do
            // (`Number(text) === Number(BigInt(text))`). Bun rounds some of them twice:
            // 0xfffffffffffffbff is 18446744073709552000 there.
            ("0x20000000000001", 9_007_199_254_740_992.0),
            ("0x1fffffffffffff1", 144_115_188_075_855_860.0),
            ("0xfffffffffffffbff", 18_446_744_073_709_550_000.0),
            ("0x1FFFFFFFFFFFFF7FF", 36_893_488_147_419_100_000.0),
            // Past 128 bits a nonzero tail still breaks the tie upwards.
            (
                "0x20000000000001000000000000000000001",
                1.742_245_718_635_205_3e41,
            ),
            ("+Infinity", f64::INFINITY),
            ("-Infinity", f64::NEG_INFINITY),
            ("1e400", f64::INFINITY),
        ] {
            assert_eq!(
                string_to_number(text).to_bits(),
                expected.to_bits(),
                "{text:?}"
            );
        }
        for text in [
            "abc",
            "inf",
            "nan",
            "infinity",
            "1e",
            "0x",
            "-0b",
            "1.2.3",
            "--1",
            "+-1",
            "- 1",
            "1e1.5",
            ".",
            ".e1",
            "1_000",
            "-0x10",
            "+0x10",
            "0b2",
            "\u{85}5",
            "\u{180e}9",
        ] {
            assert!(string_to_number(text).is_nan(), "{text:?}");
        }
        let read = |value: Value| number(Some(&value));
        assert_eq!(read(json!(40)), Some(40.0));
        assert_eq!(read(json!("")), None);
        assert_eq!(read(json!("Infinity")), None);
        assert_eq!(read(json!(true)), None);
        assert_eq!(read(Value::Null), None);
    }

    #[test]
    fn number_strings_match_javascript() {
        // Expected values are Bun's `String(number)`: positional from 1e-6 up to 1e21.
        for (value, expected) in [
            (75.0, "75"),
            (39.5, "39.5"),
            (-2.0, "-2"),
            (-0.25, "-0.25"),
            (1e21, "1e+21"),
            (123_456_789_012_345_680_000.0, "123456789012345680000"),
            (0.000_001, "0.000001"),
            (1e-7, "1e-7"),
            (-1e-7, "-1e-7"),
            (1.5e-7, "1.5e-7"),
            (0.1 + 0.2, "0.30000000000000004"),
            (2.5e300, "2.5e+300"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
            (f64::NAN, "NaN"),
            (f64::NEG_INFINITY, "-Infinity"),
        ] {
            assert_eq!(number_string(value), expected);
        }
    }

    #[test]
    fn strings_and_json_match_javascript() {
        assert_eq!(string(None), "undefined");
        assert_eq!(
            string(Some(&json!([1, null, "a", [2, 3], true]))),
            "1,,a,2,3,true"
        );
        assert_eq!(string(Some(&json!({"a": 1}))), "[object Object]");
        assert_eq!(string(Some(&json!(2.5))), "2.5");
        assert_eq!(
            stringify(&json!({"b": 1.0, "a": [1e21, "x\u{1}"], "c": null})),
            r#"{"b":1,"a":[1e+21,"x\u0001"],"c":null}"#
        );
    }

    #[test]
    fn truthiness_and_or_match_javascript() {
        for falsy in [json!(null), json!(false), json!(0), json!(0.0), json!("")] {
            assert!(!truthy(Some(&falsy)), "{falsy}");
        }
        for truthy_value in [json!(true), json!(-1), json!("0"), json!([]), json!({})] {
            assert!(truthy(Some(&truthy_value)), "{truthy_value}");
        }
        assert!(!truthy(None));
        assert_eq!(or(Some(&json!("")), Some(&json!("x"))), Some(&json!("x")));
        assert_eq!(or(Some(&json!("a")), Some(&json!("x"))), Some(&json!("a")));
    }

    #[test]
    fn objects_keep_javascript_key_order() {
        let parsed = parse(r#"{"b":1,"2":2,"a":3,"10":4,"01":5}"#).unwrap();
        assert_eq!(parsed.to_string(), r#"{"2":2,"10":4,"b":1,"a":3,"01":5}"#);
    }

    #[test]
    fn whole_numbers_are_written_as_javascript_writes_them() {
        let parsed = parse(r#"{"a":1.0,"b":1e3,"c":0.5,"d":-0.0}"#).unwrap();
        assert_eq!(parsed.to_string(), r#"{"a":1,"b":1000,"c":0.5,"d":0}"#);
    }

    #[test]
    fn strings_escape_as_json_stringify_does() {
        assert_eq!(
            quote("a\"b\\c\n\u{1}\u{7f}\u{2028}é"),
            "\"a\\\"b\\\\c\\n\\u0001\u{7f}\u{2028}é\""
        );
    }

    #[test]
    fn lengths_count_utf16_code_units() {
        assert_eq!(utf16_len("Équipe"), 6);
        assert_eq!(utf16_len("★ ★ ★"), 5);
        assert_eq!(utf16_len("😀"), 2);
        assert_eq!(utf16_len(""), 0);
    }
}
