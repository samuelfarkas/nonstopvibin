//! Agent setup through its public API, in one test binary: the TypeScript goldens, the data
//! directory main wrote, and the behaviours its tests covered beyond the goldens.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]

mod common;
mod fixture;
mod golden;
mod setup;
mod socket;
