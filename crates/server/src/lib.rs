//! The daemon: the loopback listener with its Host, Origin and token checks, the management API,
//! the agent gateway's host, the interface's files, and the order of startup and shutdown.
//!
//! [`Daemon::start`] opens the store and starts accepting; requests are served from then on.
//! [`Daemon::restore`] selects the core, reaps orphans, starts the profiles that are kept
//! running and restores agent connections, then writes `server.json`. [`Daemon::close`] undoes
//! it all in order. The desktop shell drives the same daemon through [`nsv_shell::ShellHost`].
#![forbid(unsafe_code)]

mod api;
mod assets;
mod daemon;
mod features;
mod http;
pub mod panic;
mod parse;
mod session;

pub use daemon::{CloseError, CoreOptions, Daemon, Options, StartError, VERSION};
pub use features::{FeatureContext, FeatureHost, Features, Unwired};
pub use http::valid_allow_host;
pub use session::{ServerState, Session, SessionError};
