//! The shapes Linux and macOS each implement once: every platform twin in the shell implements
//! one of these traits, beside the code it shares with the other platform.

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::install::Install;
use crate::tray::{Action, Look, TrayLink};
use crate::update::Detected;

/// The running platform, for the twins that keep no state of their own.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Os;

/// Which thread runs what (`process.rs`).
pub(crate) trait Threads {
    /// Runs `daemon` and exits with its code. macOS runs AppKit's loop on this, the main,
    /// thread for the menu bar item and the daemon on another; Linux runs the daemon here.
    fn run_desktop(daemon: impl FnOnce(TrayLink) -> u8 + Send + 'static) -> ExitCode;
}

/// The tray icon and its menu as the shell's loop sees them (`tray/linux.rs`,
/// `tray/macos.rs`).
pub(crate) trait SystemTray: Sized {
    /// What the main thread hands the daemon thread for the tray.
    type Link;

    /// Without a tray host the shell runs on without a tray, as Electron did.
    async fn start(link: Self::Link, look: Look, updates: bool) -> Self;
    /// `None` once no tray can report anything.
    async fn next(&mut self) -> Option<Action>;
    async fn show(&self, look: Look);
}

/// Starting at login (`login.rs`).
pub(crate) trait LoginItem: Sized {
    fn of(install: &Install) -> io::Result<Self>;
    fn enabled(&self) -> bool;
    /// Turns it on or off and answers whether it is on now.
    fn set(&self, enabled: bool) -> Result<bool, String>;
    /// An update may move the app: points an existing registration at this one and creates
    /// none.
    fn follow(&self) -> Result<(), String>;
}

/// What an app update replaces (`update/linux.rs`, `update/macos.rs`).
pub(crate) trait UpdateTarget: Sized {
    /// The release's feed for this platform.
    const FEED: &'static str;
    /// How the feed's payload name ends.
    const PAYLOAD: &'static str;

    fn detect(install: &Install) -> Detected<Self>;
    /// What the swap replaces.
    fn path(&self) -> &Path;
    /// What starts the updated app.
    fn executable(&self) -> &Path;
    /// Readies the verified `payload` in `staging`: answers what replaces [`Self::path`].
    async fn prepare(&self, staging: &Path, payload: &Path) -> Result<PathBuf, String>;
}

/// What the desktop opens for the app (`external.rs`).
pub(crate) trait Opener {
    /// Opens a URL in the default browser, or a file with its default app.
    fn open(target: &std::ffi::OsStr) -> Result<(), String>;

    /// Shows `file` selected in the file manager, as Electron's `showItemInFolder`.
    async fn reveal(file: &Path) -> Result<(), String>;
}
