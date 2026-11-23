//! Integration-test support shared by the workspace's crates. A dev-dependency only: no shipped
//! binary links it.
#![forbid(unsafe_code)]

mod directory;
mod http;
mod port;

pub use directory::TempDir;
pub use http::dechunk;
pub use port::Port;
