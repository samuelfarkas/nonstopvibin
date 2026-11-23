//! JSON objects built field by field, in the order JavaScript would write them.

use serde_json::{Map, Value};

/// An object from `(key, value)` pairs in order.
pub(crate) fn object<const N: usize>(fields: [(&str, Value); N]) -> Map<String, Value> {
    fields
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}
