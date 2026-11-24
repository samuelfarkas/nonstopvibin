//! The window helper's tests, in one binary: the helper under an X server, the UI checks, the
//! desktop app's error boxes and signals, and the measurements.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]
#![cfg(any(target_os = "linux", target_os = "macos"))]

#[cfg(target_os = "linux")]
mod app;
#[cfg_attr(
    target_os = "macos",
    expect(
        dead_code,
        reason = "only the UI checks run on macOS; the rest is the X server tests'"
    )
)]
mod common;
#[cfg(target_os = "linux")]
mod helper;
#[cfg(target_os = "linux")]
mod measure;
mod ui;
