//! AppKit: the pasteboard, `NSOpenPanel`, `NSAlert` and the menu bar, which exists only while
//! the helper runs.

use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{AboutMetadata, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use nsv_domain::helper::DialogKind;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSModalResponseOK, NSOpenPanel,
    NSPasteboard,
};
use objc2_foundation::NSString;
use tao::window::Window;

use super::{FOLDER_BUTTON, FOLDER_TITLE, dialog_text};
use crate::app::{Proxy, Ticket, UserEvent, log, post};

/// `NSPasteboardTypeString`'s value: the static itself is an extern symbol only unsafe code
/// can read.
const PLAIN_TEXT: &str = "public.utf8-plain-text";

/// The pasteboard server keeps the text after this process exits.
pub(crate) fn copy(text: &str) -> Result<(), String> {
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    if pasteboard.setString_forType(&NSString::from_str(text), &NSString::from_str(PLAIN_TEXT)) {
        Ok(())
    } else {
        Err("Could not copy to the clipboard.".to_owned())
    }
}

/// Brings the helper to the front: the daemon that asked for the dialog has no windows.
#[expect(deprecated, reason = "its replacement, activate(), needs macOS 14")]
fn activate(main_thread: MainThreadMarker) {
    NSApplication::sharedApplication(main_thread).activateIgnoringOtherApps(true);
}

/// An application-modal folder picker; answers with `FolderChosen`.
pub(crate) fn choose_folder(_parent: &Window, proxy: &Proxy, ticket: Ticket) {
    let Some(main_thread) = MainThreadMarker::new() else {
        log(format_args!(
            "the folder picker must open on the main thread"
        ));
        return;
    };
    activate(main_thread);
    let panel = NSOpenPanel::openPanel(main_thread);
    panel.setCanChooseFiles(false);
    panel.setCanChooseDirectories(true);
    panel.setAllowsMultipleSelection(false);
    panel.setCanCreateDirectories(true);
    panel.setTitle(Some(&NSString::from_str(FOLDER_TITLE)));
    panel.setMessage(Some(&NSString::from_str(FOLDER_TITLE)));
    panel.setPrompt(Some(&NSString::from_str(FOLDER_BUTTON)));
    let chosen = (panel.runModal() == NSModalResponseOK)
        .then(|| panel.URLs().firstObject())
        .flatten()
        .and_then(|url| url.path());
    let result = Ok(chosen.map(|path| path.to_string()));
    post(proxy, UserEvent::FolderChosen { ticket, result });
}

/// An application-modal alert; answers with `DialogAnswered`.
pub(crate) fn show_dialog(kind: DialogKind, proxy: &Proxy) {
    let Some(main_thread) = MainThreadMarker::new() else {
        log(format_args!("a dialog must open on the main thread"));
        return;
    };
    let text = dialog_text(kind);
    activate(main_thread);
    let alert = NSAlert::new(main_thread);
    alert.setMessageText(&NSString::from_str(text.message));
    alert.setInformativeText(&NSString::from_str(text.detail));
    alert.setAlertStyle(if text.error {
        NSAlertStyle::Critical
    } else {
        NSAlertStyle::Informational
    });
    let buttons: Vec<_> = text
        .buttons
        .iter()
        .map(|label| alert.addButtonWithTitle(&NSString::from_str(label)))
        .collect();
    // AppKit makes the first button the default; main.ts made "Later" (the last) the default.
    if let [first, .., last] = buttons.as_slice() {
        first.setKeyEquivalent(&NSString::from_str(""));
        last.setKeyEquivalent(&NSString::from_str("\r"));
    }
    let response = alert.runModal();
    let restart = kind == DialogKind::UpdateReady && response == NSAlertFirstButtonReturn;
    post(proxy, UserEvent::DialogAnswered { kind, restart });
}

/// What a menu item asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuAction {
    CheckUpdates,
    QuitApp,
    ToggleQuota,
    OpenMain,
    ResetZoom,
    ZoomIn,
    ZoomOut,
}

const ACTIONS: [(&str, MenuAction); 7] = [
    ("check-updates", MenuAction::CheckUpdates),
    ("quit", MenuAction::QuitApp),
    ("quotas", MenuAction::ToggleQuota),
    ("open-main", MenuAction::OpenMain),
    ("reset-zoom", MenuAction::ResetZoom),
    ("zoom-in", MenuAction::ZoomIn),
    ("zoom-out", MenuAction::ZoomOut),
];

pub(crate) fn menu_action(id: &MenuId) -> Option<MenuAction> {
    ACTIONS
        .iter()
        .find(|(name, _)| id == *name)
        .map(|&(_, action)| action)
}

fn item(action: MenuAction, label: &str, modifiers: Modifiers, key: Option<Code>) -> MenuItem {
    let id = ACTIONS
        .iter()
        .find(|(_, a)| *a == action)
        .map_or("", |(name, _)| name);
    MenuItem::with_id(
        id,
        label,
        true,
        key.map(|key| Accelerator::new(modifiers, key)),
    )
}

/// Electron's application menu (`desktop/main.ts`). Menu events arrive as `UserEvent::Menu`.
pub(crate) fn install_menu(proxy: &Proxy) -> muda::Result<Menu> {
    let command = Modifiers::META;
    let app = Submenu::with_items(
        "NonstopVibin",
        true,
        &[
            &PredefinedMenuItem::about(
                None,
                Some(AboutMetadata {
                    name: Some("NonstopVibin".to_owned()),
                    version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                    ..AboutMetadata::default()
                }),
            ),
            &item(
                MenuAction::CheckUpdates,
                "Check for updates…",
                command,
                None,
            ),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &item(
                MenuAction::QuitApp,
                "Quit NonstopVibin",
                command,
                Some(Code::KeyQ),
            ),
        ],
    )?;
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
        ],
    )?;
    let view = Submenu::with_items(
        "View",
        true,
        &[
            &item(
                MenuAction::ToggleQuota,
                "Subscription quotas",
                command | Modifiers::SHIFT,
                Some(Code::KeyU),
            ),
            &item(
                MenuAction::OpenMain,
                "Open main window",
                command,
                Some(Code::Digit1),
            ),
            &PredefinedMenuItem::separator(),
            &item(
                MenuAction::ResetZoom,
                "Actual Size",
                command,
                Some(Code::Digit0),
            ),
            &item(MenuAction::ZoomIn, "Zoom In", command, Some(Code::Equal)),
            &item(MenuAction::ZoomOut, "Zoom Out", command, Some(Code::Minus)),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::fullscreen(None),
        ],
    )?;
    let window = Submenu::with_items(
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::maximize(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::bring_all_to_front(None),
        ],
    )?;
    let menu = Menu::with_items(&[&app, &edit, &view, &window])?;
    menu.init_for_nsapp();
    window.set_as_windows_menu_for_nsapp();
    let proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        post(&proxy, UserEvent::Menu(event.id));
    }));
    Ok(menu)
}
