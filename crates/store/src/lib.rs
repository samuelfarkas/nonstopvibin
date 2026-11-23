//! The data directory: layout, owner-only file checks, atomic writes, the vault and the SQLite store.
#![forbid(unsafe_code)]

mod db;
pub mod files;
pub mod layout;
mod lock;
mod store;
mod vault;

use std::io;

pub use db::{AuthEntry, AuthSnapshot, Db, ProfileNames, UsageCursor, UsagePage, UsageRange};
pub use lock::{LockError, ServerLock};
pub use store::{Store, StoreError};
pub use vault::{Vault, VaultError};

fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes)?;
    Ok(bytes)
}

/// A version 4 UUID as `crypto.randomUUID()` writes it: lowercase and hyphenated.
///
/// # Errors
/// The system's random source failed.
pub fn random_uuid() -> io::Result<String> {
    let mut bytes = random_bytes::<16>()?;
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
