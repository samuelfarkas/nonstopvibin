//! The desktop contract between the daemon, its shell and the window helper: the pipe's
//! messages, limits and handshake, the bridge methods a page calls, the menu commands, and the
//! plain types the daemon hands the shell.
#![forbid(unsafe_code)]

pub mod bridge;
mod command;
pub mod pipe;

pub use command::Command;

/// What the daemon reports for the tray.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrayStatus {
    /// The running profiles' names, in the order the tooltip lists them.
    pub running: Vec<String>,
    pub streaming: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostEvent {
    /// A provider sign-in finished: an open main window comes forward to show it.
    SignInAccepted,
    /// `POST /api/desktop/show` from a second launch.
    ShowMain,
}
