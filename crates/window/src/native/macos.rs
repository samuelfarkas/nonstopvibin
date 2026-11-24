//! AppKit: the pasteboard, `NSOpenPanel`, `NSAlert`, the menu bar, which exists only while
//! the helper runs, and the Dock icon, which shows only while the main window does.

use std::io;
use std::path::PathBuf;

use muda::MenuId;
use nsv_desktop::Command;
use nsv_desktop::pipe::{Anchor, Choice, Rect};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSEvent, NSEventMask,
    NSEventType, NSModalResponseOK, NSOpenPanel, NSPasteboard, NSScreen,
};
use objc2_foundation::{NSDate, NSPoint, NSRect, NSString};
use tao::dpi::LogicalPosition;
use tao::event_loop::{EventLoop, EventLoopBuilder};
use tao::platform::macos::{
    ActivationPolicy, EventLoopExtMacOS, EventLoopWindowTargetExtMacOS, WindowBuilderExtMacOS,
    WindowExtMacOS,
};
use tao::window::{Window, WindowBuilder};
use wry::{WebView, WebViewBuilder};

use super::menu::{self, Action, AppMenu, Zoom};
use super::{Dialog, FOLDER_BUTTON, FOLDER_TITLE, Native};
use crate::app::{Proxy, Target, Ticket, UserEvent, log, post};

/// `NSPasteboardTypeString`'s value: the static itself is an extern symbol only unsafe code
/// can read.
const PLAIN_TEXT: &str = "public.utf8-plain-text";
/// `NSEventTrackingRunLoopMode`'s value, for the same reason.
const EVENT_TRACKING: &str = "NSEventTrackingRunLoopMode";

pub(crate) struct AppKit;

#[derive(Default)]
pub(crate) struct State {
    /// The menu bar, from the loop's start.
    menu: Option<AppMenu>,
    /// The Dock icon shows.
    regular: bool,
    zoom: Zoom,
}

impl Native for AppKit {
    type State = State;

    const POPUP_BELOW_ANCHOR: bool = true;
    const MAIN_DRAG_REGIONS: bool = true;

    /// A UI element until the main window shows.
    fn event_loop<T: 'static>() -> EventLoop<T> {
        let mut event_loop = EventLoopBuilder::<T>::with_user_event().build();
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
        event_loop
    }

    /// The app menu in the menu bar.
    fn start(state: &mut State, proxy: &Proxy, updates: bool) {
        match menu::build(updates) {
            Ok(app_menu) => {
                app_menu.menu.init_for_nsapp();
                app_menu.window.set_as_windows_menu_for_nsapp();
                menu::listen(proxy);
                state.menu = Some(app_menu);
            }
            Err(error) => log(format_args!("could not build the menu: {error}")),
        }
    }

    /// The command for the daemon, or `None` when the item was the page zoom.
    fn menu(state: &mut State, id: &MenuId, main: Option<(&Window, &WebView)>) -> Option<Command> {
        match state.menu.as_ref()?.action(id)? {
            Action::Command(command) => Some(command),
            Action::Zoom(step) => {
                state.zoom.apply(step, main.map(|(_, webview)| webview));
                None
            }
        }
    }

    /// The Dock icon shows while the main window is on screen.
    fn settle(state: &mut State, target: &Target, main_on_screen: impl FnOnce() -> bool) {
        let regular = main_on_screen();
        if regular != state.regular {
            state.regular = regular;
            let policy = if regular {
                ActivationPolicy::Regular
            } else {
                ActivationPolicy::Accessory
            };
            target.set_activation_policy_at_runtime(policy);
        }
    }

    /// Page points are window points scaled by the page zoom.
    fn drag(state: &State, window: &Window, x: i32, y: i32) {
        let scale = state.zoom.scale();
        drag_window(window, f64::from(x) * scale, f64::from(y) * scale);
    }

    /// WebKit's default store.
    fn web_data(_path: PathBuf) -> io::Result<Option<PathBuf>> {
        Ok(None)
    }

    fn main_window(builder: WindowBuilder) -> WindowBuilder {
        builder
            .with_titlebar_transparent(true)
            .with_title_hidden(true)
            .with_fullsize_content_view(true)
            .with_traffic_light_inset(LogicalPosition::new(21.0, 17.0))
    }

    fn quota_window(builder: WindowBuilder) -> WindowBuilder {
        builder
    }

    fn main_opened(_state: &mut State, _window: &Window) {}

    fn main_closing(_state: &mut State) {}

    fn webview(builder: WebViewBuilder<'_>, window: &Window) -> Result<WebView, String> {
        builder.build(window).map_err(|error| error.to_string())
    }

    /// Brings the helper to the front too.
    fn present(window: &Window) {
        window.set_visible(true);
        window.set_focus();
    }

    /// The pasteboard server keeps the text after this process exits.
    fn copy(text: &str) -> Result<(), String> {
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        if pasteboard.setString_forType(&NSString::from_str(text), &NSString::from_str(PLAIN_TEXT))
        {
            Ok(())
        } else {
            Err("Could not copy to the clipboard.".to_owned())
        }
    }

    /// An application-modal folder picker; answers with `FolderChosen`.
    fn choose_folder(_parent: &Window, proxy: &Proxy, ticket: Ticket) {
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

    /// An application-modal alert; calls `answered` with the user's choice once it is dismissed.
    fn show_dialog(text: &Dialog<'_>, answered: impl FnOnce(Choice) + 'static) {
        let Some(main_thread) = MainThreadMarker::new() else {
            log(format_args!("a dialog must open on the main thread"));
            return;
        };
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
            .labels()
            .map(|label| alert.addButtonWithTitle(&NSString::from_str(label)))
            .collect();
        // AppKit makes the first button the default.
        if let (Some(first), Some(default)) = (buttons.first(), buttons.get(text.buttons.default()))
        {
            first.setKeyEquivalent(&NSString::from_str(""));
            default.setKeyEquivalent(&NSString::from_str("\r"));
        }
        let index = alert
            .runModal()
            .checked_sub(NSAlertFirstButtonReturn)
            .and_then(|index| usize::try_from(index).ok());
        answered(text.buttons.choice(index));
    }

    /// The visible frame (without the menu bar and the Dock) of the screen that holds `anchor`, or
    /// of the main screen.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "screen frames are whole points well inside i32"
    )]
    fn work_area(anchor: Anchor) -> Option<Rect> {
        let screens = NSScreen::screens(MainThreadMarker::new()?).to_vec();
        // Cocoa's origin is the bottom left of the first screen; tao's, the anchor's, the top left.
        let top = screens.first()?.frame().size.height;
        let flip = |frame: NSRect| {
            let y = top - frame.origin.y - frame.size.height;
            (frame.origin.x, y, frame.size.width, frame.size.height)
        };
        let (x, y) = (f64::from(anchor.x), f64::from(anchor.y));
        let screen = screens
            .iter()
            .find(|screen| {
                let (left, upper, width, height) = flip(screen.frame());
                (left..left + width).contains(&x) && (upper..upper + height).contains(&y)
            })
            .or_else(|| screens.first())?;
        let (x, y, width, height) = flip(screen.visibleFrame());
        Some(Rect {
            x: x.round() as i32,
            y: y.round() as i32,
            width: width.round() as u32,
            height: height.round() as u32,
        })
    }
}

/// Moves `window` with the pointer until the primary button is released: a press at (`x`, `y`),
/// in points from the window's top left, in a drag region. The page reports the press after
/// `mouseDown:` has returned, when the window server no longer starts a drag for it
/// (`performWindowDragWithEvent:`, which tao's `drag_window` calls, leaves the window in place),
/// so the helper tracks the drag itself, from the press, however late the report arrives.
fn drag_window(window: &Window, x: f64, y: f64) {
    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    // Released already: the press was a click.
    if NSEvent::pressedMouseButtons() & 1 == 0 {
        return;
    }
    let app = NSApplication::sharedApplication(main_thread);
    // The press made the window key; any other key window means the pointer left it.
    let Some(key) = app
        .keyWindow()
        .filter(|key| Retained::as_ptr(key).cast_mut().cast() == window.ns_window())
    else {
        return;
    };
    let frame = key.frame();
    let start = key.convertPointToScreen(NSPoint::new(x, frame.size.height - y));
    let until = NSDate::distantFuture();
    let tracking = NSString::from_str(EVENT_TRACKING);
    let follow = || {
        let pointer = NSEvent::mouseLocation();
        key.setFrameOrigin(NSPoint::new(
            frame.origin.x + pointer.x - start.x,
            frame.origin.y + pointer.y - start.y,
        ));
    };
    // What the pointer did before the report arrived.
    follow();
    while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
        NSEventMask::LeftMouseDragged | NSEventMask::LeftMouseUp,
        Some(&until),
        &tracking,
        true,
    ) {
        follow();
        if event.r#type() == NSEventType::LeftMouseUp {
            // The page saw the press; it sees the release too.
            key.sendEvent(&event);
            break;
        }
    }
}

/// Brings the helper to the front: the daemon that asked for the dialog has no windows.
#[expect(deprecated, reason = "its replacement, activate(), needs macOS 14")]
fn activate(main_thread: MainThreadMarker) {
    NSApplication::sharedApplication(main_thread).activateIgnoringOtherApps(true);
}
