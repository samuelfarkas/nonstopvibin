//! The desktop shell: the tray, the window helper's supervision, the login item and app
//! updates. The desktop app implements [`ShellHost`]; on Linux the shell links no GUI library.
#![cfg_attr(not(target_os = "macos"), forbid(unsafe_code))]
// SMAppService's bindings are unsafe functions; macOS code allows them one call at a time.
#![cfg_attr(target_os = "macos", deny(unsafe_code))]

mod external;
mod helper;
mod install;
mod login;
mod platform;
mod process;
mod shell;
mod tray;
mod update;

use nsv_files::DataDir;
use tokio::sync::{broadcast, watch};

pub use helper::alert;
pub use install::Install;
pub use login::Login;
// What `ShellHost` and `alert` speak, so their callers need no other crate.
pub use nsv_desktop::pipe::Failure;
pub use nsv_desktop::{HostEvent, TrayStatus};
pub use nsv_domain::endpoint::Origin;
pub use nsv_domain::id::ProfileId;
pub use nsv_domain::secret::SessionToken;
pub use process::run_desktop;
pub use shell::{Options, run};
pub use tray::TrayLink;

/// What the shell needs from the daemon.
pub trait ShellHost {
    /// `http://127.0.0.1:<port>`: the only origin the windows show.
    fn origin(&self) -> &Origin;
    /// The session token the helper hands the UI; never in argv, the environment or a URL.
    fn session_token(&self) -> &SessionToken;
    fn data_directory(&self) -> &DataDir;
    /// The running profiles' names and whether any request streams. The tray redraws on each
    /// change that alters its icon or tooltip; it never polls.
    fn tray_status(&self) -> watch::Receiver<TrayStatus>;
    /// Sign-ins and second launches. A closed channel ends [`run`].
    fn events(&self) -> broadcast::Receiver<HostEvent>;
    fn has_profile(&self, profile: &ProfileId) -> bool;
    /// Shows an error in the UI, as `Application.report` did.
    fn report(&self, message: &str);
    /// Starts the graceful shutdown: Quit in the tray or in the app menu. [`run`] returns next.
    fn shutdown(&self);
    /// Stops the daemon gracefully and waits for it: open requests finish, usage drains, the
    /// cores stop and the data directory is released, so an update can start in its place.
    /// Later calls wait for the first.
    ///
    /// # Errors
    /// What failed to stop.
    fn close(&self) -> impl Future<Output = Result<(), String>>;
}

/// The daemon stopped for an update that could not be installed; the process exits with a
/// failure after the error dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateFailed;
