//! What each platform does its own way: the windows' chrome, the webview's container, the
//! clipboard, the folder picker, message dialogs, the work area, where the app menu shows and,
//! on macOS, the Dock icon. [`Native`] states it once; `linux.rs` (GTK) and `macos.rs` (AppKit)
//! implement it, and answers come back as events on the loop. The dialogs' text, buttons and
//! what each button means are written here once, and the app menu's items in `menu.rs`.

use std::io;
use std::path::PathBuf;

use muda::MenuId;
use nsv_desktop::Command;
use nsv_desktop::pipe::{Anchor, Choice, DialogKind, Failure, Reason, Rect};
use tao::event_loop::EventLoop;
use tao::window::{Window, WindowBuilder};
use wry::{WebView, WebViewBuilder};

use crate::app::{Proxy, Target, Ticket};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod menu;

#[cfg(target_os = "linux")]
pub(crate) use linux::Gtk as Os;
#[cfg(target_os = "macos")]
pub(crate) use macos::AppKit as Os;

pub(crate) trait Native {
    /// What the platform keeps for the whole process.
    type State: Default;

    /// The quota popup hangs below the tray icon, in the menu bar, instead of standing on the
    /// bottom of the work area.
    const POPUP_BELOW_ANCHOR: bool;
    /// The main window has no title bar, so the page's drag regions move it.
    const MAIN_DRAG_REGIONS: bool;

    fn event_loop<T: 'static>() -> EventLoop<T>;
    /// Once the loop runs.
    fn start(state: &mut Self::State, proxy: &Proxy, updates: bool);
    /// After each event; `main_on_screen` tells whether the main window shows.
    fn settle(state: &mut Self::State, target: &Target, main_on_screen: impl FnOnce() -> bool);
    /// A press at page point (`x`, `y`) in a drag region of the main window.
    fn drag(state: &Self::State, window: &Window, x: i32, y: i32);
    /// An app menu item, on the main window if one is open: the command for the daemon, or
    /// `None` when the helper carried it out itself.
    fn menu(
        state: &mut Self::State,
        id: &MenuId,
        main: Option<(&Window, &WebView)>,
    ) -> Option<Command>;

    /// Where WebKit keeps its storage, made from the data directory's `path`; `None` keeps the
    /// platform's default.
    ///
    /// # Errors
    /// When the directory cannot be made the owner's alone.
    fn web_data(path: PathBuf) -> io::Result<Option<PathBuf>>;
    fn main_window(builder: WindowBuilder) -> WindowBuilder;
    fn quota_window(builder: WindowBuilder) -> WindowBuilder;
    /// The main window has been built.
    fn main_opened(state: &mut Self::State, window: &Window);
    /// The main window, if any, is about to be destroyed.
    fn main_closing(state: &mut Self::State);
    /// The webview inside `window`.
    ///
    /// # Errors
    /// What failed to build it.
    fn webview(builder: WebViewBuilder<'_>, window: &Window) -> Result<WebView, String>;
    /// Shows `window` and gives it the focus.
    fn present(window: &Window);
    /// # Errors
    /// The message the page's promise rejects with.
    fn copy(text: &str) -> Result<(), String>;
    /// A folder picker over the main window; answers with `FolderChosen`.
    fn choose_folder(parent: &Window, proxy: &Proxy, ticket: Ticket);
    /// A message dialog; calls `answered` once with the user's choice.
    fn show_dialog(text: &Dialog<'_>, answered: impl FnOnce(Choice) + 'static);
    /// The work area of the display that holds `anchor`, or of the nearest one.
    fn work_area(anchor: Anchor) -> Option<Rect>;
}

pub(crate) const FOLDER_TITLE: &str = "Connect Claude Code in a project";
pub(crate) const FOLDER_BUTTON: &str = "Choose project";

const REOPEN: &str = "Please reopen NonstopVibin and try again.";

/// A message dialog's text and buttons.
pub(crate) struct Dialog<'a> {
    pub(crate) message: &'static str,
    pub(crate) detail: &'a str,
    pub(crate) buttons: Buttons,
    pub(crate) error: bool,
}

/// A dialog's buttons in order, each with what it answers. The last is the default, as main.ts
/// made "Later"; closing the dialog without a button dismisses it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Buttons(&'static [(&'static str, Choice)]);

const OK: Buttons = Buttons(&[("OK", Choice::Dismiss)]);

impl Buttons {
    pub(crate) fn labels(self) -> impl Iterator<Item = &'static str> {
        self.0.iter().map(|&(label, _)| label)
    }

    /// Return presses it.
    pub(crate) const fn default(self) -> usize {
        self.0.len().saturating_sub(1)
    }

    /// What the user chose with the button at `index`; `None` closed the dialog without one.
    pub(crate) fn choice(self, index: Option<usize>) -> Choice {
        index
            .and_then(|index| self.0.get(index))
            .map_or(Choice::Dismiss, |&(_, choice)| choice)
    }
}

impl Dialog<'_> {
    /// The dialogs `desktop/main.ts` and `desktop/updates.ts` showed.
    pub(crate) const fn of(kind: DialogKind) -> Dialog<'static> {
        let (message, detail, buttons, error) = match kind {
            DialogKind::UpdateReady => (
                "An update is ready",
                "Restarting stops the local proxy and interrupts connected agents. You can update later from the app or tray menu.",
                Buttons(&[
                    ("Restart to update", Choice::Accept),
                    ("Later", Choice::Dismiss),
                ]),
                false,
            ),
            DialogKind::NoUpdates => (
                "No updates available",
                "You are running the latest available version.",
                OK,
                false,
            ),
            DialogKind::UpdateFailed => (
                "Could not update the app",
                "Check your connection and try again later. Your current installation has not been replaced.",
                OK,
                true,
            ),
            DialogKind::MoveToApplications => (
                "Move NonstopVibin to update it",
                "NonstopVibin can update itself only from a folder you can change, such as Applications or the Applications folder in your home folder. Quit it, move it there, and open it again.",
                OK,
                false,
            ),
            DialogKind::UpdateGone => (
                "The update is no longer available",
                "It was removed before it could be installed, and NonstopVibin keeps running. Check for updates to download it again.",
                OK,
                true,
            ),
            DialogKind::CouldNotRestart => ("Could not restart for update", REOPEN, OK, true),
            DialogKind::CouldNotInstall => ("Could not install update", REOPEN, OK, true),
        };
        Dialog {
            message,
            detail,
            buttons,
            error,
        }
    }

    /// main.ts's error box for a daemon that failed, with `reason` as its text.
    pub(crate) fn alert(failure: Failure, reason: &Reason) -> Dialog<'_> {
        Dialog {
            message: failure.title(),
            detail: reason.as_str(),
            buttons: OK,
            error: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_restart_to_update_accepts_and_return_presses_later() {
        let ready = Dialog::of(DialogKind::UpdateReady).buttons;
        let labels: Vec<_> = ready.labels().collect();
        assert_eq!(labels, ["Restart to update", "Later"]);
        assert_eq!(ready.default(), 1);
        assert_eq!(ready.choice(Some(0)), Choice::Accept);
        assert_eq!(ready.choice(Some(1)), Choice::Dismiss);
        assert_eq!(ready.choice(None), Choice::Dismiss);
        let failed = Dialog::of(DialogKind::UpdateFailed).buttons;
        assert_eq!(failed.default(), 0);
        assert_eq!(failed.choice(Some(0)), Choice::Dismiss);
    }
}
