use serde::{Deserialize, Serialize};

use crate::registry::Agent;

/// `POST /profiles/:profileId/agent-setup`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct AgentSetupInput {
    pub agent: Agent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_directory: Option<String>,
    /// Claude Code and Codex: also offer models from other providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_providers: Option<bool>,
}

/// Which connection of an agent: Claude Code also takes a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct AgentSelection {
    pub agent: Agent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_directory: Option<String>,
}

/// An agent connection: the input it was made with and what it wrote. Its fields are
/// [`AgentSetupInput`]'s and more; they are spelled out because that input rejects unknown
/// fields, which `#[serde(flatten)]` cannot combine with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct AgentSetup {
    pub agent: Agent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_providers: Option<bool>,
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_reconnect: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projects: Option<Vec<String>>,
    pub command: String,
    pub files: Vec<String>,
    pub provider: String,
    pub instructions: String,
}
