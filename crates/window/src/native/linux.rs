//! GTK 3, which tao and WebKitGTK already run on this thread's main loop.

use std::cell::Cell;

use gtk::prelude::*;
use gtk::{ButtonsType, DialogFlags, FileChooserAction, MessageType, ResponseType};
use nsv_domain::helper::DialogKind;
use tao::platform::unix::WindowExtUnix;
use tao::window::Window;

use super::{FOLDER_BUTTON, FOLDER_TITLE, dialog_text};
use crate::app::{Proxy, Ticket, UserEvent, post};

const NON_UTF8_FOLDER: &str = "The project folder's path is not valid UTF-8.";

/// Shows `window` and gives it the focus. tao queues `set_visible` and ignores `set_focus`
/// until the window is visible, and GTK's `present` only shows a hidden window; so show it
/// here, then present the visible window, which focuses it.
pub(crate) fn present(window: &Window) {
    let window = window.gtk_window();
    window.show_all();
    window.present();
}

#[expect(clippy::unnecessary_wraps, reason = "the macOS twin can fail")]
pub(crate) fn copy(text: &str) -> Result<(), String> {
    let clipboard = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
    clipboard.set_text(text);
    // Hands the text to the clipboard manager, so it outlives this short-lived process.
    clipboard.store();
    Ok(())
}

/// A folder picker over the main window; answers with `FolderChosen`.
pub(crate) fn choose_folder(parent: &Window, proxy: &Proxy, ticket: Ticket) {
    let dialog = gtk::FileChooserDialog::with_buttons(
        Some(FOLDER_TITLE),
        Some(parent.gtk_window()),
        FileChooserAction::SelectFolder,
        &[
            ("_Cancel", ResponseType::Cancel),
            (FOLDER_BUTTON, ResponseType::Accept),
        ],
    );
    dialog.set_modal(true);
    dialog.set_default_response(ResponseType::Accept);
    let (answered, proxy) = (Cell::new(false), proxy.clone());
    dialog.connect_response(move |dialog, response| {
        // Closing the dialog emits one more response, DeleteEvent.
        if answered.replace(true) {
            return;
        }
        let path = if response == ResponseType::Accept {
            dialog.filename()
        } else {
            None
        };
        let result = match path {
            None => Ok(None),
            Some(path) => path
                .into_os_string()
                .into_string()
                .map(Some)
                .map_err(|_| NON_UTF8_FOLDER.to_owned()),
        };
        post(&proxy, UserEvent::FolderChosen { ticket, result });
        dialog.close();
    });
    dialog.show_all();
    dialog.present();
}

/// A message dialog of its own; answers with `DialogAnswered`.
pub(crate) fn show_dialog(kind: DialogKind, proxy: &Proxy) {
    let text = dialog_text(kind);
    let dialog = gtk::MessageDialog::new(
        None::<&gtk::Window>,
        DialogFlags::empty(),
        if text.error {
            MessageType::Error
        } else {
            MessageType::Info
        },
        ButtonsType::None,
        text.message,
    );
    dialog.set_title("NonstopVibin");
    dialog.set_secondary_text(Some(text.detail));
    let mut last = ResponseType::None;
    for (index, label) in (0u16..).zip(text.buttons) {
        last = ResponseType::Other(index);
        dialog.add_button(label, last);
    }
    dialog.set_default_response(last);
    let (answered, proxy) = (Cell::new(false), proxy.clone());
    dialog.connect_response(move |dialog, response| {
        if answered.replace(true) {
            return;
        }
        let restart = kind == DialogKind::UpdateReady && response == ResponseType::Other(0);
        post(&proxy, UserEvent::DialogAnswered { kind, restart });
        dialog.close();
    });
    dialog.show_all();
    dialog.present();
}
