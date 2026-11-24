//! GTK 3, which tao and WebKitGTK already run on this thread's main loop.

use std::cell::Cell;
use std::io;
use std::path::PathBuf;

use gtk::gdk::prelude::*;
use gtk::gdk_pixbuf::{InterpType, PixbufLoader};
use gtk::prelude::*;
use gtk::{ButtonsType, DialogFlags, FileChooserAction, MessageType, ResponseType};
use muda::MenuId;
use nsv_desktop::Command;
use nsv_desktop::pipe::{Anchor, Choice, Rect};
use tao::event_loop::{EventLoop, EventLoopBuilder};
use tao::platform::unix::{WindowBuilderExtUnix, WindowExtUnix};
use tao::window::{Fullscreen, Window, WindowBuilder};
use wry::{WebView, WebViewBuilder, WebViewBuilderExtUnix};

use super::menu::{self, Action, AppMenu, WindowItem, Zoom};
use super::{Dialog, FOLDER_BUTTON, FOLDER_TITLE, Native};
use crate::app::{Proxy, Target, Ticket, UserEvent, log, post};

/// The app icon Electron gave the main window (`dist/client/icon.png`).
const ICON: &[u8] = include_bytes!("../../../../public/icon.png");

const NON_UTF8_FOLDER: &str = "The project folder's path is not valid UTF-8.";

pub(crate) struct Gtk;

#[derive(Default)]
pub(crate) struct State {
    /// "Check for updates…" is enabled.
    updates: bool,
    /// The main window's menu bar and its page's zoom: each main window gets its own.
    bar: Option<(AppMenu, Zoom)>,
}

/// The app menu is the main window's menu bar, and the windows keep their title bars.
impl Native for Gtk {
    type State = State;

    const POPUP_BELOW_ANCHOR: bool = false;
    const MAIN_DRAG_REGIONS: bool = false;

    /// GTK names the windows' X11 class and Wayland app id after the program, so they match
    /// the desktop entry (`nonstopvibin.desktop`) and docks show its icon. tao's app id cannot
    /// do it: `GApplication` refuses an id without a dot, and with one the helper would become
    /// a unique application that a second helper on the session bus hands over to and exits.
    fn event_loop<T: 'static>() -> EventLoop<T> {
        gtk::glib::set_prgname(Some("nonstopvibin"));
        EventLoopBuilder::<T>::with_user_event().build()
    }

    fn start(state: &mut State, proxy: &Proxy, updates: bool) {
        state.updates = updates;
        menu::listen(proxy);
    }

    fn settle(_state: &mut State, _target: &Target, _main_on_screen: impl FnOnce() -> bool) {}

    fn drag(_state: &State, _window: &Window, _x: i32, _y: i32) {}

    /// The command for the daemon, or `None` when the item zoomed the page or was a window
    /// item.
    fn menu(state: &mut State, id: &MenuId, main: Option<(&Window, &WebView)>) -> Option<Command> {
        let (bar, zoom) = state.bar.as_mut()?;
        match bar.action(id)? {
            Action::Command(command) => return Some(command),
            Action::Zoom(step) => zoom.apply(step, main.map(|(_, webview)| webview)),
            Action::Window(item) => {
                if let Some((window, _)) = main {
                    window_item(window, item);
                }
            }
        }
        None
    }

    /// Keeps the webview's storage in `path`, for the owner only.
    fn web_data(path: PathBuf) -> io::Result<Option<PathBuf>> {
        use std::fs;
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_dir() {
            return Err(io::Error::other(format!(
                "{} is not a directory",
                path.display()
            )));
        }
        if metadata.permissions().mode() & 0o777 != 0o700 {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Some(path))
    }

    fn main_window(builder: WindowBuilder) -> WindowBuilder {
        builder
    }

    fn quota_window(builder: WindowBuilder) -> WindowBuilder {
        builder.with_skip_taskbar(true)
    }

    /// The window manager's icon, which Wayland compositors take from the desktop entry
    /// instead, and the menu bar.
    fn main_opened(state: &mut State, window: &Window) {
        set_icon(window);
        let bar = menu::build(state.updates).and_then(|bar| {
            bar.menu
                .init_for_gtk_window(window.gtk_window(), window.default_vbox())?;
            Ok(bar)
        });
        state.bar = match bar {
            Ok(bar) => Some((bar, Zoom::default())),
            Err(error) => {
                log(format_args!("could not build the menu: {error}"));
                None
            }
        };
    }

    /// The menu bar goes while its items exist, so that it can remove their shortcuts.
    fn main_closing(state: &mut State) {
        state.bar = None;
    }

    fn webview(builder: WebViewBuilder<'_>, window: &Window) -> Result<WebView, String> {
        let container = window
            .default_vbox()
            .ok_or("the window has no GTK container")?;
        builder
            .build_gtk(container)
            .map_err(|error| error.to_string())
    }

    /// tao queues `set_visible` and ignores `set_focus` until the window is visible, and GTK's
    /// `present` only shows a hidden window; so show it here, then present the visible window,
    /// which focuses it.
    fn present(window: &Window) {
        let window = window.gtk_window();
        window.show_all();
        window.present();
    }

    fn copy(text: &str) -> Result<(), String> {
        let clipboard = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
        clipboard.set_text(text);
        // Hands the text to the clipboard manager, so it outlives this short-lived process.
        clipboard.store();
        Ok(())
    }

    fn choose_folder(parent: &Window, proxy: &Proxy, ticket: Ticket) {
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

    /// A message dialog of its own.
    fn show_dialog(text: &Dialog<'_>, answered: impl FnOnce(Choice) + 'static) {
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
        // Plain text: GTK reads markup only when told to.
        dialog.set_secondary_text(Some(text.detail));
        for (index, label) in (0u16..).zip(text.buttons.labels()) {
            dialog.add_button(label, ResponseType::Other(index));
        }
        if let Ok(default) = u16::try_from(text.buttons.default()) {
            dialog.set_default_response(ResponseType::Other(default));
        }
        let (buttons, answered) = (text.buttons, Cell::new(Some(answered)));
        dialog.connect_response(move |dialog, response| {
            // Closing the dialog emits one more response, DeleteEvent.
            let Some(answered) = answered.take() else {
                return;
            };
            let index = match response {
                ResponseType::Other(index) => Some(usize::from(index)),
                _ => None,
            };
            answered(buttons.choice(index));
            dialog.close();
        });
        dialog.show_all();
        dialog.present();
    }

    fn work_area(anchor: Anchor) -> Option<Rect> {
        let monitor = gtk::gdk::Display::default()?.monitor_at_point(anchor.x, anchor.y)?;
        let area = monitor.workarea();
        Some(Rect {
            x: area.x(),
            y: area.y(),
            width: u32::try_from(area.width()).ok()?,
            height: u32::try_from(area.height()).ok()?,
        })
    }
}

fn window_item(window: &Window, item: WindowItem) {
    match item {
        WindowItem::FullScreen => {
            let full = window.fullscreen().is_none();
            window.set_fullscreen(full.then_some(Fullscreen::Borderless(None)));
        }
        WindowItem::Minimize => window.set_minimized(true),
        // As the title bar's close button: the page may keep the window for a draft.
        WindowItem::Close => window.gtk_window().close(),
    }
}

/// Scaled down: GDK silently drops an X11 icon list over 262,144 pixels, and the 512-pixel
/// image alone is just over.
fn set_icon(window: &Window) {
    let loader = PixbufLoader::new();
    let decoded = loader.write(ICON).and_then(|()| loader.close());
    match decoded.map(|()| loader.pixbuf()) {
        Ok(Some(icon)) => {
            let sizes = [256, 64, 32]
                .into_iter()
                .filter_map(|size| icon.scale_simple(size, size, InterpType::Bilinear))
                .collect::<Vec<_>>();
            window.gtk_window().set_icon_list(&sizes);
        }
        Ok(None) => log(format_args!("the app icon decoded to nothing")),
        Err(error) => log(format_args!("could not decode the app icon: {error}")),
    }
}
