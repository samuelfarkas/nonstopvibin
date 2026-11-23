//! The pool and the scheduler on fake cores under a paused clock, and the account and usage
//! goldens.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]

mod common;
mod fake;
mod golden;
mod lazy;
mod lifecycle;
mod restart;
mod status;
