//! Agent setup writers for Claude Code, Codex, `OpenCode` and pi, and the credential socket.
//!
//! Users already have connections the TypeScript server installed: the file names, manifests,
//! helper scripts and generated code stay byte-for-byte what it wrote, so this crate recognises,
//! updates and removes them as it did.
#![forbid(unsafe_code)]

mod catalog;
mod claude;
mod codex;
mod connection;
mod credential;
mod error;
mod files;
mod json;
mod manifest;
mod opencode;
mod pi;
mod pi_controls;
mod setup;
mod socket;

pub use catalog::{CatalogAnswer, connection_models, curated, gateway_catalog};
pub use connection::{agent_environment, native_provider};
pub use error::AgentError;
pub use manifest::{
    Issue, PROJECT_LENGTH, project_issues, selection_issues, setup_issues, validate_setup,
};
pub use setup::{AgentDirectories, AgentSetup, Connection, ConnectionSource, manifest_name};
