//! A block-style YAML emitter for JSON values. Every string, keys included, is double-quoted
//! with JSON-compatible escapes, so no scalar is read as another type. Characters a YAML
//! reader treats as line breaks or that are not printable are escaped as well: the core's Go
//! YAML library, like libyaml, breaks lines at U+0085, U+2028 and U+2029.

use std::fmt::Write;

use serde_json::{Map, Value};

/// `document` as YAML; a mapping at the top, as the core's config is.
pub(crate) fn to_yaml(document: &Map<String, Value>) -> String {
    let mut out = String::new();
    mapping(document, 0, false, &mut out);
    out
}

/// Entries at `indent`; with `inline`, the first one continues the current line (`- `).
fn mapping(map: &Map<String, Value>, indent: usize, inline: bool, out: &mut String) {
    for (index, (key, value)) in map.iter().enumerate() {
        if index > 0 || !inline {
            pad(indent, out);
        }
        quote(key, out);
        out.push(':');
        nested(value, indent, out);
    }
}

fn sequence(items: &[Value], indent: usize, out: &mut String) {
    for item in items {
        pad(indent, out);
        out.push('-');
        match item {
            Value::Object(map) if !map.is_empty() => {
                out.push(' ');
                mapping(map, indent + 2, true, out);
            }
            other => nested(other, indent, out),
        }
    }
}

/// A value after `key:` or `-`, whose own entries go two spaces deeper than `indent`.
fn nested(value: &Value, indent: usize, out: &mut String) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            out.push('\n');
            mapping(map, indent + 2, false, out);
        }
        Value::Array(items) if !items.is_empty() => {
            out.push('\n');
            sequence(items, indent + 2, out);
        }
        scalar => {
            out.push(' ');
            match scalar {
                Value::String(text) => quote(text, out),
                Value::Object(_) => out.push_str("{}"),
                Value::Array(_) => out.push_str("[]"),
                other => out.push_str(&other.to_string()),
            }
            out.push('\n');
        }
    }
}

fn pad(indent: usize, out: &mut String) {
    out.extend(std::iter::repeat_n(' ', indent));
}

fn quote(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0'..='\u{1f}'
            | '\u{7f}'..='\u{9f}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{feff}'
            | '\u{fffe}'
            | '\u{ffff}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn writes_block_style_with_every_string_quoted() {
        let document = json!({
            "host": "127.0.0.1",
            "port": 8317,
            "debug": false,
            "keys": ["a", "b"],
            "empty-list": [],
            "empty-map": {},
            "nested": { "inner": "x" },
            "entries": [{ "name": "n", "models": [{ "name": "m", "alias": "m" }], "headers": {} }],
        });
        let map = document.as_object().unwrap();
        assert_eq!(
            to_yaml(map),
            concat!(
                "\"debug\": false\n",
                "\"empty-list\": []\n",
                "\"empty-map\": {}\n",
                "\"entries\":\n",
                "  - \"headers\": {}\n",
                "    \"models\":\n",
                "      - \"alias\": \"m\"\n",
                "        \"name\": \"m\"\n",
                "    \"name\": \"n\"\n",
                "\"host\": \"127.0.0.1\"\n",
                "\"keys\":\n",
                "  - \"a\"\n",
                "  - \"b\"\n",
                "\"nested\":\n",
                "  \"inner\": \"x\"\n",
                "\"port\": 8317\n",
            )
        );
    }

    #[test]
    fn escapes_quotes_controls_and_yaml_line_breaks() {
        let mut out = String::new();
        quote(
            "a\"b\\c\nd\t\u{1}\u{7f}\u{85}\u{2028}\u{2029}\u{feff}é★😀: #&*!|>'%@`",
            &mut out,
        );
        assert_eq!(
            out,
            "\"a\\\"b\\\\c\\nd\\t\\u0001\\u007f\\u0085\\u2028\\u2029\\ufeffé★😀: #&*!|>'%@`\""
        );
        // The escapes are JSON's, so a JSON reader reads the text back.
        assert_eq!(
            serde_json::from_str::<String>(&out).unwrap(),
            "a\"b\\c\nd\t\u{1}\u{7f}\u{85}\u{2028}\u{2029}\u{feff}é★😀: #&*!|>'%@`"
        );
    }
}
