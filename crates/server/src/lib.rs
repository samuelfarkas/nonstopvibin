//! The daemon: the loopback listener with its Host, Origin and token checks, the management API,
//! the agent gateway's host, the interface's files, and the order of startup and shutdown.
//!
//! [`Daemon::start`] opens the store and starts accepting; requests are served from then on.
//! [`Daemon::restore`] selects the core, reaps orphans, starts the profiles that are kept
//! running and restores agent connections, then writes `server.json`. [`Daemon::close`] undoes
//! it all in order. The desktop app's shell drives the same daemon through its public methods
//! and its [`Desktop`] part.
#![forbid(unsafe_code)]

mod api;
mod assets;
mod checks;
mod daemon;
mod desktop;
mod failure;
mod gateway_host;
mod listener;
pub mod panic;
mod parse;
mod session;

pub use checks::{AllowedHost, InvalidHost};
pub use daemon::{Boundaries, CloseError, CoreOptions, Daemon, Options, StartError, VERSION};
pub use desktop::Desktop;
pub use panic::runtime;
pub use session::{ServerState, Session, SessionError, ShowError};
