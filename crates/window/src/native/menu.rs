//! main.ts's application menu (`Menu.setApplicationMenu`), one item list for both platforms:
//! macOS shows it in the menu bar, Linux in the main window, as Electron did. Its items send the
//! daemon the tray's commands or act on the main window; events arrive as `UserEvent::Menu`.

#[cfg(target_os = "macos")]
use muda::AboutMetadata;
use muda::accelerator::{Accelerator, CMD_OR_CTRL, Code, Modifiers};
use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use nsv_desktop::Command;
use wry::WebView;

use crate::app::{Proxy, UserEvent, log, post};

/// What an item asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// A command the daemon carries out, as from the tray.
    Command(Command),
    Zoom(Step),
    /// muda's GTK menu has no working window items of its own.
    #[cfg(target_os = "linux")]
    Window(WindowItem),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Reset,
    In,
    Out,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowItem {
    FullScreen,
    Minimize,
    Close,
}

/// The menu, and what each of its own items asks for.
pub(crate) struct AppMenu {
    pub(crate) menu: Menu,
    /// The Window menu, which AppKit fills with the app's windows.
    #[cfg(target_os = "macos")]
    pub(crate) window: Submenu,
    actions: Vec<(MenuItem, Action)>,
}

impl AppMenu {
    /// What the item `id` asks for, when it is one of this menu's.
    pub(crate) fn action(&self, id: &MenuId) -> Option<Action> {
        self.actions
            .iter()
            .find(|(item, _)| item.id() == id)
            .map(|&(_, action)| action)
    }
}

/// A dropped GTK menu destroys its submenus before it removes their items' shortcuts, and GTK
/// warns for each one it no longer finds; with the shortcuts removed first there are none.
#[cfg(target_os = "linux")]
impl Drop for AppMenu {
    fn drop(&mut self) {
        for (item, _) in &self.actions {
            if let Err(error) = item.set_accelerator(None) {
                log(format_args!("could not remove a menu shortcut: {error}"));
            }
        }
    }
}

/// Electron's zoom level: each step of 0.5 scales the page by 1.2^0.5.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Zoom(f64);

impl Zoom {
    pub(crate) fn scale(self) -> f64 {
        1.2_f64.powf(self.0)
    }

    /// Takes `step` and zooms the main window's page, when there is one, to the new level.
    pub(crate) fn apply(&mut self, step: Step, main: Option<&WebView>) {
        self.0 = match step {
            Step::Reset => 0.0,
            Step::In => (self.0 + 0.5).min(9.0),
            Step::Out => (self.0 - 0.5).max(-8.0),
        };
        if let Some(main) = main
            && let Err(error) = main.zoom(self.scale())
        {
            log(format_args!("could not zoom: {error}"));
        }
    }
}

/// Sends every menu event to the loop.
pub(crate) fn listen(proxy: &Proxy) {
    let proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        post(&proxy, UserEvent::Menu(event.id));
    }));
}

/// The items that act, as they are made.
struct Items {
    /// The app can update itself.
    updates: bool,
    actions: Vec<(MenuItem, Action)>,
}

impl Items {
    fn item(&mut self, action: Action, label: &str, modifiers: Modifiers, key: Code) -> MenuItem {
        let item = MenuItem::new(label, true, Some(Accelerator::new(modifiers, key)));
        self.actions.push((item.clone(), action));
        item
    }

    /// A command's item, labelled and enabled as in the tray.
    fn command(&mut self, command: Command, modifiers: Modifiers, key: Option<Code>) -> MenuItem {
        let item = MenuItem::new(
            command.app_menu_label(),
            command.enabled(self.updates),
            key.map(|key| Accelerator::new(modifiers, key)),
        );
        self.actions.push((item.clone(), Action::Command(command)));
        item
    }
}

/// Electron's application menu (`desktop/main.ts`). "Check for updates…" is enabled when the
/// app can update itself (`updates`).
pub(crate) fn build(updates: bool) -> muda::Result<AppMenu> {
    let mut items = Items {
        updates,
        actions: Vec::new(),
    };
    let command = CMD_OR_CTRL;
    let app = Submenu::new("NonstopVibin", true);
    #[cfg(target_os = "macos")]
    app.append(&PredefinedMenuItem::about(
        None,
        Some(AboutMetadata {
            name: Some("NonstopVibin".to_owned()),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            ..AboutMetadata::default()
        }),
    ))?;
    app.append(&items.command(Command::CheckUpdates, command, None))?;
    #[cfg(target_os = "macos")]
    app.append_items(&[
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::hide(None),
        &PredefinedMenuItem::hide_others(None),
        &PredefinedMenuItem::show_all(None),
    ])?;
    app.append_items(&[
        &PredefinedMenuItem::separator(),
        &items.command(Command::Quit, command, Some(Code::KeyQ)),
    ])?;
    let edit = Submenu::new("Edit", true);
    #[cfg(target_os = "macos")]
    edit.append_items(&[
        &PredefinedMenuItem::undo(None),
        &PredefinedMenuItem::redo(None),
        &PredefinedMenuItem::separator(),
    ])?;
    edit.append_items(&[
        &PredefinedMenuItem::cut(None),
        &PredefinedMenuItem::copy(None),
        &PredefinedMenuItem::paste(None),
        &PredefinedMenuItem::select_all(None),
    ])?;
    let view = Submenu::with_items(
        "View",
        true,
        &[
            &items.command(
                Command::ToggleQuota,
                command | Modifiers::SHIFT,
                Some(Code::KeyU),
            ),
            &items.command(Command::OpenMain, command, Some(Code::Digit1)),
            &PredefinedMenuItem::separator(),
            &items.item(
                Action::Zoom(Step::Reset),
                "Actual Size",
                command,
                Code::Digit0,
            ),
            &items.item(Action::Zoom(Step::In), "Zoom In", command, Code::Equal),
            &items.item(Action::Zoom(Step::Out), "Zoom Out", command, Code::Minus),
            &PredefinedMenuItem::separator(),
        ],
    )?;
    #[cfg(target_os = "macos")]
    view.append(&PredefinedMenuItem::fullscreen(None))?;
    #[cfg(target_os = "linux")]
    view.append(&items.item(
        Action::Window(WindowItem::FullScreen),
        "Toggle Full Screen",
        Modifiers::empty(),
        Code::F11,
    ))?;
    let window = Submenu::new("Window", true);
    #[cfg(target_os = "macos")]
    window.append_items(&[
        &PredefinedMenuItem::minimize(None),
        &PredefinedMenuItem::maximize(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::bring_all_to_front(None),
    ])?;
    #[cfg(target_os = "linux")]
    window.append_items(&[
        &items.item(
            Action::Window(WindowItem::Minimize),
            "Minimize",
            command,
            Code::KeyM,
        ),
        &items.item(
            Action::Window(WindowItem::Close),
            "Close",
            command,
            Code::KeyW,
        ),
    ])?;
    Ok(AppMenu {
        menu: Menu::with_items(&[&app, &edit, &view, &window])?,
        #[cfg(target_os = "macos")]
        window,
        actions: items.actions,
    })
}
