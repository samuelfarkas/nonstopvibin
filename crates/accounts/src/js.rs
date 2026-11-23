//! The JavaScript value rules `src/server/json.ts` and its callers relied on, for JSON the
//! core sends: truthiness, `String(value)`, `Number(text)` and `JSON.stringify`.

use std::sync::LazyLock;

use serde_json::{Map, Value};

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
    value.and_then(Value::as_str)
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

/// `value || fallback`, for JSON values.
pub(crate) fn or<'a>(value: Option<&'a Value>, fallback: Option<&'a Value>) -> Option<&'a Value> {
    if truthy(value) { value } else { fallback }
}

/// The value unless it is `null` or absent, for `value ?? fallback`.
pub(crate) fn defined(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

/// `number(value)` of json.ts: a finite number, or a non-empty string `Number()` reads as one.
pub(crate) fn number(value: Option<&Value>) -> Option<f64> {
    let parsed = match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) if !text.is_empty() => from_text(text),
        _ => None,
    }?;
    parsed.is_finite().then_some(parsed)
}

/// `Number(text)`: whitespace around a decimal, hexadecimal, octal or binary literal, and
/// empty text is 0. `None` is `NaN`; infinities are `Some`.
fn from_text(text: &str) -> Option<f64> {
    let trimmed = text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if trimmed.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = trimmed.strip_prefix(prefix) {
            if digits.is_empty() {
                return None;
            }
            return digits.chars().try_fold(0.0_f64, |total, digit| {
                Some(total * f64::from(radix) + f64::from(digit.to_digit(radix)?))
            });
        }
    }
    let unsigned = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if unsigned == "Infinity" {
        return Some(if trimmed.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    // Rust also reads `inf` and `nan`, which JavaScript does not: check the literal first.
    let (mantissa, exponent) = unsigned
        .split_once(['e', 'E'])
        .map_or((unsigned, None), |(mantissa, exponent)| {
            (mantissa, Some(exponent))
        });
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((whole, fraction)) => {
            (whole.is_empty() || digits(whole))
                && (fraction.is_empty() || digits(fraction))
                && !(whole.is_empty() && fraction.is_empty())
        }
        None => digits(mantissa),
    };
    let exponent_ok = exponent
        .is_none_or(|exponent| digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent)));
    if !(mantissa_ok && exponent_ok) {
        return None;
    }
    trimmed.parse().ok()
}

/// `String(value)` for a JSON value.
pub(crate) fn string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => number_text(number.as_f64().unwrap_or(f64::NAN)),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    string(item)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

/// `Number.prototype.toString()`: the shortest digits that read back as `value`, in plain
/// notation from 1e-7 up to 1e21 and in exponent notation outside.
fn number_text(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    if value == 0.0 {
        return "0".to_owned();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    // `{:e}` writes the shortest round-trip digits as `d.ddde±x`.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .unwrap_or((scientific.as_str(), "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exponent: i64 = exponent.parse().unwrap_or(0);
    let count = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    let point = exponent + 1;
    let body = if count <= point && point <= 21 {
        let zeros = usize::try_from(point - count).unwrap_or(0);
        format!("{digits}{}", "0".repeat(zeros))
    } else if 0 < point && point <= 21 {
        let (whole, fraction) = digits.split_at(usize::try_from(point).unwrap_or(0));
        format!("{whole}.{fraction}")
    } else if -6 < point && point <= 0 {
        let zeros = usize::try_from(-point).unwrap_or(0);
        format!("0.{}{digits}", "0".repeat(zeros))
    } else {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let sign = if point - 1 < 0 { '-' } else { '+' };
        format!("{first}{fraction}e{sign}{}", (point - 1).abs())
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
                out.push_str(&number_text(number));
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn numbers_read_from_text_as_javascript_number_reads_them() {
        // Expected values from Bun's Number(text).
        for (text, expected) in [
            ("100", Some(100.0)),
            ("-5", Some(-5.0)),
            ("1e2", Some(100.0)),
            (" 12 ", Some(12.0)),
            ("0x10", Some(16.0)),
            (".5", Some(0.5)),
            ("5.", Some(5.0)),
            ("+3", Some(3.0)),
            ("   ", Some(0.0)),
        ] {
            assert_eq!(from_text(text), expected, "{text}");
        }
        for text in [
            "abc", "inf", "nan", "1e", "0x", "1.2.3", "--1", "1_000", "-0x10",
        ] {
            assert_eq!(from_text(text), None, "{text}");
        }
        assert_eq!(number(Some(&json!("Infinity"))), None);
        assert_eq!(number(Some(&json!(""))), None);
        assert_eq!(number(Some(&json!(true))), None);
    }

    #[test]
    fn numbers_are_written_as_javascript_writes_them() {
        // Expected values from Bun's String(number).
        for (value, expected) in [
            (42.0, "42"),
            (1.5, "1.5"),
            (-0.25, "-0.25"),
            (1e21, "1e+21"),
            (123_456_789_012_345_680_000.0, "123456789012345680000"),
            (1e-7, "1e-7"),
            (1.5e-7, "1.5e-7"),
            (0.000_001, "0.000001"),
            (2.5e300, "2.5e+300"),
        ] {
            assert_eq!(number_text(value), expected, "{value}");
        }
    }

    #[test]
    fn values_convert_to_strings_and_json_as_javascript_converts_them() {
        assert_eq!(string(&json!([1, null, [2, "a"], true])), "1,,2,a,true");
        assert_eq!(string(&json!({"a": 1})), "[object Object]");
        assert_eq!(
            stringify(&json!({"b": 1.0, "a": [1e21, "x\u{1}"], "c": null})),
            r#"{"b":1,"a":[1e+21,"x\u0001"],"c":null}"#
        );
    }
}
