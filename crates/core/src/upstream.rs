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

/// The Claude Code identity the core presents to Anthropic for subscription traffic
/// (`defaultClaudeFingerprintUserAgent` in `claude_device_profile.go`). Anthropic lists and
/// redeems Claude's limit resets only for Claude Code (`ineligible_reason: "surface"`
/// otherwise) of a recent enough version (`"cli_version"`), so the app's reset calls present
/// the same one.
pub const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.321 (external, cli)";

/// The loopback ports the core's browser sign-in links redirect to: Codex, Claude.
pub const SIGN_IN_PORTS: [u16; 2] = [1455, 54545];
