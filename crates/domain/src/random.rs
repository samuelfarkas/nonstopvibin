//! Random identifiers and keys. Everything secret or persistent draws from the operating
//! system's random source.

use std::hash::{BuildHasher, RandomState};
use std::io;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use secrecy::SecretString;
use secrecy::zeroize::Zeroizing;

/// `N` bytes from the operating system's random source.
///
/// # Errors
/// The random source failed.
pub fn bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes)?;
    Ok(bytes)
}

/// A version 4 UUID as `crypto.randomUUID()` writes it: lowercase and hyphenated.
///
/// # Errors
/// The random source failed.
pub fn uuid() -> io::Result<String> {
    let mut bytes = bytes::<16>()?;
    let [.., version, _, variant, _, _, _, _, _, _, _] = &mut bytes;
    *version = *version & 0x0f | 0x40;
    *variant = *variant & 0x3f | 0x80;
    let mut text = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        for nibble in [byte >> 4, byte & 0x0f] {
            text.extend(char::from_digit(u32::from(nibble), 16));
        }
    }
    Ok(text)
}

/// A version 4 UUID for a request id. It needs to be unique, not secret: `RandomState` keys
/// `SipHash` with OS randomness once per thread and steps the key on every call.
#[must_use]
pub fn request_uuid() -> String {
    let state = RandomState::new();
    let high = (state.hash_one(0_u8) & !0xF000) | 0x4000;
    let low = (state.hash_one(1_u8) & !(0b11 << 62)) | (0b10 << 62);
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        high >> 32,
        (high >> 16) & 0xFFFF,
        high & 0xFFFF,
        low >> 48,
        low & 0xFFFF_FFFF_FFFF
    )
}

/// `prefix` and 32 random bytes in base64url (43 characters), never copied outside a
/// zeroizing buffer.
///
/// # Errors
/// The random source failed.
pub(crate) fn key(prefix: &str) -> io::Result<SecretString> {
    let bytes = Zeroizing::new(bytes::<32>()?);
    let mut key = String::with_capacity(prefix.len() + 43);
    key.push_str(prefix);
    URL_SAFE_NO_PAD.encode_string(bytes.as_slice(), &mut key);
    Ok(SecretString::from(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::is_uuid;

    #[test]
    fn uuids_are_version_4_and_distinct() {
        for generate in [|| uuid().unwrap(), request_uuid] {
            let first = generate();
            assert!(is_uuid(&first), "{first}");
            assert_eq!(first.as_bytes()[14], b'4');
            assert!(matches!(first.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
            assert_ne!(first, generate());
        }
    }
}
