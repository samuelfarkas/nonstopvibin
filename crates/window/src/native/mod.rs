//! What the webview cannot do: the clipboard, the folder picker and message dialogs. Each
//! platform module offers `copy`, `choose_folder` and `show_dialog`; answers come back as
//! events on the loop. The dialogs' text, buttons and what each button means are written here
//! once.

use nsv_desktop::pipe::{Choice, DialogKind, Failure, Reason};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub(crate) use linux::{choose_folder, copy, present, set_icon, show_dialog, work_area};
#[cfg(target_os = "macos")]
pub(crate) use macos::{
    MenuAction, choose_folder, copy, drag, install_menu, menu_action, present, show_dialog,
    work_area,
};

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
