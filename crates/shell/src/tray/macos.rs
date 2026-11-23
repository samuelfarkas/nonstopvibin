//! The menu bar item, on the main thread's AppKit loop (`process.rs`), and the daemon
//! thread's handle to it.

use muda::{Menu, MenuId, MenuItem, PredefinedMenuItem};
use nsv_desktop::Command;
use nsv_desktop::pipe::Anchor;
use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use tao::event_loop::EventLoopProxy;
use tokio::sync::mpsc;
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::{Action, Entry, Icon, Look, MENU, rgba};
use crate::log;
use crate::platform::SystemTray;
use crate::process::MainEvent;

/// The daemon thread's line to the menu bar item.
#[derive(Debug)]
pub(crate) struct Link {
    pub(crate) proxy: EventLoopProxy<MainEvent>,
    pub(crate) actions: mpsc::Receiver<Action>,
}

/// The menu bar item: draws the look and reports what the user chose.
pub(crate) struct StatusItem {
    tray: TrayIcon,
    ball: tray_icon::Icon,
    stopped: tray_icon::Icon,
    /// The menu items' ids, in [`MENU`] order.
    ids: Vec<(MenuId, Command)>,
    updates: Option<MenuItem>,
    actions: mpsc::Sender<Action>,
    _menu: Menu,
}

impl StatusItem {
    pub(crate) fn build(actions: mpsc::Sender<Action>) -> Result<Self, String> {
        let icon = |bytes: &[u8]| -> Result<tray_icon::Icon, String> {
            let (pixels, width, height) = rgba(bytes).map_err(|error| error.to_string())?;
            tray_icon::Icon::from_rgba(pixels, width, height).map_err(|error| error.to_string())
        };
        // The @2x images: tray-icon draws them 18 points tall, sharp on Retina screens.
        let ball = icon(include_bytes!("../../../../public/tray/mac-ball@2x.png"))?;
        let stopped = icon(include_bytes!("../../../../public/tray/mac-stopped@2x.png"))?;
        let menu = Menu::new();
        let mut ids = Vec::new();
        let mut updates = None;
        for entry in MENU {
            let appended = match entry {
                Entry::Item(command) => {
                    // The daemon turns "Check for updates…" on once it knows it can update.
                    let item = MenuItem::new(command.label(), command.enabled(false), None);
                    ids.push((item.id().clone(), command));
                    if command == Command::CheckUpdates {
                        updates = Some(item.clone());
                    }
                    menu.append(&item)
                }
                Entry::Separator => menu.append(&PredefinedMenuItem::separator()),
            };
            appended.map_err(|error| error.to_string())?;
        }
        let tray = TrayIconBuilder::new()
            .with_icon(stopped.clone())
            .with_menu(Box::new(menu.clone()))
            .with_menu_on_left_click(false)
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            tray,
            ball,
            stopped,
            ids,
            updates,
            actions,
            _menu: menu,
        })
    }

    pub(crate) fn handle(&self, event: MainEvent) {
        match event {
            MainEvent::Look(look) => {
                let icon = match look.icon {
                    Icon::Ball => self.ball.clone(),
                    Icon::Stopped => self.stopped.clone(),
                };
                if let Err(error) = self.tray.set_icon(Some(icon)) {
                    log(format_args!("could not change the menu bar icon: {error}"));
                }
                if let Err(error) = self.tray.set_tooltip(Some(look.tooltip)) {
                    log(format_args!(
                        "could not change the menu bar tooltip: {error}"
                    ));
                }
            }
            MainEvent::Updates => {
                if let Some(item) = &self.updates {
                    item.set_enabled(true);
                }
            }
            MainEvent::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) => self.send(Action {
                command: Command::ToggleQuota,
                anchor: self.anchor(),
            }),
            MainEvent::Menu(event) => {
                if let Some(&(_, command)) = self.ids.iter().find(|(id, _)| *id == event.id) {
                    let anchor = (command == Command::ToggleQuota)
                        .then(|| self.anchor())
                        .flatten();
                    self.send(Action { command, anchor });
                }
            }
            MainEvent::Tray(_) | MainEvent::Exit(_) => {}
        }
    }

    pub(crate) fn send(&self, action: Action) {
        if self.actions.try_send(action).is_err() {
            log(format_args!(
                "dropped a menu bar click: the shell is busy or gone"
            ));
        }
    }

    /// The item's bounds, converted from Cocoa's bottom-left origin.
    fn anchor(&self) -> Option<Anchor> {
        let mtm = MainThreadMarker::new()?;
        let window = self.tray.ns_status_item()?.button(mtm)?.window()?;
        let frame = window.frame();
        let top = NSScreen::screens(mtm).to_vec().first()?.frame().size.height;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "menu bar frames are whole points well inside i32"
        )]
        Some(Anchor {
            x: frame.origin.x.round() as i32,
            y: (top - frame.origin.y - frame.size.height).round() as i32,
            width: frame.size.width.round() as u32,
            height: frame.size.height.round() as u32,
        })
    }
}

pub(crate) struct Tray {
    proxy: EventLoopProxy<MainEvent>,
    actions: mpsc::Receiver<Action>,
}

impl SystemTray for Tray {
    type Link = Link;

    async fn start(link: Link, look: Look, updates: bool) -> Self {
        let tray = Self {
            proxy: link.proxy,
            actions: link.actions,
        };
        if updates && tray.proxy.send_event(MainEvent::Updates).is_err() {
            log(format_args!("the menu bar item is gone"));
        }
        tray.show(look).await;
        tray
    }

    async fn next(&mut self) -> Option<Action> {
        self.actions.recv().await
    }

    /// The main thread draws it.
    fn show(&self, look: Look) -> impl Future<Output = ()> {
        if self.proxy.send_event(MainEvent::Look(look)).is_err() {
            log(format_args!("the menu bar item is gone"));
        }
        std::future::ready(())
    }
}
