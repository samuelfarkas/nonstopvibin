//! The data directory, its lock and the owner-only file checks through the public API, in one
//! test binary.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]

mod data_dir;
mod files;
mod lock;
mod path;
