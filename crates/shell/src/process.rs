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
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
    use std::time::Duration;

    use crate::platform::{Os, Threads};
    use crate::tray::{Action, Link, Look, StatusItem, TrayLink};
    use muda::MenuEvent;
    use nsv_desktop::Command;
    use objc2_foundation::NSAppleEventManager;
    use tao::event::{Event, StartCause};
    use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
    use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
    use tokio::sync::mpsc;
    use tokio::sync::mpsc::error::TrySendError;
    use tray_icon::TrayIconEvent;

    /// `kAEOpenApplication`, `keyAEPropData` and `keyAELaunchedAsLogInItem`.
    const OPEN_APPLICATION: u32 = u32::from_be_bytes(*b"oapp");
    const PROP_DATA: u32 = u32::from_be_bytes(*b"prdt");
    const LAUNCHED_AS_LOGIN_ITEM: u32 = u32::from_be_bytes(*b"lgit");
    /// How long a quit from outside the app waits for the daemon to close: open requests
    /// finish, usage drains and the cores stop, with a 5 s kill grace.
    const QUIT_WAIT: Duration = Duration::from_secs(30);

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
            // A quit from outside the app asks the daemon over the status item's channel and
            // waits for the daemon thread's exit code.
            let mut outside: Option<(mpsc::Sender<Action>, Receiver<u8>)> = None;
            // The daemon has finished, and the loop ends itself.
            let mut exiting = false;
            event_loop.run(move |event, _target, control_flow| {
                *control_flow = ControlFlow::Wait;
                match event {
                    Event::NewEvents(StartCause::Init) => {
                        // The open event is current only while the app finishes launching.
                        let login_launch = launched_at_login();
                        let (sender, actions) = mpsc::channel(8);
                        let (finished, code) = sync_channel(1);
                        outside = Some((sender.clone(), code));
                        item = StatusItem::build(sender)
                            .inspect_err(|error| tracing::warn!("no menu bar item: {error}"))
                            .ok();
                        let link = TrayLink {
                            login_launch,
                            tray: Link {
                                proxy: proxy.clone(),
                                actions,
                            },
                        };
                        if let Some(daemon) = daemon.take() {
                            start_daemon(daemon, link, &proxy, finished);
                        }
                    }
                    Event::UserEvent(MainEvent::Exit(code)) => {
                        exiting = true;
                        *control_flow = ControlFlow::ExitWithCode(code);
                    }
                    // AppKit ends the loop for a quit from outside the app: logout, restart,
                    // shutdown, or a quit Apple Event (`osascript`, Activity Monitor's Quit).
                    Event::LoopDestroyed if !exiting => {
                        if let Some((actions, code)) = &outside {
                            quit_from_outside(actions, code);
                        }
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
        proxy: &EventLoopProxy<MainEvent>,
        finished: SyncSender<u8>,
    ) {
        let exit = proxy.clone();
        let spawned = std::thread::Builder::new()
            .name("daemon".to_owned())
            .spawn(move || {
                let code = panic::catch_unwind(AssertUnwindSafe(|| daemon(link))).unwrap_or(1);
                // A main thread inside `quit_from_outside` reads no loop events. The receiver
                // lives as long as the loop, and its one slot is free.
                let _ = finished.send(code);
                drop(exit.send_event(MainEvent::Exit(i32::from(code))));
            });
        if let Err(error) = spawned {
            tracing::warn!("could not start the daemon thread: {error}");
            // Without the daemon nothing would end the loop: the menu bar item's Quit goes to it.
            drop(proxy.send_event(MainEvent::Exit(1)));
        }
    }

    /// AppKit exits the process with 0 once the loop's handler returns. First the daemon quits
    /// as the menu's Quit makes it (`Shell::quit`, then `Daemon::close`), and the process exits
    /// with its code once it has closed, or after [`QUIT_WAIT`].
    #[expect(
        clippy::exit,
        reason = "AppKit's exit would report a failed close as success"
    )]
    fn quit_from_outside(actions: &mpsc::Sender<Action>, code: &Receiver<u8>) {
        let quit = Action {
            command: Command::Quit,
            anchor: None,
        };
        // A closed channel means the shell has ended: the daemon is closing already.
        if let Err(TrySendError::Full(_)) = actions.try_send(quit) {
            tracing::warn!("could not ask the daemon to quit: the shell is busy");
        }
        match code.recv_timeout(QUIT_WAIT) {
            Ok(code) => std::process::exit(i32::from(code)),
            Err(error) => tracing::warn!("the daemon did not finish quitting: {error}"),
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
