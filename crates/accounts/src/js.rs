//! JSON from the core and providers read the way `src/server/json.ts` and JavaScript read it:
//! truthiness and `||`, `??` skipping `null` and absent alike, counters that may be numeric
//! strings (`Number(text)`), labels as `String(x)`, `JSON.stringify`, and dates as
//! `new Date(x)`. Upstream bodies are untrusted and loosely shaped, so every reader answers
//! `None` instead of failing.

use std::borrow::Cow;
use std::sync::LazyLock;

use serde_json::{Map, Value};

/// `record(value)[key]`: a field of an object; `None` for a non-object or a missing key.
pub(crate) fn field<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    value?.as_object()?.get(key)
}

/// `a ?? b ?? …` over fields of `value`: the first that is present and not `null`.
pub(crate) fn first<'a>(value: Option<&'a Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .find_map(|key| field(value, key).filter(|found| !found.is_null()))
}

/// `value ?? fallback` for one value.
pub(crate) fn nullish(value: Option<&Value>) -> Option<&Value> {
    value.filter(|found| !found.is_null())
}

/// `Object.keys(record(value)).length > 0`.
pub(crate) fn non_empty_record(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_object)
        .is_some_and(|object| !object.is_empty())
}

/// `record(value)`: the object, or an empty one.
pub(crate) fn record(value: Option<&Value>) -> &Map<String, Value> {
    static EMPTY: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
    match value {
        Some(Value::Object(map)) => map,
        _ => &EMPTY,
    }
}

/// `text(value)`: a string, or nothing.
pub(crate) fn text(value: Option<&Value>) -> Option<&str> {
    value?.as_str()
}

/// Whether JavaScript treats `value` as true; an absent value is `undefined`.
pub(crate) fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `value || fallback`.
pub(crate) fn or<'a>(value: Option<&'a Value>, fallback: Option<&'a Value>) -> Option<&'a Value> {
    if truthy(value) { value } else { fallback }
}

/// `number(value)`: a finite number from a JSON number or a non-empty numeric string.
pub(crate) fn number(value: Option<&Value>) -> Option<f64> {
    let parsed = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) if !text.is_empty() => string_to_number(text),
        _ => return None,
    };
    parsed.is_finite().then_some(parsed)
}

/// `Number(text)`: trimmed decimal, `0x`/`0o`/`0b` integers, `Infinity`; blank is 0, anything
/// else `NaN`.
fn string_to_number(text: &str) -> f64 {
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
pub(crate) fn unwrap(value: &Value) -> Result<Cow<'_, Value>, serde_json::Error> {
    match value {
        Value::String(text) => serde_json::from_str(text).map(Cow::Owned),
        other => Ok(Cow::Borrowed(other)),
    }
}

/// `String(value)`, with `None` for `undefined`.
pub(crate) fn string(value: Option<&Value>) -> Cow<'_, str> {
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
pub(crate) fn number_string(value: f64) -> String {
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
pub(crate) fn stringify(value: &Value) -> String {
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

/// `String.prototype.trim()`. Rust's `trim` differs on U+0085 and U+FEFF.
pub(crate) fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c == '\u{feff}' || (c != '\u{85}' && c.is_whitespace()))
}

/// The most milliseconds from the epoch a JavaScript `Date` holds (`TimeClip`).
const MAX_TIME: f64 = 8.64e15;

/// `new Date(milliseconds).getTime()`: truncated, or `None` past `TimeClip`'s range.
pub(crate) fn time_clip(milliseconds: f64) -> Option<i64> {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the value is whole and within ±8.64e15"
    )]
    (milliseconds.abs() <= MAX_TIME).then(|| milliseconds.trunc() as i64)
}

/// `Date.parse(text)` for the ECMAScript date-time string format: `YYYY[-MM[-DD]]` or
/// `±YYYYYY…`, then `THH:mm[:ss[.fff…]]` and `Z` or `±HH:mm`. V8's other, implementation-defined
/// formats are not read. A date-time without an offset is read as UTC (V8: local time).
pub(crate) fn parse_date(text: &str) -> Option<i64> {
    let mut cursor = Cursor(text.as_bytes());
    let year = match cursor.peek() {
        Some(sign @ (b'+' | b'-')) => {
            cursor.skip();
            let digits = cursor.digits(6)?;
            if sign == b'-' && digits == 0 {
                return None;
            }
            if sign == b'-' { -digits } else { digits }
        }
        _ => cursor.digits(4)?,
    };
    let (month, day) = if cursor.eat(b'-') {
        let month = cursor.digits(2)?;
        (
            month,
            if cursor.eat(b'-') {
                cursor.digits(2)?
            } else {
                1
            },
        )
    } else {
        (1, 1)
    };
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let mut milliseconds = 0;
    let mut offset = 0;
    if cursor.eat(b'T') {
        let hour = cursor.digits(2)?;
        if !cursor.eat(b':') {
            return None;
        }
        let minute = cursor.digits(2)?;
        let mut second = 0;
        let mut fraction = 0;
        if cursor.eat(b':') {
            second = cursor.digits(2)?;
            if cursor.eat(b'.') {
                fraction = cursor.fraction()?;
            }
        }
        let midnight = hour == 24 && minute == 0 && second == 0 && fraction == 0;
        if !(hour < 24 || midnight) || minute > 59 || second > 59 {
            return None;
        }
        milliseconds = ((hour * 60 + minute) * 60 + second) * 1000 + fraction;
        match cursor.peek() {
            Some(b'Z') => cursor.skip(),
            Some(sign @ (b'+' | b'-')) => {
                cursor.skip();
                let hours = cursor.digits(2)?;
                if !cursor.eat(b':') {
                    return None;
                }
                let minutes = cursor.digits(2)?;
                if hours > 23 || minutes > 59 {
                    return None;
                }
                offset = (hours * 60 + minutes) * 60_000;
                if sign == b'-' {
                    offset = -offset;
                }
            }
            _ => {}
        }
    }
    if !cursor.0.is_empty() {
        return None;
    }
    let days = days_from_civil(year, month, day);
    #[expect(
        clippy::cast_precision_loss,
        reason = "years of six digits keep the time far below 2^53"
    )]
    time_clip((days * 86_400_000 + milliseconds - offset) as f64)
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.0.first().copied()
    }

    fn skip(&mut self) {
        self.0 = self.0.get(1..).unwrap_or_default();
    }

    fn eat(&mut self, byte: u8) -> bool {
        let matched = self.peek() == Some(byte);
        if matched {
            self.skip();
        }
        matched
    }

    /// Exactly `count` ASCII digits.
    fn digits(&mut self, count: usize) -> Option<i64> {
        let digits = self.0.get(..count)?;
        let mut value = 0;
        for &byte in digits {
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value * 10 + i64::from(byte - b'0');
        }
        self.0 = self.0.get(count..).unwrap_or_default();
        Some(value)
    }

    /// One or more digits of a fraction, as milliseconds (the first three, truncated).
    fn fraction(&mut self) -> Option<i64> {
        let length = self.0.iter().take_while(|b| b.is_ascii_digit()).count();
        if length == 0 {
            return None;
        }
        let mut milliseconds = 0;
        for index in 0..3 {
            let digit = self
                .0
                .get(index)
                .filter(|_| index < length)
                .map_or(0, |b| b - b'0');
            milliseconds = milliseconds * 10 + i64::from(digit);
        }
        self.0 = self.0.get(length..).unwrap_or_default();
        Some(milliseconds)
    }
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// zod 4's `z.iso.date()`: `YYYY-MM-DD` with a real day, leap years included.
pub(crate) fn is_iso_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10 && parse_date(text).is_some() && bytes.get(4) == Some(&b'-')
}

/// zod 4's `z.string().datetime({ offset: true })`: an ISO date, `THH:mm:ss`, an optional
/// fraction, then `Z` or `±HH:mm`.
pub(crate) fn is_offset_datetime(text: &str) -> bool {
    let Some((date, time)) = text.split_once('T') else {
        return false;
    };
    let (clock, zone) = match time.find(['Z', '+', '-']) {
        Some(index) => time.split_at(index),
        None => return false,
    };
    let zone_ok = zone == "Z" || {
        let zone = zone.as_bytes();
        zone.len() == 6
            && zone.get(3) == Some(&b':')
            && two_digits(zone.get(1..3), 23)
            && two_digits(zone.get(4..6), 59)
    };
    let (seconds, fraction) = clock.split_at(clock.find('.').unwrap_or(clock.len()));
    let fraction_ok = fraction.is_empty()
        || (fraction.len() > 1 && fraction.bytes().skip(1).all(|b| b.is_ascii_digit()));
    let seconds = seconds.as_bytes();
    is_iso_date(date)
        && zone_ok
        && fraction_ok
        && seconds.len() == 8
        && seconds.get(2) == Some(&b':')
        && seconds.get(5) == Some(&b':')
        && two_digits(seconds.get(..2), 23)
        && two_digits(seconds.get(3..5), 59)
        && two_digits(seconds.get(6..8), 59)
}

fn two_digits(bytes: Option<&[u8]>, most: u8) -> bool {
    match bytes {
        Some(&[tens, ones]) if tens.is_ascii_digit() && ones.is_ascii_digit() => {
            (tens - b'0') * 10 + (ones - b'0') <= most
        }
        _ => false,
    }
}

/// A string's length in UTF-16 code units, as zod's `.min()` and `.max()` count it.
pub(crate) fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
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
    fn dates_parse_the_ecmascript_format() {
        let iso = |text: &str| parse_date(text).map(nsv_domain::time::iso_string);
        assert_eq!(
            iso("2026-11-27T00:00:00+02:00").as_deref(),
            Some("2026-11-26T22:00:00.000Z")
        );
        assert_eq!(
            iso("2026-12-01").as_deref(),
            Some("2026-12-01T00:00:00.000Z")
        );
        assert_eq!(iso("2026").as_deref(), Some("2026-01-01T00:00:00.000Z"));
        assert_eq!(
            iso("2026-11-22T14:00:00.1239Z").as_deref(),
            Some("2026-11-22T14:00:00.123Z")
        );
        assert_eq!(
            iso("+275760-09-13T00:00:00Z").as_deref(),
            Some("+275760-09-13T00:00:00.000Z")
        );
        for invalid in [
            "not a date",
            "bad",
            "2026-02-30",
            "2026-11-22T25:00Z",
            "+275760-09-13T00:00:00.001Z",
            "-000000-01-01",
            "2026-11-22 14:00",
        ] {
            assert_eq!(parse_date(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn zod_date_formats() {
        assert!(is_iso_date("2024-02-29"));
        assert!(!is_iso_date("2026-02-29"));
        assert!(!is_iso_date("2026-1-01"));
        assert!(is_offset_datetime("2026-12-01T00:00:00+00:00"));
        assert!(is_offset_datetime("2026-12-01T00:00:00.5Z"));
        assert!(!is_offset_datetime("2026-12-01T00:00:00"));
        assert!(!is_offset_datetime("2026-12-01T00:00Z"));
        assert!(!is_offset_datetime("invalid"));
    }
}
