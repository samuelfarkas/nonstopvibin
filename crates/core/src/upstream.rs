//! Behaviour of the pinned core (`core-release.json`) that the app must match. Read from
//! `CLIProxyAPI` 8.1.0: re-read each fact here when the pin moves to another release.

use std::time::Duration;

/// How long the core (`conductor_cooldown.go`) keeps a key or model from requests after an
/// upstream status, at most. It keeps this only in memory, so a core that stops forgets it.
/// Other failures cool down for a minute, which no idle period outlasts.
#[must_use]
pub fn cooldown(status: u16) -> Option<Duration> {
    match status {
        401 | 402 | 403 | 429 => Some(Duration::from_mins(30)),
        404 => Some(Duration::from_hours(12)),
        _ => None,
    }
}

/// The loopback ports the core's browser sign-in links redirect to: Codex.
pub const SIGN_IN_PORTS: [u16; 1] = [1455];
