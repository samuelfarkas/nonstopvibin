//! The menu bar item: `tray-icon` and `muda` need AppKit's loop on the main thread, so `tao`
//! runs it there and the daemon (its tokio runtime, the server and the shell) runs on a thread
//! of its own.

use std::panic::{self, AssertUnwindSafe};

use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use nsv_domain::helper::Rect;
use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_foundation::NSAppleEventManager;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tokio::sync::mpsc;
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::{Action, Entry, Icon, Look, MENU, rgba};
use crate::log;

/// `kAEOpenApplication`, `keyAEPropData` and `keyAELaunchedAsLogInItem`.
const OPEN_APPLICATION: u32 = u32::from_be_bytes(*b"oapp");
const PROP_DATA: u32 = u32::from_be_bytes(*b"prdt");
const LAUNCHED_AS_LOGIN_ITEM: u32 = u32::from_be_bytes(*b"lgit");

enum MainEvent {
    Look(Look),
    Tray(TrayIconEvent),
    Menu(MenuEvent),
    Exit(i32),
}

/// The daemon thread's line to the menu bar item.
#[derive(Debug)]
pub struct TrayLink {
    proxy: EventLoopProxy<MainEvent>,
    actions: mpsc::Receiver<Action>,
    login_launch: bool,
}

impl TrayLink {
    /// Opened by the login item: the daemon starts without a window, as with `--background`.
    pub(crate) fn login_launch(&self) -> bool {
        self.login_launch
    }
}

impl std::fmt::Debug for MainEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MainEvent")
    }
}

/// Runs AppKit's loop on this, the main, thread and `daemon` on another, then exits with the
/// daemon's code.
pub fn run_desktop(daemon: impl FnOnce(TrayLink) -> u8 + Send + 'static) -> ! {
    let mut event_loop = EventLoopBuilder::<MainEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    let proxy = event_loop.create_proxy();
    let forward = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        drop(forward.send_event(MainEvent::Tray(event)));
    }));
    let forward = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        drop(forward.send_event(MainEvent::Menu(event)));
    }));
    let mut daemon = Some(daemon);
    let mut item: Option<StatusItem> = None;
    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                // The open event is current only while the app finishes launching.
                let login_launch = launched_at_login();
                let (sender, actions) = mpsc::channel(8);
                item = StatusItem::build(sender)
                    .inspect_err(|error| log(format_args!("no menu bar item: {error}")))
                    .ok();
                let link = TrayLink {
                    proxy: proxy.clone(),
                    actions,
                    login_launch,
                };
                if let Some(daemon) = daemon.take() {
                    start_daemon(daemon, link, proxy.clone());
                }
            }
            Event::UserEvent(MainEvent::Exit(code)) => {
                *control_flow = ControlFlow::ExitWithCode(code);
            }
            Event::UserEvent(event) => {
                if let Some(item) = &item {
                    item.handle(event);
                }
            }
            _ => {}
        }
    })
}

fn start_daemon(
    daemon: impl FnOnce(TrayLink) -> u8 + Send + 'static,
    link: TrayLink,
    proxy: EventLoopProxy<MainEvent>,
) {
    let spawned = std::thread::Builder::new()
        .name("daemon".to_owned())
        .spawn(move || {
            let code = panic::catch_unwind(AssertUnwindSafe(|| daemon(link))).unwrap_or(1);
            drop(proxy.send_event(MainEvent::Exit(i32::from(code))));
        });
    if let Err(error) = spawned {
        log(format_args!("could not start the daemon thread: {error}"));
    }
}

/// Whether macOS opened the app as a login item: Electron's `wasOpenedAtLogin`.
fn launched_at_login() -> bool {
    let Some(event) = NSAppleEventManager::sharedAppleEventManager().currentAppleEvent() else {
        return false;
    };
    event.eventID() == OPEN_APPLICATION
        && event
            .paramDescriptorForKeyword(PROP_DATA)
            .is_some_and(|data| data.enumCodeValue() == LAUNCHED_AS_LOGIN_ITEM)
}

struct StatusItem {
    tray: TrayIcon,
    ball: tray_icon::Icon,
    stopped: tray_icon::Icon,
    /// The menu items' ids, in [`MENU`] order.
    ids: Vec<(MenuId, Action)>,
    actions: mpsc::Sender<Action>,
    _menu: Menu,
}

impl StatusItem {
    fn build(actions: mpsc::Sender<Action>) -> Result<Self, String> {
        let icon = |bytes: &[u8]| -> Result<tray_icon::Icon, String> {
            let (pixels, width, height) = rgba(bytes).map_err(|error| error.to_string())?;
            tray_icon::Icon::from_rgba(pixels, width, height).map_err(|error| error.to_string())
        };
        // The @2x images: tray-icon draws them 18 points tall, sharp on Retina screens.
        let ball = icon(include_bytes!("../../../../public/tray/mac-ball@2x.png"))?;
        let stopped = icon(include_bytes!("../../../../public/tray/mac-stopped@2x.png"))?;
        let menu = Menu::new();
        let mut ids = Vec::new();
        for entry in MENU {
            let appended = match entry {
                Entry::Item { label, action } => {
                    let item = MenuItem::new(label, action.is_some(), None);
                    if let Some(action) = action {
                        ids.push((item.id().clone(), action));
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
            actions,
            _menu: menu,
        })
    }

    fn handle(&self, event: MainEvent) {
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
            MainEvent::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) => self.send(Action::ToggleQuota(self.anchor())),
            MainEvent::Menu(event) => {
                let chosen = self.ids.iter().find(|(id, _)| *id == event.id);
                match chosen {
                    Some((_, Action::ToggleQuota(_))) => {
                        self.send(Action::ToggleQuota(self.anchor()));
                    }
                    Some((_, action)) => self.send(*action),
                    None => {}
                }
            }
            MainEvent::Tray(_) | MainEvent::Exit(_) => {}
        }
    }

    fn send(&self, action: Action) {
        if self.actions.try_send(action).is_err() {
            log(format_args!(
                "dropped a menu bar click: the shell is busy or gone"
            ));
        }
    }

    /// The item's bounds in points, with the origin at the top left of the main screen.
    fn anchor(&self) -> Option<Rect> {
        let mtm = MainThreadMarker::new()?;
        let window = self.tray.ns_status_item()?.button(mtm)?.window()?;
        let frame = window.frame();
        let top = NSScreen::screens(mtm).to_vec().first()?.frame().size.height;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "menu bar frames are whole points well inside i32"
        )]
        Some(Rect {
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

impl Tray {
    pub(crate) async fn start(link: TrayLink, look: Look) -> Self {
        let tray = Self {
            proxy: link.proxy,
            actions: link.actions,
        };
        tray.show(look).await;
        tray
    }

    pub(crate) async fn next(&mut self) -> Option<Action> {
        self.actions.recv().await
    }

    /// Ready at once: the main thread draws it. The Linux twin awaits the D-Bus service.
    pub(crate) fn show(&self, look: Look) -> std::future::Ready<()> {
        if self.proxy.send_event(MainEvent::Look(look)).is_err() {
            log(format_args!("the menu bar item is gone"));
        }
        std::future::ready(())
    }
}
