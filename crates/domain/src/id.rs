//! Identifiers. Cheap to clone (`Arc<str>`), validated where they enter from outside.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// An identifier that failed validation at a boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind}")]
pub struct InvalidId {
    pub kind: &'static str,
}

macro_rules! id {
    ($(#[$meta:meta])* $name:ident, $kind:literal, $valid:expr) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        #[cfg_attr(test, derive(ts_rs::TS), ts(type = "string"))]
        pub struct $name(Arc<str>);

        impl $name {
            /// # Errors
            /// When `value` does not have the identifier's shape.
            pub fn parse(value: &str) -> Result<Self, InvalidId> {
                let valid: fn(&str) -> bool = $valid;
                if valid(value) {
                    Ok(Self(Arc::from(value)))
                } else {
                    Err(InvalidId { kind: $kind })
                }
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidId;
            fn try_from(value: String) -> Result<Self, InvalidId> {
                Self::parse(&value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0.as_ref().to_owned()
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
    };
}

id!(
    /// A profile's UUID, as `randomUUID()` wrote it (lowercase, hyphenated).
    ProfileId,
    "profile id",
    is_uuid
);
id!(
    /// A profile's URL slug: `[a-z0-9-]+`, unique among profiles.
    ProfileSlug,
    "profile slug",
    |value| !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
);
id!(
    /// An account: a core credential file name, or an API-key account's id. At most 255 bytes.
    AccountId,
    "account id",
    |value| !value.is_empty() && value.len() <= 255
);

/// Whether `value` is a UUID as zod 4's `.uuid()` accepts it: 8-4-4-4-12 hexadecimal with an
/// RFC 9562 version (1–8) and variant (8, 9, a, b), or the nil or max UUID.
#[must_use]
pub fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    let shaped = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, &byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        });
    shaped
        && (value == "00000000-0000-0000-0000-000000000000"
            || value.eq_ignore_ascii_case("ffffffff-ffff-ffff-ffff-ffffffffffff")
            || (matches!(bytes.get(14), Some(b'1'..=b'8'))
                && matches!(bytes.get(19), Some(b'8' | b'9' | b'a' | b'b' | b'A' | b'B'))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_ids_are_uuids() {
        assert!(ProfileId::parse("6e4156ab-2acf-4d89-9a81-264f3b3da50a").is_ok());
        assert!(ProfileId::parse("6E4156AB-2ACF-4D89-9A81-264F3B3DA50A").is_ok());
        assert!(ProfileId::parse("6e4156ab2acf4d899a81264f3b3da50a").is_err());
        // zod 4 checks the version and variant nibbles.
        assert!(ProfileId::parse("6e4156ab-2acf-0d89-9a81-264f3b3da50a").is_err());
        assert!(ProfileId::parse("6e4156ab-2acf-4d89-7a81-264f3b3da50a").is_err());
        assert!(ProfileId::parse("00000000-0000-0000-0000-000000000000").is_ok());
        assert!(ProfileId::parse("../etc/passwd").is_err());
        assert!(ProfileId::parse("").is_err());
    }

    #[test]
    fn slugs_are_lowercase_words() {
        assert!(ProfileSlug::parse("equipe-cafe-r-d-ete").is_ok());
        assert!(ProfileSlug::parse("Personal").is_err());
        assert!(ProfileSlug::parse("a/b").is_err());
        assert!(ProfileSlug::parse("").is_err());
    }

    #[test]
    fn ids_round_trip_through_json() {
        let id = ProfileId::parse("6e4156ab-2acf-4d89-9a81-264f3b3da50a").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"6e4156ab-2acf-4d89-9a81-264f3b3da50a\"");
        assert_eq!(serde_json::from_str::<ProfileId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<ProfileId>("\"nope\"").is_err());
    }
}
