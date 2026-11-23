//! Integration-test support shared by the workspace's crates. A dev-dependency only: no shipped
//! binary links it. The `core` feature adds the fake cores, `accounts` the account features'
//! world over them.
#![forbid(unsafe_code)]

mod directory;
#[cfg(feature = "core")]
pub mod fake;
mod http;
mod port;
#[cfg(feature = "accounts")]
pub mod world;

pub use directory::TempDir;
pub use http::dechunk;
pub use port::Port;
