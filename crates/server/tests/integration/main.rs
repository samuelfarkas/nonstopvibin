//! The daemon through its HTTP API, in one test binary: the TypeScript server's goldens
//! replayed, the account features over fake cores, the desktop's routes and the lifecycle.
//! The panic hook is process-wide, so its test runs in a binary of its own.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]

mod common;
mod desktop;
mod features;
mod golden;
mod hub;
mod lifecycle;
