//! The core crate's behaviour through its public API, in one test binary. Process tests that
//! need a fake core live in `tests/processes.rs`.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]

mod binary;
mod common;
mod config;
mod install;
mod restart;
