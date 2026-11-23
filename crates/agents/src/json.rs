//! JSON as `JSON.parse` and `JSON.stringify` see it, for files the user shares with us.

use serde_json::{Map, Number, Value};

/// `JSON.parse`: `None` for invalid JSON. Objects take JavaScript's key order (array-index keys
/// first, ascending, then the rest as written) and whole numbers below 2^53 become integers, so
/// a merged file is written back the way the TypeScript server wrote it.
pub(crate) fn parse(content: &str) -> Option<Value> {
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
        Value::Number(number) => Value::Number(whole(number)),
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

fn whole(number: Number) -> Number {
    // 2^53: past it, JavaScript no longer holds every integer.
    const SAFE: f64 = 9_007_199_254_740_992.0;
    match number.as_f64() {
        Some(value) if number.is_f64() && value.fract() == 0.0 && value.abs() < SAFE => {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a whole value below 2^53 converts exactly"
            )]
            let integer = value as i64;
            Number::from(integer)
        }
        _ => number,
    }
}

/// `JSON.stringify(value, null, 2)`.
pub(crate) fn pretty(value: &Value) -> String {
    format!("{value:#}")
}

/// `JSON.stringify(text)`: a JSON string literal, escaped as JavaScript escapes it.
pub(crate) fn string(text: &str) -> String {
    Value::from(text).to_string()
}

/// Whether JavaScript's `\s` and `trim()` count `c` as white space. Rust's `is_whitespace`
/// differs on U+0085 and U+FEFF.
pub(crate) fn is_space(c: char) -> bool {
    c == '\u{feff}' || (c != '\u{85}' && c.is_whitespace())
}

/// `String.prototype.trim`.
pub(crate) fn trim(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// An object from `(key, value)` pairs in order.
pub(crate) fn object<const N: usize>(fields: [(&str, Value); N]) -> Map<String, Value> {
    fields
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            string("a\"b\\c\n\u{1}\u{7f}\u{2028}é"),
            "\"a\\\"b\\\\c\\n\\u0001\u{7f}\u{2028}é\""
        );
    }
}
