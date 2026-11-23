//! What the webview cannot do: the clipboard, the folder picker and message dialogs. Each
//! platform module offers `copy`, `choose_folder` and `show_dialog`; answers come back as
//! events on the loop.

use nsv_domain::helper::DialogKind;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub(crate) use linux::{choose_folder, copy, present, show_dialog};
#[cfg(target_os = "macos")]
pub(crate) use macos::{
    MenuAction, choose_folder, copy, install_menu, menu_action, present, show_dialog,
};

pub(crate) const FOLDER_TITLE: &str = "Connect Claude Code in a project";
pub(crate) const FOLDER_BUTTON: &str = "Choose project";

/// A dialog's text, fixed per kind. The first button is "Restart to update" for
/// [`DialogKind::UpdateReady`]; the last button is the default.
pub(crate) struct DialogText {
    pub(crate) message: &'static str,
    pub(crate) detail: &'static str,
    pub(crate) buttons: &'static [&'static str],
    pub(crate) error: bool,
}

/// The texts `desktop/main.ts` and `desktop/updates.ts` showed.
pub(crate) fn dialog_text(kind: DialogKind) -> DialogText {
    let (message, detail, buttons, error): (_, _, &'static [&'static str], _) = match kind {
        DialogKind::UpdateReady => (
            "An update is ready",
            "Restarting stops the local proxy and interrupts connected agents. You can update later from the app or tray menu.",
            &["Restart to update", "Later"],
            false,
        ),
        DialogKind::NoUpdates => (
            "No updates available",
            "You are running the latest available version.",
            &["OK"],
            false,
        ),
        DialogKind::UpdateFailed => (
            "Could not update the app",
            "Check your connection and try again later. Your current installation has not been replaced.",
            &["OK"],
            true,
        ),
        DialogKind::MoveToApplications => (
            "Move NonstopVibin to update it",
            "NonstopVibin can update itself only from a folder you can change, such as Applications or the Applications folder in your home folder. Quit it, move it there, and open it again.",
            &["OK"],
            false,
        ),
        DialogKind::CouldNotStart => (
            "NonstopVibin could not start",
            "Please reopen NonstopVibin and try again.",
            &["OK"],
            true,
        ),
    };
    DialogText {
        message,
        detail,
        buttons,
        error,
    }
}
