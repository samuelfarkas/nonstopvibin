//! What every agent connection shares, and the one definition per agent that `AgentSetup` runs
//! the same flow with.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use nsv_domain::registry::Agent;
use nsv_domain::wire::{AgentSetupInput, Profile};

use crate::error::{AgentError, conflict};
use crate::manifest::Manifest;
use crate::{claude, codex, opencode, pi};

/// [`PROVIDER_PREFIX`] as a literal, which `concat!` splices into pi's controls.
macro_rules! provider_prefix {
    () => {
        "nonstopvibin-"
    };
}
pub(crate) use provider_prefix;

/// Starts every provider name a connected agent lists; pi's controls recognise ours by it.
pub(crate) const PROVIDER_PREFIX: &str = provider_prefix!();

/// The provider a connected agent lists for a profile.
#[must_use]
pub fn native_provider(profile: &Profile) -> String {
    format!("{PROVIDER_PREFIX}{}", profile.slug)
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A path as the generated text names it.
pub(crate) fn text(path: &Path) -> Cow<'_, str> {
    path.to_string_lossy()
}

pub(crate) struct Context<'a> {
    pub profile: &'a Profile,
    pub input: &'a AgentSetupInput,
    pub port: u16,
    pub helper: &'a Path,
    /// The credential socket, for `OpenCode`'s plugin, which asks it directly.
    pub socket: &'a Path,
    /// Where pi's extensions remember each repository's profile.
    pub pi_preferences: &'a Path,
}

/// Where an agent reads its configuration: `$variable`, `$XDG_CONFIG_HOME/<xdg>`, or `~/<home>`.
pub(crate) struct ConfigDirectory {
    pub variable: &'static str,
    pub xdg: Option<&'static str>,
    pub home: &'static [&'static str],
}

type Merge = fn(Option<&str>, Option<&str>, Option<&str>) -> Result<Option<String>, AgentError>;

type ProjectRoot = fn(&Path) -> Result<PathBuf, AgentError>;
type Retain = fn(Option<&str>, &str) -> Result<Option<String>, AgentError>;
type LaunchSettings = fn(&Manifest) -> Result<Option<String>, AgentError>;

pub(crate) struct Definition {
    /// Manifest version a connection made now records; older ones are migrated.
    pub version: u8,
    /// Whether a connection from an older version asks the user to reconnect even though
    /// startup migrates its files: the agent must reload or launch differently.
    pub older_needs_reconnect: bool,
    pub directory: Option<ConfigDirectory>,
    /// Agents connected per project: the project root the configuration belongs to.
    pub project_root: Option<ProjectRoot>,
    /// The configuration file, under the agent's directory or the project, by manifest version.
    pub file: fn(&Profile, u8) -> Vec<String>,
    /// Whether the connection installs a credential helper that asks the socket.
    pub helper: bool,
    pub configure: fn(&Context<'_>) -> String,
    /// The configuration with `previous` (our last fields) replaced by `next`; `None` when
    /// nothing is left. Only what this app installed is replaced or removed; a collision fails,
    /// so existing credentials need no plaintext backup.
    pub merge: Merge,
    /// Whether the installed configuration still equals what merging our fields gives.
    pub unchanged: fn(&str, Option<&str>) -> Result<bool, AgentError>,
    /// Our settings already in the user's file before connecting, restored on disconnect.
    pub retain: Option<Retain>,
    /// A settings file the launch command passes, derived from the manifest.
    pub launch_settings: Option<LaunchSettings>,
    pub command: fn(&Profile, &str) -> String,
    pub instructions: fn(&Profile) -> String,
}

pub(crate) const fn definition(agent: Agent) -> &'static Definition {
    match agent {
        Agent::Claude => &claude::DEFINITION,
        Agent::Codex => &codex::DEFINITION,
        Agent::Pi => &pi::DEFINITION,
        Agent::Opencode => &opencode::DEFINITION,
    }
}

/// Moves the configuration of agents whose [`ConfigDirectory`] names an `xdg` directory.
pub(crate) const XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";

/// The environment variables that move agent configuration, which a service must forward: each
/// agent's own, then [`XDG_CONFIG_HOME`] if an agent follows it.
#[must_use]
pub fn agent_environment() -> Vec<&'static str> {
    let directories = Agent::ALL
        .iter()
        .filter_map(|&agent| definition(agent).directory.as_ref());
    let mut names: Vec<&str> = directories
        .clone()
        .map(|directory| directory.variable)
        .collect();
    if directories.clone().any(|directory| directory.xdg.is_some()) {
        names.push(XDG_CONFIG_HOME);
    }
    names
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "the signature of `Definition::unchanged`"
)]
pub(crate) fn same_text(current: &str, expected: Option<&str>) -> Result<bool, AgentError> {
    Ok(Some(current) == expected)
}

/// The rule for a file that is wholly ours: any other content is a conflict.
pub(crate) fn owned_file(
    current: Option<&str>,
    previous: Option<&str>,
    next: Option<&str>,
) -> Result<Option<String>, AgentError> {
    if current.is_some() && current != previous && current != next {
        return Err(conflict());
    }
    Ok(next.map(str::to_owned))
}
