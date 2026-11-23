//! Ids, wire types, the provider and agent registry, secret newtypes, JavaScript semantics, the
//! daemon's addresses and the window-helper protocol. No file, network or process I/O.
#![forbid(unsafe_code)]

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

pub mod endpoint;
pub mod error;
pub mod helper;
pub mod id;
pub mod js;
pub mod random;
pub mod registry;
pub mod secret;
pub mod sync;
pub mod time;
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

/// The SHA-256 of `bytes` in lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
