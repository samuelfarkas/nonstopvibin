//! The event loop. The main thread owns every window, webview and piece of state; the daemon
//! pipe, the webviews and the native dialogs reach it as `UserEvent`s.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::PathBuf;

use nsv_desktop::Command;
use nsv_desktop::bridge::fits_external;
use nsv_desktop::pipe::{
    self, Anchor, Choice, DialogKind, Failure, FromHelper, Outcome, Reason, Rect, ToHelper, View,
    ViewKind,
};
use nsv_domain::endpoint::Origin;
use nsv_domain::id::ProfileId;
use nsv_domain::secret::SessionToken;
use serde_json::{Value, json};
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Theme, Window, WindowBuilder, WindowId};
use wry::{
    NewWindowResponse, PageLoadEvent, PermissionResponse, WebContext, WebView, WebViewBuilder,
};

use crate::bridge::{self, Call, Rejected};
use crate::layout;
use crate::native::{Dialog, Native, Os};

const BACKGROUND: (u8, u8, u8, u8) = (0x1c, 0x1b, 0x18, 0xff);

pub(crate) type Proxy = EventLoopProxy<UserEvent>;
pub(crate) type Target = EventLoopWindowTarget<UserEvent>;

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
    /// The page's `beforeunload` answer to close request `request`.
    Unload {
        request: u64,
        vetoed: bool,
    },
    FolderChosen {
        ticket: Ticket,
        result: Result<Option<String>, String>,
    },
    DialogAnswered {
        kind: DialogKind,
        choice: Choice,
    },
    /// A click or a shortcut in the app menu.
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
    pub(crate) updates: bool,
}

/// Where a close request for the main window stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closing {
    No,
    /// Waiting for the page's `beforeunload` answer to the close request with this number.
    Asking(u64),
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

impl Surface {
    fn on_screen(&self) -> bool {
        self.shown && self.window.is_visible()
    }
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
    /// Close requests asked so far.
    close_requests: u64,
    forwarded: HashMap<u64, Ticket>,
    next_forward: u64,
    ready: bool,
    exit: Option<i32>,
    native: <Os as Native>::State,
}

/// The platform's loop. The UI is dark only, so the native dialogs and menus are dark too,
/// as main.ts's `nativeTheme.themeSource = "dark"` made them.
fn event_loop<T: 'static>() -> EventLoop<T> {
    let event_loop = Os::event_loop();
    event_loop.set_theme(Some(Theme::Dark));
    event_loop
}

/// `failure`'s error dialog with `reason` as its text, then exit once it is dismissed.
pub(crate) fn alert(failure: Failure, reason: Reason) -> ! {
    let event_loop = event_loop::<()>();
    let proxy = event_loop.create_proxy();
    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                let dismissed = proxy.clone();
                Os::show_dialog(&Dialog::alert(failure, &reason), move |_| {
                    // Fails only once the loop has ended.
                    let _ = dismissed.send_event(());
                });
            }
            Event::UserEvent(()) => *control_flow = ControlFlow::ExitWithCode(0),
            _ => {}
        }
    })
}

pub(crate) fn run(boot: Boot) -> ! {
    let event_loop = event_loop();
    let proxy = event_loop.create_proxy();
    read_daemon(&proxy);
    let updates = boot.updates;
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
        close_requests: 0,
        forwarded: HashMap::new(),
        next_forward: 0,
        ready: false,
        exit: None,
        native: <Os as Native>::State::default(),
    };
    // The helper ends the loop itself; any other end is a quit from outside.
    let mut exiting = false;
    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                Os::start(&mut app.native, &app.proxy, updates);
                if let Some(view) = first.take() {
                    app.show(target, view);
                }
            }
            Event::UserEvent(event) => app.handle(target, event),
            Event::WindowEvent {
                window_id, event, ..
            } => app.window_event(window_id, &event),
            // AppKit ends the loop for a quit from outside the helper (the Dock's Quit, the app
            // switcher's, logout) and exits once this returns. The whole app quits, as
            // Electron's did, rather than the daemon taking it for a closed window.
            Event::LoopDestroyed => {
                if !exiting {
                    app.send(&FromHelper::Command {
                        command: Command::Quit,
                    });
                }
                return;
            }
            _ => {}
        }
        Os::settle(&mut app.native, target, || {
            app.main.as_ref().is_some_and(Surface::on_screen)
        });
        // Before Init has opened the first view, an empty helper is starting, not done.
        let idle = first.is_none() && app.windowless();
        if let Some(code) = app.exit.or(idle.then_some(0)) {
            exiting = true;
            *control_flow = ControlFlow::ExitWithCode(code);
        }
    })
}

fn read_daemon(proxy: &Proxy) {
    let reader = proxy.clone();
    let spawned = std::thread::Builder::new()
        .name("daemon-pipe".to_owned())
        .spawn(move || {
            // Stdin buffers what `main` read past the token line; this lock reads on from it.
            let mut input = io::stdin().lock();
            loop {
                match pipe::read::<ToHelper>(&mut input) {
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

    /// The helper exits with its last window.
    fn windowless(&self) -> bool {
        self.main.is_none() && self.quota.is_none() && self.dialogs.is_empty()
    }

    /// Tells the daemon `view` has closed, and whether the helper exits with it.
    fn report_closed(&mut self, view: ViewKind, bounds: Option<Rect>) {
        // A helper that failed tells by its exit status instead.
        let last = self.windowless() && self.exit.is_none();
        self.send(&FromHelper::Closed { view, bounds, last });
    }

    fn send(&mut self, message: &FromHelper) {
        if let Err(error) = pipe::write(&mut io::stdout().lock(), message) {
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
        Os::main_closing(&mut self.native);
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
                    let proxy = self.proxy.clone();
                    Os::show_dialog(&Dialog::of(kind), move |choice| {
                        post(&proxy, UserEvent::DialogAnswered { kind, choice });
                    });
                }
            }
        }
    }

    /// As showMain did: the popup gives way to the main window. It is reported closed once the
    /// main window stands in its place.
    fn show_main(&mut self, target: &Target, profile: Option<&ProfileId>) {
        let popup = self.quota.take().is_some();
        self.open_main(target, profile);
        if popup {
            self.report_closed(ViewKind::Quota, None);
        }
    }

    fn open_main(&mut self, target: &Target, profile: Option<&ProfileId>) {
        if let Some(main) = &self.main {
            if profile.is_some()
                && let Err(error) = main.webview.load_url(&self.origin.main_url(profile))
            {
                log(format_args!("could not open the profile: {error}"));
            }
            self.main_closing = Closing::No;
            Os::present(&main.window);
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
        let url = self.origin.main_url(profile);
        match self.open(target, Pane::Main, Os::main_window(builder), &url) {
            Ok(surface) => {
                Os::main_opened(&mut self.native, &surface.window);
                self.main = Some(surface);
            }
            Err(error) => self.failed("main window", &error),
        }
    }

    fn show_quota(&mut self, target: &Target, anchor: Anchor) {
        if let Some(quota) = &self.quota {
            Os::present(&quota.window);
            return;
        }
        let Some(work_area) = Os::work_area(anchor) else {
            return self.quota_failed(&"no display holds the tray icon");
        };
        let bounds = layout::quota_bounds(anchor, work_area, Os::POPUP_BELOW_ANCHOR);
        let builder = WindowBuilder::new()
            .with_title("NonstopVibin quotas")
            .with_inner_size(LogicalSize::new(bounds.width, bounds.height))
            .with_position(LogicalPosition::new(bounds.x, bounds.y))
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top(true);
        let url = self.origin.tray_url();
        match self.open(target, Pane::Quota, Os::quota_window(builder), &url) {
            Ok(surface) => self.quota = Some(surface),
            Err(error) => self.quota_failed(&error),
        }
    }

    /// The daemon took the popup for open when it asked for it. A helper that keeps running
    /// reports it closed, so the next click on the tray opens it again; one with nothing on
    /// screen exits instead, and the daemon opens the browser.
    fn quota_failed(&mut self, error: &dyn fmt::Display) {
        self.failed("quota popup", error);
        if self.ready {
            self.report_closed(ViewKind::Quota, None);
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
            Os::MAIN_DRAG_REGIONS && pane == Pane::Main,
        );
        let (ipc, loads, popup) = (self.proxy.clone(), self.proxy.clone(), self.proxy.clone());
        let (origin, downloads) = (self.origin.clone(), self.origin.clone());
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
            // WebKitGTK asks here before it downloads a link, and wry does not say the link
            // downloads: the page's own files pass, and the download handler then decides. The
            // helper drops what a page opened at such a URL posts: the URL is not on the origin.
            .with_navigation_handler(move |url| {
                origin.contains(&url)
                    || (cfg!(target_os = "linux") && bridge::own_blob(&origin, &url))
            })
            .with_new_window_req_handler(move |url, _features| {
                post(&popup, UserEvent::NewWindow(url));
                NewWindowResponse::Deny
            })
            // A file the page made (Activity's CSV export) goes where wry puts it: the Downloads
            // folder, under a name no file there has yet.
            .with_download_started_handler(move |url, _path| {
                let allowed = bridge::own_blob(&downloads, &url);
                if !allowed {
                    log(format_args!("refused a download"));
                }
                allowed
            })
            .with_permission_handler(|_kind| PermissionResponse::Deny)
            .with_on_page_load_handler(move |event, _url| {
                let finished = matches!(event, PageLoadEvent::Finished);
                post(&loads, UserEvent::PageLoad { pane, finished });
            });
        let webview = Os::webview(builder, &window)?;
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
            self.report_closed(ViewKind::Quota, None);
        }
    }

    fn close_main(&mut self) {
        let Some(main) = self.main.take() else { return };
        if let Some(bounds) = normal_bounds(&main.window) {
            self.main_bounds = Some(bounds);
        }
        Os::main_closing(&mut self.native);
        drop(main);
        self.main_closing = Closing::No;
        self.report_closed(ViewKind::Main, self.main_bounds);
    }

    /// The draft guard (`useKeepDraft` in src/client/controls.tsx is the page's half): the page
    /// may veto, as with Electron's `will-prevent-unload`; then the window hides and closes on
    /// `releaseWindow`.
    fn ask_to_close_main(&mut self) {
        let Some(main) = &self.main else { return };
        if matches!(self.main_closing, Closing::Asking(_)) {
            return;
        }
        self.close_requests += 1;
        let (request, proxy) = (self.close_requests, self.proxy.clone());
        let asked =
            main.webview
                .evaluate_script_with_callback(bridge::BEFORE_UNLOAD, move |vetoed| {
                    post(
                        &proxy,
                        UserEvent::Unload {
                            request,
                            vetoed: vetoed == "true",
                        },
                    );
                });
        match asked {
            Ok(()) => self.main_closing = Closing::Asking(request),
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
            UserEvent::Daemon(ToHelper::Start { .. } | ToHelper::Alert { .. }) => {
                log(format_args!(
                    "ignored a first-line message after the first line"
                ));
            }
            UserEvent::Ipc { pane, url, body } => self.ipc(pane, &url, &body),
            UserEvent::PageLoad { pane, finished } => self.page_load(pane, finished),
            UserEvent::NewWindow(url) => {
                if fits_external(&url) {
                    self.send(&FromHelper::OpenExternal { url });
                } else {
                    log(format_args!("dropped an overlong new-window URL"));
                }
            }
            // Only the open request's answer counts: showing the window again withdrew it.
            UserEvent::Unload { request, vetoed }
                if self.main_closing == Closing::Asking(request) =>
            {
                match &self.main {
                    Some(main) if vetoed => {
                        main.window.set_visible(false);
                        self.main_closing = Closing::Deferred;
                    }
                    _ => self.close_main(),
                }
            }
            UserEvent::Unload { .. } => {}
            UserEvent::FolderChosen { ticket, result } => {
                let outcome = match result {
                    Ok(path) => Outcome::Result(json!(path)),
                    Err(message) => Outcome::Error(message),
                };
                self.answer(ticket, &outcome);
            }
            UserEvent::DialogAnswered { kind, choice } => {
                if let Some(index) = self.dialogs.iter().position(|open| *open == kind) {
                    self.dialogs.swap_remove(index);
                }
                self.report_closed(ViewKind::Dialog { kind, choice }, None);
            }
            UserEvent::Menu(id) => {
                let main = self.main.as_ref().map(|main| (&main.window, &main.webview));
                if let Some(command) = Os::menu(&mut self.native, &id, main) {
                    self.send(&FromHelper::Command { command });
                }
            }
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
        Os::present(&surface.window);
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
                let outcome = Os::copy(&text).map_or_else(Outcome::Error, |()| done);
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
                Some(main) => Os::choose_folder(&main.window, &self.proxy, ticket),
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
            // Only the bridge script of a main window with drag regions sends it.
            Call::DragWindow { x, y } => {
                if let Some(main) = self.main.as_ref().filter(|_| ticket.pane == Pane::Main) {
                    Os::drag(&self.native, &main.window, x, y);
                }
            }
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
}
