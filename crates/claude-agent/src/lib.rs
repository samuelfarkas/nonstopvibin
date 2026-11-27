//! Claude Agent: a profile's Claude subscription served by the user's own signed-in,
//! unmodified `claude` program, so Claude Code makes every request to Anthropic itself
//! (`docs/decisions/claude-agent.md`).
//!
//! The profile's core sends Anthropic Messages requests to `/claude-agent/<profile>/v1/messages`
//! with the account's key. [`Engine`] parses each into a conversation, routes it to a `claude`
//! process that has seen its history, or starts one, and translates that process's
//! stream-json output into the answer. The harness's tools reach Claude Code as an MCP server
//! on `/claude-agent/<profile>/mcp`; a call is held until the harness's next request carries
//! its result.
#![cfg_attr(not(target_os = "linux"), forbid(unsafe_code))]
// Linux's parent-death signal is set in `pre_exec`, which is unsafe; that one call allows it.
#![cfg_attr(target_os = "linux", deny(unsafe_code))]

mod conversation;
mod driver;
mod engine;
mod mcp;
mod process;
mod route;
mod sessions;
mod translate;

pub use engine::{Account, Engine, Host, Options, Probe};
