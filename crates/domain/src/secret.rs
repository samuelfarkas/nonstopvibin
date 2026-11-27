//! Secrets as distinct types. None of them implements `Serialize` or shows its value in
//! `Debug`, so a secret reaches a log, a response or another crate's API only through an
//! explicit `expose_secret()` that a reviewer can grep for.

use std::{fmt, io};

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::random;

/// Compares two secrets in constant time for equal lengths; differing lengths are unequal.
#[must_use]
pub fn same_secret(left: &str, right: &str) -> bool {
    left.len() == right.len() && bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

macro_rules! secret {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        pub struct $name(SecretString);

        impl $name {
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(SecretString::from(value.into()))
            }

            /// The plaintext. Every call site is a place the secret leaves this type.
            #[must_use]
            pub fn expose_secret(&self) -> &str {
                self.0.expose_secret()
            }

            /// Whether `presented` is this secret, compared in constant time.
            #[must_use]
            pub fn matches(&self, presented: &str) -> bool {
                same_secret(self.expose_secret(), presented)
            }
        }

        impl Clone for $name {
            fn clone(&self) -> Self {
                Self::new(self.expose_secret())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($name), "(<redacted>)"))
            }
        }

        /// A secret may arrive in a request body; it never leaves through serde.
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                String::deserialize(deserializer).map(Self::new)
            }
        }
    };
}

secret!(
    /// The key an agent sends for a profile. It never grants management access.
    ClientKey
);
secret!(
    /// The key the gateway sends to a profile's core.
    CoreKey
);
secret!(
    /// The secret of a core's management API. Only the core's management client takes it.
    ManagementKey
);
secret!(
    /// The bearer token of the management API, new for every daemon process.
    SessionToken
);
secret!(
    /// An upstream provider API key of an API-key account.
    ApiKey
);
secret!(
    /// The key a usage hub client such as T3 Code sends for one profile. It reads that
    /// profile's limits and never reaches management or the agent API.
    HubKey
);

/// Secrets this application issues: a prefix and 32 random bytes in base64url.
macro_rules! generated {
    ($($name:ident = $prefix:expr),*) => {$(
        impl $name {
            /// # Errors
            /// The random source failed.
            pub fn generate() -> io::Result<Self> {
                random::key($prefix).map(Self)
            }
        }
    )*};
}
generated!(
    ClientKey = CLIENT_KEY_PREFIX,
    CoreKey = "",
    ManagementKey = "",
    SessionToken = "",
    HubKey = "nvh_"
);

/// The client-key alphabet and length that installed credential helpers already match.
pub const CLIENT_KEY_PREFIX: &str = "nv_";
pub const CLIENT_KEY_LENGTH: usize = 43;
/// The base64url alphabet as a character class, in the order installed credential helpers
/// already contain.
pub const CLIENT_KEY_CHARACTERS: &str = "a-zA-Z0-9_-";

impl ClientKey {
    /// Whether `value` has a client key's shape: `nv_` and 43 base64url characters.
    #[must_use]
    pub fn well_formed(value: &str) -> bool {
        value.strip_prefix(CLIENT_KEY_PREFIX).is_some_and(|rest| {
            rest.len() == CLIENT_KEY_LENGTH
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
    }

    /// The one-way index key the gateway looks a presented key up by.
    #[must_use]
    pub fn digest(&self) -> KeyDigest {
        KeyDigest::of(self.expose_secret())
    }
}

/// SHA-256 of a client key: the auth index never holds the key itself.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyDigest([u8; 32]);

impl KeyDigest {
    #[must_use]
    pub fn of(key: &str) -> Self {
        Self(Sha256::digest(key.as_bytes()).into())
    }
}

impl fmt::Debug for KeyDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyDigest(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_value() {
        let key = ClientKey::new("nv_visible");
        assert_eq!(format!("{key:?}"), "ClientKey(<redacted>)");
        assert_eq!(format!("{:?}", key.digest()), "KeyDigest(<redacted>)");
    }

    #[test]
    fn matches_only_the_same_secret() {
        let key = CoreKey::new("abc");
        assert!(key.matches("abc"));
        assert!(!key.matches("abd"));
        assert!(!key.matches("ab"));
        assert!(!key.matches(""));
    }

    #[test]
    fn client_key_shape() {
        let good = format!("nv_{}", "a".repeat(43));
        assert!(ClientKey::well_formed(&good));
        assert!(!ClientKey::well_formed(&format!("nv_{}", "a".repeat(42))));
        assert!(!ClientKey::well_formed(&format!("nv_{}=", "a".repeat(42))));
        assert!(!ClientKey::well_formed(&format!("xx_{}", "a".repeat(43))));
        let generated = ClientKey::generate().unwrap();
        assert!(ClientKey::well_formed(generated.expose_secret()));
        assert!(!generated.matches(ClientKey::generate().unwrap().expose_secret()));
    }

    #[test]
    fn digest_is_sha256_of_the_key() {
        // sha256("abc"), the FIPS 180-2 example.
        let expected = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(KeyDigest::of("abc"), KeyDigest(expected));
    }
}
