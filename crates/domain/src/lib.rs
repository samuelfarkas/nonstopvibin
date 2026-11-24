//! Ids, wire types, the provider and agent registry, secret newtypes, JavaScript semantics and
//! the daemon's addresses. No file, network or process I/O.
#![forbid(unsafe_code)]

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

pub mod endpoint;
pub mod error;
pub mod id;
pub mod js;
pub mod random;
pub mod registry;
pub mod secret;
pub mod sync;
pub mod time;
#[cfg(test)]
mod typescript;
pub mod version;
pub mod wire;

/// Lowercase hex of `bytes`, as Node's `digest("hex")` writes it.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut hex, byte| {
            let _infallible = write!(hex, "{byte:02x}");
            hex
        })
}

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The SHA-256 that [`sha256_hex`] writes: `None` unless `text` is 64 lowercase hex digits.
#[must_use]
pub fn sha256_from_hex(text: &str) -> Option<[u8; 32]> {
    const fn digit(digit: u8) -> Option<u8> {
        match digit {
            b'0'..=b'9' => Some(digit - b'0'),
            b'a'..=b'f' => Some(digit - b'a' + 10),
            _ => None,
        }
    }
    let (pairs, []) = text.as_bytes().as_chunks::<2>() else {
        return None;
    };
    let mut bytes = [0; 32];
    if pairs.len() != bytes.len() {
        return None;
    }
    for (byte, [high, low]) in bytes.iter_mut().zip(pairs) {
        *byte = digit(*high)? << 4 | digit(*low)?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sha256_reads_back_only_as_written() {
        let written = sha256_hex(b"core");
        assert_eq!(
            sha256_from_hex(&written).map(|bytes| hex(&bytes)),
            Some(written.clone())
        );
        let rest = written.strip_prefix(|_| true).unwrap();
        for refused in [
            written.to_uppercase(),
            rest.to_owned(),
            format!("{written}0"),
            format!("+{rest}"),
            String::new(),
        ] {
            assert_eq!(sha256_from_hex(&refused), None, "{refused}");
        }
    }
}
