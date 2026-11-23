//! Pricing, the model catalog, quota, resets, imports and API accounts through their public
//! API, in one test binary: the TypeScript goldens and the behaviours its tests covered.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]

mod behaviour;
mod changes;
mod common;
mod golden_accounts;
mod golden_usage;
mod signin;
mod updates;
