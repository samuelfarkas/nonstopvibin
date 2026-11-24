//! The shell on Linux, in one test binary: the supervisor over a real pipe, and the desktop end
//! to end, which tests/gui.sh runs.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]
#![cfg(target_os = "linux")]

mod common;
mod desktop;
mod supervisor;
