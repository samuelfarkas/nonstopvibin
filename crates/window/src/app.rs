//! The event loop. The main thread owns every window, webview and piece of state; the daemon
//! pipe, the webviews and the native dialogs reach it as `UserEvent`s.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::PathBuf;

use nsv_domain::helper::{
    self, DialogKind, FromHelper, Origin, Outcome, Rect, ToHelper, View, ViewKind,
};
use nsv_domain::id::ProfileId;
use nsv_domain::secret::SessionToken;
use serde_json::{Value, json};
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder, WindowId};
use wry::{
    NewWindowResponse, PageLoadEvent, PermissionResponse, WebContext, WebView, WebViewBuilder,
};

use crate::bridge::{self, Call, Rejected};
use crate::{layout, native};

const BACKGROUND: (u8, u8, u8, u8) = (0x1c, 0x1b, 0x18, 0xff);

pub(crate) type Proxy = EventLoopProxy<UserEvent>;
type Target = EventLoopWindowTarget<UserEvent>;

pub(crate) enum UserEvent {
    Daemon(ToHelper),
    /// The daemon closed the pipe or broke the protocol.
    DaemonGone,
    Ipc {
        pane: Pane,
        url: String,
        body: String,
    },
    PageLoad {
        pane: Pane,
        finished: bool,
    },
    NewWindow(String),
    Unload {
        vetoed: bool,
    },
    FolderChosen {
        ticket: Ticket,
        result: Result<Option<String>, String>,
    },
    DialogAnswered {
        kind: DialogKind,
        restart: bool,
    },
    #[cfg(target_os = "macos")]
    Menu(muda::MenuId),
}

/// The helper's two webview windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pane {
    Main,
    Quota,
}

/// Where an answer goes: call `id` of one page load in one pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ticket {
    pane: Pane,
    load: u64,
    id: u64,
}

pub(crate) fn post(proxy: &Proxy, event: UserEvent) {
    // Fails only once the loop has ended and the process is exiting.
    let _ = proxy.send_event(event);
}

#[expect(
    clippy::print_stderr,
    reason = "stderr is the helper's log, which the daemon keeps"
)]
pub(crate) fn log(message: fmt::Arguments<'_>) {
    eprintln!("nonstopvibin-window: {message}");
}

/// What `Start` and the token line set up.
pub(crate) struct Boot {
    pub(crate) origin: Origin,
    pub(crate) token: SessionToken,
    pub(crate) view: View,
    pub(crate) bounds: Option<Rect>,
    /// Where WebKit keeps its storage; `None` keeps the platform default.
    pub(crate) web_data: Option<PathBuf>,
    /// The secret the bridge script signs its messages with, so a message from any other
    /// script or frame is dropped.
    pub(crate) nonce: String,
    /// The app can update itself: the app menu enables "Check for updates…".
    #[cfg(target_os = "macos")]
    pub(crate) updates: bool,
}

/// Where a close request for the main window stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closing {
    No,
    /// Waiting for the page's `beforeunload` answer.
    Asking,
    /// The page vetoed: the window is hidden until `releaseWindow` closes it.
    Deferred,
}

/// A webview window. Fields drop in order: the webview before its window.
struct Surface {
    webview: WebView,
    window: Window,
    /// Page loads so far; an answer for a page that has since reloaded is dropped.
    load: u64,
    shown: bool,
    focused: bool,
}

struct App {
    origin: Origin,
    token: SessionToken,
    nonce: String,
    context: WebContext,
    proxy: Proxy,
    main: Option<Surface>,
    quota: Option<Surface>,
    dialogs: Vec<DialogKind>,
    main_bounds: Option<Rect>,
    main_closing: Closing,
    forwarded: HashMap<u64, Ticket>,
    next_forward: u64,
    ready: bool,
    exit: Option<i32>,
    #[cfg(target_os = "macos")]
    menu: Option<muda::Menu>,
    #[cfg(target_os = "macos")]
    regular: bool,
    #[cfg(target_os = "macos")]
    zoom_level: f64,
    #[cfg(target_os = "macos")]
    updates: bool,
}

pub(crate) fn run(boot: Boot) -> ! {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    let event_loop = {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        let mut event_loop = event_loop;
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
        event_loop
    };
    let proxy = event_loop.create_proxy();
    read_daemon(&proxy);
    let mut first = Some(boot.view);
    let mut app = App {
        origin: boot.origin,
        token: boot.token,
        nonce: boot.nonce,
        context: WebContext::new(boot.web_data),
        proxy,
        main: None,
        quota: None,
        dialogs: Vec::new(),
        main_bounds: boot.bounds,
        main_closing: Closing::No,
        forwarded: HashMap::new(),
        next_forward: 0,
        ready: false,
        exit: None,
        #[cfg(target_os = "macos")]
        menu: None,
        #[cfg(target_os = "macos")]
        regular: false,
        #[cfg(target_os = "macos")]
        zoom_level: 0.0,
        #[cfg(target_os = "macos")]
        updates: boot.updates,
    };
    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                #[cfg(target_os = "macos")]
                match native::install_menu(&app.proxy, app.updates) {
                    Ok(menu) => app.menu = Some(menu),
                    Err(error) => log(format_args!("could not build the menu: {error}")),
                }
                if let Some(view) = first.take() {
                    app.show(target, view);
                }
            }
            Event::UserEvent(event) => app.handle(target, event),
            Event::WindowEvent {
                window_id, event, ..
            } => app.window_event(window_id, &event),
            _ => {}
        }
        #[cfg(target_os = "macos")]
        app.sync_activation_policy(target);
        // Before Init has opened the first view, an empty helper is starting, not done.
        let idle =
            first.is_none() && app.main.is_none() && app.quota.is_none() && app.dialogs.is_empty();
        if let Some(code) = app.exit.or(idle.then_some(0)) {
            *control_flow = ControlFlow::ExitWithCode(code);
        }
    })
}

/// Reads the daemon's messages on a thread of their own.
fn read_daemon(proxy: &Proxy) {
    let reader = proxy.clone();
    let spawned = std::thread::Builder::new()
        .name("daemon-pipe".to_owned())
        .spawn(move || {
            // Stdin buffers what `main` read past the token line; this lock reads on from it.
            let mut input = io::stdin().lock();
            loop {
                match helper::read::<ToHelper>(&mut input) {
                    Ok(Some(message)) => post(&reader, UserEvent::Daemon(message)),
                    Ok(None) => break,
                    Err(error) => {
                        log(format_args!("the daemon pipe failed: {error}"));
                        break;
                    }
                }
            }
            post(&reader, UserEvent::DaemonGone);
        });
    if let Err(error) = spawned {
        log(format_args!("could not read the daemon pipe: {error}"));
        post(proxy, UserEvent::DaemonGone);
    }
}

fn main_url(origin: &Origin, profile: Option<&ProfileId>) -> String {
    match profile {
        Some(profile) => format!("{}/?profile={profile}", origin.as_str()),
        None => origin.as_str().to_owned(),
    }
}

/// The main window's bounds unless it is maximized, minimized or fullscreen.
fn normal_bounds(window: &Window) -> Option<Rect> {
    if window.is_maximized() || window.is_minimized() || window.fullscreen().is_some() {
        return None;
    }
    let scale = window.scale_factor();
    let position = window.outer_position().ok()?.to_logical::<i32>(scale);
    let size = window.inner_size().to_logical::<u32>(scale);
    Some(Rect {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    })
}

impl App {
    fn surface(&self, pane: Pane) -> Option<&Surface> {
        match pane {
            Pane::Main => self.main.as_ref(),
            Pane::Quota => self.quota.as_ref(),
        }
    }

    fn surface_mut(&mut self, pane: Pane) -> Option<&mut Surface> {
        match pane {
            Pane::Main => self.main.as_mut(),
            Pane::Quota => self.quota.as_mut(),
        }
    }

    fn send(&mut self, message: &FromHelper) {
        if let Err(error) = helper::write(&mut io::stdout().lock(), message) {
            log(format_args!("the daemon stopped listening: {error}"));
            self.quit();
        }
    }

    fn mark_ready(&mut self) {
        if !self.ready {
            self.ready = true;
            self.send(&FromHelper::Ready);
        }
    }

    fn quit(&mut self) {
        self.main = None;
        self.quota = None;
        self.exit = Some(0);
    }

    /// A window that cannot open ends a helper that has nothing on screen yet.
    fn failed(&mut self, what: &str, error: &dyn fmt::Display) {
        log(format_args!("could not open the {what}: {error}"));
        if !self.ready {
            self.exit = Some(1);
        }
    }

    fn show(&mut self, target: &Target, view: View) {
        match view {
            View::Main { profile } => self.show_main(target, profile.as_ref()),
            View::Quota { anchor } => self.show_quota(target, anchor),
            View::Dialog { kind } => {
                if !self.dialogs.contains(&kind) {
                    self.dialogs.push(kind);
                    self.mark_ready();
                    native::show_dialog(kind, &self.proxy);
                }
            }
        }
    }

    fn show_main(&mut self, target: &Target, profile: Option<&ProfileId>) {
        // As showMain did: the popup gives way to the main window.
        self.close_quota();
        if let Some(main) = &self.main {
            if profile.is_some()
                && let Err(error) = main.webview.load_url(&main_url(&self.origin, profile))
            {
                log(format_args!("could not open the profile: {error}"));
            }
            self.main_closing = Closing::No;
            native::present(&main.window);
            return;
        }
        let (width, height) = layout::MAIN_SIZE;
        let (min_width, min_height) = layout::MAIN_MIN_SIZE;
        let mut builder = WindowBuilder::new()
            .with_title("NonstopVibin")
            .with_inner_size(LogicalSize::new(width, height))
            .with_min_inner_size(LogicalSize::new(min_width, min_height));
        if let Some(bounds) = self.main_bounds {
            builder = builder
                .with_position(LogicalPosition::new(bounds.x, bounds.y))
                .with_inner_size(LogicalSize::new(
                    bounds.width.max(min_width),
                    bounds.height.max(min_height),
                ));
        }
        #[cfg(target_os = "macos")]
        let builder = {
            use tao::platform::macos::WindowBuilderExtMacOS;
            builder
                .with_titlebar_transparent(true)
                .with_title_hidden(true)
                .with_fullsize_content_view(true)
                .with_traffic_light_inset(LogicalPosition::new(21.0, 17.0))
        };
        let url = main_url(&self.origin, profile);
        match self.open(target, Pane::Main, builder, &url) {
            Ok(surface) => {
                #[cfg(target_os = "linux")]
                native::set_icon(&surface.window);
                self.main = Some(surface);
            }
            Err(error) => self.failed("main window", &error),
        }
    }

    fn show_quota(&mut self, target: &Target, anchor: Rect) {
        if let Some(quota) = &self.quota {
            native::present(&quota.window);
            return;
        }
        let Some(work_area) = native::work_area(anchor) else {
            return self.failed("quota popup", &"no display holds the tray icon");
        };
        let bounds = layout::quota_bounds(anchor, work_area, cfg!(target_os = "macos"));
        let builder = WindowBuilder::new()
            .with_title("NonstopVibin quotas")
            .with_inner_size(LogicalSize::new(bounds.width, bounds.height))
            .with_position(LogicalPosition::new(bounds.x, bounds.y))
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top(true);
        #[cfg(target_os = "linux")]
        let builder = {
            use tao::platform::unix::WindowBuilderExtUnix;
            builder.with_skip_taskbar(true)
        };
        let url = format!("{}/?view=tray", self.origin.as_str());
        match self.open(target, Pane::Quota, builder, &url) {
            Ok(surface) => self.quota = Some(surface),
            Err(error) => self.failed("quota popup", &error),
        }
    }

    /// A hidden window and its webview; the window shows once its page has loaded.
    fn open(
        &mut self,
        target: &Target,
        pane: Pane,
        builder: WindowBuilder,
        url: &str,
    ) -> Result<Surface, String> {
        let window = builder
            .with_visible(false)
            .with_background_color(BACKGROUND)
            .build(target)
            .map_err(|error| error.to_string())?;
        let script = bridge::script(
            &self.origin,
            &self.nonce,
            pane == Pane::Quota,
            cfg!(target_os = "macos") && pane == Pane::Main,
        );
        let (ipc, loads, popup) = (self.proxy.clone(), self.proxy.clone(), self.proxy.clone());
        let origin = self.origin.clone();
        let builder = WebViewBuilder::new_with_web_context(&mut self.context)
            .with_url(url)
            .with_background_color(BACKGROUND)
            .with_devtools(cfg!(debug_assertions))
            .with_initialization_script_for_main_only(script, true)
            .with_ipc_handler(move |request| {
                let url = request.uri().to_string();
                post(
                    &ipc,
                    UserEvent::Ipc {
                        pane,
                        url,
                        body: request.into_body(),
                    },
                );
            })
            .with_navigation_handler(move |url| origin.contains(&url))
            .with_new_window_req_handler(move |url, _features| {
                post(&popup, UserEvent::NewWindow(url));
                NewWindowResponse::Deny
            })
            .with_download_started_handler(|_url, _path| false)
            .with_permission_handler(|_kind| PermissionResponse::Deny)
            .with_on_page_load_handler(move |event, _url| {
                let finished = matches!(event, PageLoadEvent::Finished);
                post(&loads, UserEvent::PageLoad { pane, finished });
            });
        #[cfg(target_os = "linux")]
        let webview = {
            use tao::platform::unix::WindowExtUnix;
            use wry::WebViewBuilderExtUnix;
            let container = window
                .default_vbox()
                .ok_or("the window has no GTK container")?;
            builder.build_gtk(container)
        };
        #[cfg(not(target_os = "linux"))]
        let webview = builder.build(&window);
        let webview = webview.map_err(|error| error.to_string())?;
        Ok(Surface {
            webview,
            window,
            load: 0,
            shown: false,
            focused: false,
        })
    }

    fn close_quota(&mut self) {
        if self.quota.take().is_some() {
            self.send(&FromHelper::Closed {
                view: ViewKind::Quota,
                bounds: None,
            });
        }
    }

    fn close_main(&mut self) {
        let Some(main) = self.main.take() else { return };
        if let Some(bounds) = normal_bounds(&main.window) {
            self.main_bounds = Some(bounds);
        }
        drop(main);
        self.main_closing = Closing::No;
        let bounds = self.main_bounds;
        self.send(&FromHelper::Closed {
            view: ViewKind::Main,
            bounds,
        });
    }

    /// The draft guard (`useKeepDraft`): the page may veto, as with Electron's
    /// `will-prevent-unload`; then the window hides and closes on `releaseWindow`.
    fn ask_to_close_main(&mut self) {
        let Some(main) = &self.main else { return };
        if self.main_closing == Closing::Asking {
            return;
        }
        let proxy = self.proxy.clone();
        let asked =
            main.webview
                .evaluate_script_with_callback(bridge::BEFORE_UNLOAD, move |vetoed| {
                    post(
                        &proxy,
                        UserEvent::Unload {
                            vetoed: vetoed == "true",
                        },
                    );
                });
        match asked {
            Ok(()) => self.main_closing = Closing::Asking,
            Err(error) => {
                log(format_args!("could not ask the page to close: {error}"));
                self.close_main();
            }
        }
    }

    fn handle(&mut self, target: &Target, event: UserEvent) {
        match event {
            UserEvent::Daemon(ToHelper::Show { view }) => self.show(target, view),
            UserEvent::Daemon(ToHelper::Reply { id, outcome }) => {
                if let Some(ticket) = self.forwarded.remove(&id) {
                    self.answer(ticket, &outcome);
                }
            }
            UserEvent::Daemon(ToHelper::CloseQuota) => self.close_quota(),
            UserEvent::Daemon(ToHelper::Quit) | UserEvent::DaemonGone => self.quit(),
            UserEvent::Daemon(ToHelper::Start { .. }) => {
                log(format_args!("ignored a second Start"));
            }
            UserEvent::Ipc { pane, url, body } => self.ipc(pane, &url, &body),
            UserEvent::PageLoad { pane, finished } => self.page_load(pane, finished),
            UserEvent::NewWindow(url) => {
                if bridge::fits_external(&url) {
                    self.send(&FromHelper::OpenExternal { url });
                } else {
                    log(format_args!("dropped an overlong new-window URL"));
                }
            }
            UserEvent::Unload { vetoed } => match &self.main {
                Some(main) if vetoed => {
                    main.window.set_visible(false);
                    self.main_closing = Closing::Deferred;
                }
                _ => self.close_main(),
            },
            UserEvent::FolderChosen { ticket, result } => {
                let outcome = match result {
                    Ok(path) => Outcome::Result(json!(path)),
                    Err(message) => Outcome::Error(message),
                };
                self.answer(ticket, &outcome);
            }
            UserEvent::DialogAnswered { kind, restart } => {
                if let Some(index) = self.dialogs.iter().position(|open| *open == kind) {
                    self.dialogs.swap_remove(index);
                }
                if restart {
                    self.send(&FromHelper::RestartToUpdate);
                }
                self.send(&FromHelper::Closed {
                    view: ViewKind::Dialog { kind },
                    bounds: None,
                });
            }
            #[cfg(target_os = "macos")]
            UserEvent::Menu(id) => self.menu(&id),
        }
    }

    fn page_load(&mut self, pane: Pane, finished: bool) {
        let Some(surface) = self.surface_mut(pane) else {
            return;
        };
        if !finished {
            surface.load += 1;
            return;
        }
        if surface.shown {
            return;
        }
        surface.shown = true;
        native::present(&surface.window);
        self.mark_ready();
    }

    fn window_event(&mut self, id: WindowId, event: &WindowEvent) {
        let is = |surface: &Option<Surface>| surface.as_ref().is_some_and(|s| s.window.id() == id);
        if is(&self.main) {
            if matches!(event, WindowEvent::CloseRequested) {
                self.ask_to_close_main();
            }
            return;
        }
        let Some(quota) = self.quota.as_mut().filter(|quota| quota.window.id() == id) else {
            return;
        };
        match event {
            WindowEvent::Focused(true) => quota.focused = true,
            // Focus loss closes the popup, once it has had focus.
            WindowEvent::Focused(false) if quota.focused => self.close_quota(),
            WindowEvent::CloseRequested => self.close_quota(),
            _ => {}
        }
    }

    fn ipc(&mut self, pane: Pane, url: &str, body: &str) {
        let Some(load) = self.surface(pane).map(|surface| surface.load) else {
            return;
        };
        match bridge::parse(&self.origin, &self.nonce, url, body) {
            Err(Rejected::Untrusted(reason)) => {
                log(format_args!("dropped a bridge message: {reason}"));
            }
            Err(Rejected::Invalid { id, message }) => {
                self.answer(Ticket { pane, load, id }, &Outcome::Error(message));
            }
            Ok((id, call)) => self.call(Ticket { pane, load, id }, call),
        }
    }

    fn call(&mut self, ticket: Ticket, call: Call) {
        let done = Outcome::Result(Value::Null);
        match call {
            Call::Bootstrap => {
                let token = json!({ "token": self.token.expose_secret() });
                self.answer(ticket, &Outcome::Result(token));
            }
            Call::Copy(text) => {
                let outcome = native::copy(&text).map_or_else(Outcome::Error, |()| done);
                self.answer(ticket, &outcome);
            }
            Call::ShowWindow(profile) => {
                self.send(&FromHelper::OpenMain { profile });
                self.answer(ticket, &done);
            }
            Call::ReleaseWindow => {
                let hidden = self
                    .main
                    .as_ref()
                    .is_some_and(|main| !main.window.is_visible());
                self.answer(ticket, &done);
                // As Electron's close(): another draft on the page may still veto.
                if ticket.pane == Pane::Main && self.main_closing == Closing::Deferred && hidden {
                    self.ask_to_close_main();
                }
            }
            Call::ChooseProject => match &self.main {
                Some(main) => native::choose_folder(&main.window, &self.proxy, ticket),
                None => self.answer(
                    ticket,
                    &Outcome::Error("Open the main window first.".to_owned()),
                ),
            },
            Call::Daemon(call) => {
                self.next_forward += 1;
                let id = self.next_forward;
                self.forwarded.insert(id, ticket);
                self.send(&FromHelper::Bridge { id, call });
            }
            Call::Escape => {
                if ticket.pane == Pane::Quota {
                    self.close_quota();
                }
            }
            // Only the macOS main window's bridge script sends it.
            #[cfg(target_os = "macos")]
            Call::DragWindow => {
                if let Some(main) = self.main.as_ref().filter(|_| ticket.pane == Pane::Main) {
                    native::drag(&main.window);
                }
            }
            #[cfg(not(target_os = "macos"))]
            Call::DragWindow => {}
        }
    }

    fn answer(&self, ticket: Ticket, outcome: &Outcome) {
        let Some(surface) = self.surface(ticket.pane).filter(|s| s.load == ticket.load) else {
            return;
        };
        if let Err(error) = surface
            .webview
            .evaluate_script(&bridge::reply(ticket.id, outcome))
        {
            log(format_args!("could not answer the page: {error}"));
        }
    }

    #[cfg(target_os = "macos")]
    fn menu(&mut self, id: &muda::MenuId) {
        use native::MenuAction;
        let zoom = match native::menu_action(id) {
            Some(MenuAction::CheckUpdates) => return self.send(&FromHelper::CheckUpdates),
            Some(MenuAction::QuitApp) => return self.send(&FromHelper::QuitApp),
            Some(MenuAction::ToggleQuota) => return self.send(&FromHelper::ToggleQuota),
            Some(MenuAction::OpenMain) => {
                return self.send(&FromHelper::OpenMain { profile: None });
            }
            Some(MenuAction::ResetZoom) => 0.0,
            Some(MenuAction::ZoomIn) => (self.zoom_level + 0.5).min(9.0),
            Some(MenuAction::ZoomOut) => (self.zoom_level - 0.5).max(-8.0),
            None => return,
        };
        self.zoom_level = zoom;
        // Electron's zoom levels: each step of 0.5 scales by 1.2^0.5.
        if let Some(main) = &self.main
            && let Err(error) = main.webview.zoom(1.2_f64.powf(zoom))
        {
            log(format_args!("could not zoom: {error}"));
        }
    }

    /// The Dock icon shows while the main window is on screen.
    #[cfg(target_os = "macos")]
    fn sync_activation_policy(&mut self, target: &Target) {
        use tao::platform::macos::{ActivationPolicy, EventLoopWindowTargetExtMacOS};
        let regular = self
            .main
            .as_ref()
            .is_some_and(|main| main.shown && main.window.is_visible());
        if regular != self.regular {
            self.regular = regular;
            let policy = if regular {
                ActivationPolicy::Regular
            } else {
                ActivationPolicy::Accessory
            };
            target.set_activation_policy_at_runtime(policy);
        }
    }
}
