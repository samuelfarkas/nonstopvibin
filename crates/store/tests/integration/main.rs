//! The store's behaviour through its public API, in one test binary.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]

mod common;
mod files;
mod layout;
mod lock;
mod migration;
mod store;
