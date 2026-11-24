//! Integration-test support shared by the workspace's crates. A dev-dependency only: no shipped
//! binary links it. The `core` feature adds the fake cores and a store under the paused clock,
//! `accounts` the account features' world over them, and `shell` a host for the shell.
#![forbid(unsafe_code)]

mod directory;
#[cfg(feature = "core")]
pub mod fake;
mod http;
#[cfg(feature = "core")]
pub mod paused;
mod port;
#[cfg(feature = "shell")]
pub mod shell;
#[cfg(feature = "accounts")]
pub mod world;

pub use directory::{TempDir, copy_executable, mode, set_mode, write_executable};
pub use http::dechunk;
pub use port::Port;
