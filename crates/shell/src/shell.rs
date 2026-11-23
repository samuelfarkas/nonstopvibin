//! The shell's loop: tray clicks, the helper's messages, the daemon's status and events. It
//! owns at most one helper and starts one only when a window must show.

use std::path::PathBuf;
use std::time::Instant;

use nsv_domain::helper::{
    DaemonCall, DialogKind, FromHelper, Outcome, Rect, ToHelper, View, ViewKind,
};
use nsv_store::layout::database_path;
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;

use crate::helper::{Event, Helper};
use crate::login::{Login, UNPACKAGED};
use crate::tray::{Action, Look, Popup, Toggle, Tray, TrayLink};
use crate::{HostEvent, ShellHost, external, log};

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

/// Runs the tray and the windows until Quit, or until the daemon's event channel closes.
/// Dropping the future closes the helper's stdin, and the helper exits with its windows.
pub async fn run(host: &impl ShellHost, options: Options, link: TrayLink) {
    let open_window = options.open_window && !link.login_launch();
    let mut status = host.tray_status();
    let mut events = host.events();
    let look = Look::of(&status.borrow_and_update());
    let mut shell = Shell {
        host,
        tray: Tray::start(link, look.clone()).await,
        tray_alive: true,
        look,
        options,
        helper: None,
        main: false,
        popup: Popup::Closed { at: None },
        dialogs: Vec::new(),
        bounds: None,
        anchor: Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
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
        };
        if step == Step::Stop {
            break;
        }
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
    look: Look,
    options: Options,
    helper: Option<Helper>,
    /// What the helper shows, as its messages tell.
    main: bool,
    popup: Popup,
    dialogs: Vec<DialogKind>,
    /// Where the main window was when it last closed.
    bounds: Option<Rect>,
    /// The tray icon's last known bounds, for the popup.
    anchor: Rect,
}

impl<H: ShellHost> Shell<'_, H> {
    async fn redraw(&mut self, look: Look) {
        if look != self.look {
            self.look = look.clone();
            self.tray.show(look).await;
        }
    }

    async fn tray_action(&mut self, action: Action) -> Step {
        match action {
            Action::ToggleQuota(anchor) => {
                if let Some(anchor) = anchor {
                    self.anchor = anchor;
                }
                self.toggle_quota().await;
            }
            Action::OpenMain => self.show(View::Main { profile: None }).await,
            Action::Quit => return self.quit().await,
        }
        Step::Continue
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
            // It is exiting, and takes its windows along.
            self.helper = None;
        }
        self.forget_windows();
        self.opened(&view);
        let start = ToHelper::Start {
            origin: self.host.origin().clone(),
            view: view.clone(),
            bounds: self.bounds,
            data_directory: self.host.data_directory().to_owned(),
        };
        match Helper::spawn(&self.options.helper, &start, self.host.session_token()).await {
            Ok(helper) => self.helper = Some(helper),
            Err(error) => {
                log(format_args!("could not start the window helper: {error}"));
                self.fall_back();
                self.forget_windows();
            }
        }
    }

    fn opened(&mut self, view: &View) {
        match view {
            View::Main { .. } => self.main = true,
            View::Quota { .. } => self.popup = Popup::Open,
            View::Dialog { kind } => {
                if !self.dialogs.contains(kind) {
                    self.dialogs.push(*kind);
                }
            }
        }
    }

    /// Without a helper, the main window and the quota popup become the session link in the
    /// system browser.
    fn fall_back(&self) {
        if !self.main && self.popup != Popup::Open {
            return;
        }
        let link = format!(
            "{}/#session={}",
            self.host.origin().as_str(),
            self.host.session_token().expose_secret()
        );
        if let Err(error) = external::open(&link) {
            log(format_args!("could not open the browser either: {error}"));
        }
    }

    fn forget_windows(&mut self) {
        self.main = false;
        self.popup = Popup::Closed { at: None };
        self.dialogs.clear();
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
                let Some(helper) = self.helper.take() else {
                    return Step::Continue;
                };
                if !helper.ready {
                    log(format_args!(
                        "the window helper could not start ({status:?})"
                    ));
                    self.fall_back();
                } else if !status.is_some_and(|status| status.success()) {
                    log(format_args!("the window helper stopped ({status:?})"));
                }
                self.forget_windows();
            }
        }
        Step::Continue
    }

    async fn message(&mut self, message: FromHelper) -> Step {
        match message {
            FromHelper::Ready => {
                if let Some(helper) = &mut self.helper {
                    helper.ready = true;
                }
            }
            FromHelper::Closed { view, bounds } => {
                match view {
                    ViewKind::Main => {
                        self.main = false;
                        self.bounds = bounds.or(self.bounds);
                    }
                    ViewKind::Quota => {
                        self.popup = Popup::Closed {
                            at: Some(Instant::now()),
                        };
                    }
                    ViewKind::Dialog { kind } => self.dialogs.retain(|open| *open != kind),
                }
                if !self.main && self.popup != Popup::Open && self.dialogs.is_empty() {
                    // The helper exits with its last window: the next view needs a new one.
                    self.helper = None;
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
                if let Err(message) =
                    external::safe_external(&url).and_then(|url| external::open(&url))
                {
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
            FromHelper::ToggleQuota => self.toggle_quota().await,
            FromHelper::QuitApp => return self.quit().await,
            // The app updater answers these.
            FromHelper::CheckUpdates | FromHelper::RestartToUpdate => {}
        }
        Step::Continue
    }

    /// The bridge methods the helper forwards, each argument checked again here.
    async fn bridge(&self, call: DaemonCall) -> Result<Value, String> {
        match call {
            DaemonCall::OpenExternal(url) => {
                external::open(&external::safe_external(&url)?)?;
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
                external::reveal(&database_path(self.host.data_directory())).await?;
                Ok(Value::Null)
            }
        }
    }
}
