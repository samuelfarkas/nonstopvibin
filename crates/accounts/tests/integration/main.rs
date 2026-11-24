//! The accounts crate through its public API, in one test binary: the account features in
//! real time and the scheduler under a paused clock.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]

mod features;
mod restart;
mod scheduler;
