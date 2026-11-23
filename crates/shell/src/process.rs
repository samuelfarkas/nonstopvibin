//! The desktop process's threads: Linux runs the daemon (its tokio runtime, the server and the
//! shell, the tray included) on the calling thread. On macOS `tray-icon` and `muda` need
//! AppKit's loop on the main thread, so `tao` runs it there and the daemon runs on a thread of
//! its own.

use std::process::ExitCode;

use crate::platform::{Os, Threads};
use crate::tray::TrayLink;

/// Runs the desktop app's `daemon` and exits with its code.
pub fn run_desktop(daemon: impl FnOnce(TrayLink) -> u8 + Send + 'static) -> ExitCode {
    Os::run_desktop(daemon)
}

#[cfg(target_os = "linux")]
impl Threads for Os {
    fn run_desktop(daemon: impl FnOnce(TrayLink) -> u8 + Send + 'static) -> ExitCode {
        // The autostart entry passes `--background` instead of a login launch.
        ExitCode::from(daemon(TrayLink {
            login_launch: false,
            tray: crate::tray::Link,
        }))
    }
}

#[cfg(target_os = "macos")]
pub(crate) use macos::MainEvent;

#[cfg(target_os = "macos")]
mod macos {
    use std::panic::{self, AssertUnwindSafe};
    use std::process::ExitCode;

    use muda::MenuEvent;
    use nsv_desktop::Command;
    use objc2_foundation::NSAppleEventManager;
    use tao::event::{Event, StartCause};
    use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
    use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
    use tokio::sync::mpsc;
    use tray_icon::TrayIconEvent;

    use crate::log;
    use crate::platform::{Os, Threads};
    use crate::tray::{Action, Link, Look, StatusItem, TrayLink};

    /// `kAEOpenApplication`, `keyAEPropData` and `keyAELaunchedAsLogInItem`.
    const OPEN_APPLICATION: u32 = u32::from_be_bytes(*b"oapp");
    const PROP_DATA: u32 = u32::from_be_bytes(*b"prdt");
    const LAUNCHED_AS_LOGIN_ITEM: u32 = u32::from_be_bytes(*b"lgit");

    /// What reaches the main thread's loop.
    pub(crate) enum MainEvent {
        Look(Look),
        /// The app can update itself: "Check for updates…" turns on.
        Updates,
        Tray(TrayIconEvent),
        Menu(MenuEvent),
        Exit(i32),
    }

    impl std::fmt::Debug for MainEvent {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("MainEvent")
        }
    }

    impl Threads for Os {
        fn run_desktop(daemon: impl FnOnce(TrayLink) -> u8 + Send + 'static) -> ExitCode {
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
                            login_launch,
                            tray: Link {
                                proxy: proxy.clone(),
                                actions,
                            },
                        };
                        if let Some(daemon) = daemon.take() {
                            start_daemon(daemon, link, proxy.clone());
                        }
                    }
                    Event::UserEvent(MainEvent::Exit(code)) => {
                        *control_flow = ControlFlow::ExitWithCode(code);
                    }
                    // Opening the app while it runs (Finder, Spotlight, `open`): LaunchServices
                    // asks the running daemon to reopen instead of starting a second one.
                    Event::Reopen { .. } => {
                        if let Some(item) = &item {
                            item.send(Action {
                                command: Command::OpenMain,
                                anchor: None,
                            });
                        }
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
}
