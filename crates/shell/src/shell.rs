//! The shell's loop: tray clicks, the helper's messages, the daemon's status and events. It
//! owns at most one helper and starts one only when a window must show.

use std::path::PathBuf;
use std::time::Instant;

use nsv_desktop::pipe::{
    Anchor, DaemonCall, DialogKind, FromHelper, Outcome, Rect, Start, ToHelper, View, ViewKind,
};
use nsv_desktop::{Command, HostEvent};
use nsv_domain::endpoint::session_link;
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;

use crate::helper::{Event, Helper};
use crate::login::{Login, UNPACKAGED};
use crate::tray::{Action, Look, Popup, Toggle, Tray, TrayLink};
use crate::update::{Staged, Updates};
use crate::{ShellHost, UpdateFailed, external, log};

#[derive(Debug, Clone)]
pub struct Options {
    /// `nonstopvibin-window`: [`crate::helper_beside_executable`] in a build.
    pub helper: PathBuf,
    /// Opens the main window once ready. `--background` and a macOS login launch start without.
    pub open_window: bool,
    /// An installed build. A development run reads the login item but may not change it.
    pub packaged: bool,
    pub login: Login,
}

/// Runs the tray, the windows and app updates until Quit, until the daemon's event channel
/// closes, or until "Restart to update" has stopped the daemon and started the update.
/// Dropping the future closes the helper's stdin, and the helper exits with its windows.
///
/// # Errors
/// [`UpdateFailed`] when the daemon stopped for an update that could not be installed; the
/// process should exit with a failure, as Electron's did.
pub async fn run(
    host: &impl ShellHost,
    options: Options,
    link: TrayLink,
) -> Result<(), UpdateFailed> {
    let open_window = options.open_window && !link.login_launch();
    let mut status = host.tray_status();
    let mut events = host.events();
    let look = Look::of(&status.borrow_and_update());
    let updates = Updates::detect(options.packaged);
    let mut shell = Shell {
        host,
        tray: Tray::start(link, look.clone(), updates.possible()).await,
        updates,
        failed: false,
        tray_alive: true,
        look,
        options,
        helper: None,
        main: false,
        popup: Popup::Closed { at: None },
        bounds: None,
        anchor: Anchor::point(0, 0),
    };
    if shell.options.packaged
        && let Err(error) = shell.options.login.follow()
    {
        log(format_args!("could not update the login item: {error}"));
    }
    if open_window {
        shell.show(View::Main { profile: None }).await;
    }
    let mut status_alive = true;
    loop {
        // Biased: a helper's `Closed` that is already queued counts before a click on the tray.
        let step = tokio::select! {
            biased;
            event = next_event(&mut shell.helper) => shell.helper_event(event).await,
            action = shell.tray.next(), if shell.tray_alive => {
                if let Some(action) = action {
                    shell.tray_action(action).await
                } else {
                    shell.tray_alive = false;
                    Step::Continue
                }
            }
            changed = status.changed(), if status_alive => {
                if changed.is_ok() {
                    let look = Look::of(&status.borrow_and_update());
                    shell.redraw(look).await;
                } else {
                    status_alive = false;
                }
                Step::Continue
            }
            event = events.recv() => match event {
                Ok(event) => shell.host_event(event).await,
                Err(RecvError::Lagged(_)) => Step::Continue,
                Err(RecvError::Closed) => Step::Stop,
            },
            dialog = shell.updates.next() => {
                if let Some(kind) = dialog {
                    shell.show(View::Dialog { kind }).await;
                }
                Step::Continue
            }
        };
        if step == Step::Stop {
            break;
        }
    }
    if shell.failed {
        Err(UpdateFailed)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Continue,
    Stop,
}

async fn next_event(helper: &mut Option<Helper>) -> Event {
    match helper {
        Some(helper) => helper.next().await,
        None => std::future::pending().await,
    }
}

struct Shell<'a, H> {
    host: &'a H,
    tray: Tray,
    tray_alive: bool,
    updates: Updates,
    /// An update stopped the daemon and could not be installed.
    failed: bool,
    look: Look,
    options: Options,
    helper: Option<Helper>,
    /// Whether the main window is open, for a sign-in that brings it forward.
    main: bool,
    popup: Popup,
    /// Where the main window was when it last closed.
    bounds: Option<Rect>,
    /// Where the tray icon was last, for the popup.
    anchor: Anchor,
}

impl<H: ShellHost> Shell<'_, H> {
    async fn redraw(&mut self, look: Look) {
        if look != self.look {
            self.look = look.clone();
            self.tray.show(look).await;
        }
    }

    async fn tray_action(&mut self, action: Action) -> Step {
        if let Some(anchor) = action.anchor {
            self.anchor = anchor;
        }
        self.command(action.command).await
    }

    /// A command from the tray or the macOS app menu.
    async fn command(&mut self, command: Command) -> Step {
        match command {
            Command::OpenMain => self.show(View::Main { profile: None }).await,
            Command::ToggleQuota => self.toggle_quota().await,
            Command::CheckUpdates => {
                if let Some(kind) = self.updates.check(true) {
                    self.show(View::Dialog { kind }).await;
                }
            }
            Command::Quit => return self.quit().await,
        }
        Step::Continue
    }

    /// "Restart to update": closes the windows, stops the daemon as Quit would (open requests
    /// finish, usage drains, cores stop), then swaps the update in and starts it. A failure
    /// shows main.ts's error and ends the shell with [`UpdateFailed`].
    async fn install(&mut self, update: Staged) -> Step {
        // Open windows would show "Failed to fetch" once the local service stops.
        self.send(&ToHelper::Quit).await;
        self.helper_gone(None);
        let failure = match self.host.close().await {
            Err(error) => {
                log(format_args!("could not stop for the update: {error}"));
                DialogKind::CouldNotRestart
            }
            Ok(()) => match update.install() {
                Ok(()) => return Step::Stop,
                Err(error) => {
                    log(format_args!("could not install the update: {error}"));
                    DialogKind::CouldNotInstall
                }
            },
        };
        self.failed = true;
        self.show(View::Dialog { kind: failure }).await;
        // Electron's error box held the exit until it was dismissed.
        while let Some(helper) = &mut self.helper {
            match helper.next().await {
                Event::Message(FromHelper::Closed { .. }) | Event::Exited(_) => break,
                Event::Message(_) => {}
            }
        }
        Step::Stop
    }

    async fn host_event(&mut self, event: HostEvent) -> Step {
        match event {
            // Only an existing window holds the sign-in dialog; a new one would not.
            HostEvent::SignInAccepted if self.main => self.show(View::Main { profile: None }).await,
            HostEvent::SignInAccepted => {}
            HostEvent::ShowMain => self.show(View::Main { profile: None }).await,
        }
        Step::Continue
    }

    async fn toggle_quota(&mut self) {
        match self.popup.toggle(Instant::now()) {
            Toggle::Open => {
                self.show(View::Quota {
                    anchor: self.anchor,
                })
                .await;
            }
            Toggle::Close => self.send(&ToHelper::CloseQuota).await,
            Toggle::Ignore => {}
        }
    }

    /// Shows `view` in the running helper, or starts one for it.
    async fn show(&mut self, view: View) {
        if let Some(helper) = &mut self.helper {
            if helper
                .send(&ToHelper::Show { view: view.clone() })
                .await
                .is_ok()
            {
                self.opened(&view);
                return;
            }
            // It is exiting, and takes its windows along, but never had this one.
            self.helper_gone(match view {
                View::Dialog { kind } => Some(kind),
                _ => None,
            });
        }
        self.opened(&view);
        let start = Start {
            origin: self.host.origin().clone(),
            view: view.clone(),
            bounds: self.bounds,
            webview: self.host.data_directory().webview(),
            updates: self.updates.possible(),
        };
        match Helper::spawn(&self.options.helper, start, self.host.session_token()).await {
            Ok(helper) => self.helper = Some(helper),
            Err(error) => {
                log(format_args!("could not start the window helper: {error}"));
                self.fall_back();
                self.helper_gone(None);
            }
        }
    }

    fn opened(&mut self, view: &View) {
        match view {
            View::Main { .. } => self.main = true,
            View::Quota { .. } => self.popup = Popup::Open,
            View::Dialog { .. } => {}
        }
    }

    /// The helper has exited or is exiting, and its windows with it. `showing` is a dialog
    /// that goes to the next helper instead.
    fn helper_gone(&mut self, showing: Option<DialogKind>) {
        self.helper = None;
        self.main = false;
        if self.popup == Popup::Open {
            self.popup = Popup::Closed { at: None };
        }
        self.updates.helper_gone(showing);
    }

    /// Without a helper, the main window and the quota popup become the session link in the
    /// system browser.
    fn fall_back(&self) {
        if !self.main && self.popup != Popup::Open {
            return;
        }
        let link = session_link(self.host.origin().as_str(), self.host.session_token());
        if let Err(error) = external::open(&link) {
            log(format_args!("could not open the browser either: {error}"));
        }
    }

    async fn send(&mut self, message: &ToHelper) {
        if let Some(helper) = &mut self.helper
            && let Err(error) = helper.send(message).await
        {
            log(format_args!("the window helper stopped listening: {error}"));
        }
    }

    async fn quit(&mut self) -> Step {
        self.send(&ToHelper::Quit).await;
        self.helper = None;
        self.host.shutdown();
        Step::Stop
    }

    async fn helper_event(&mut self, event: Event) -> Step {
        match event {
            Event::Message(message) => return self.message(message).await,
            Event::Exited(status) => {
                let Some(helper) = &self.helper else {
                    return Step::Continue;
                };
                if !helper.ready() {
                    log(format_args!(
                        "the window helper could not start ({status:?})"
                    ));
                    self.fall_back();
                } else if !status.is_some_and(|status| status.success()) {
                    log(format_args!("the window helper stopped ({status:?})"));
                }
                self.helper_gone(None);
            }
        }
        Step::Continue
    }

    async fn message(&mut self, message: FromHelper) -> Step {
        match message {
            // The helper notes it.
            FromHelper::Ready => {}
            FromHelper::Closed { view, bounds, last } => {
                let update = match view {
                    ViewKind::Main => {
                        self.main = false;
                        self.bounds = bounds.or(self.bounds);
                        None
                    }
                    ViewKind::Quota => {
                        self.popup = Popup::Closed {
                            at: Some(Instant::now()),
                        };
                        None
                    }
                    ViewKind::Dialog { kind, choice } => self.updates.closed(kind, choice),
                };
                if last {
                    // The next view needs a new helper.
                    self.helper_gone(None);
                }
                if let Some(update) = update {
                    return self.install(update).await;
                }
            }
            FromHelper::Bridge { id, call } => {
                let outcome = match self.bridge(call).await {
                    Ok(value) => Outcome::Result(value),
                    Err(message) => Outcome::Error(message),
                };
                self.send(&ToHelper::Reply { id, outcome }).await;
            }
            FromHelper::OpenExternal { url } => {
                if let Err(message) = external::open_approved(&url) {
                    self.host.report(&message);
                }
            }
            FromHelper::OpenMain { profile } => {
                if profile
                    .as_ref()
                    .is_some_and(|profile| !self.host.has_profile(profile))
                {
                    self.host.report("Profile not found.");
                } else {
                    self.show(View::Main { profile }).await;
                }
            }
            FromHelper::Command { command } => return self.command(command).await,
        }
        Step::Continue
    }

    /// The bridge methods the helper forwards, each argument checked again here.
    async fn bridge(&self, call: DaemonCall) -> Result<Value, String> {
        match call {
            DaemonCall::OpenExternal(url) => {
                external::open_approved(&url)?;
                Ok(Value::Null)
            }
            DaemonCall::GetLoginItem => Ok(Value::Bool(self.options.login.enabled())),
            DaemonCall::SetLoginItem(enabled) => {
                if !self.options.packaged {
                    return Err(UNPACKAGED.to_owned());
                }
                self.options.login.set(enabled).map(Value::Bool)
            }
            DaemonCall::RevealData => {
                external::reveal(&self.host.data_directory().database()).await?;
                Ok(Value::Null)
            }
        }
    }
}
