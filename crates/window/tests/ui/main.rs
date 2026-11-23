//! UI checks in the real window helper. Each check is a script (`checks/<name>.js`) that a
//! local origin (`site.rs`) puts ahead of the page's own scripts: built fixture pages
//! (`dist/ui-fixtures`, from `fixtures/build.mjs`), the built app (`dist/client`), or the app
//! as the debug `nonstopvibin serve` daemon serves it. The script reports its result, and asks
//! the test for what only a user can do: keys, clicks, the close button.
//!
//! Needs `bun run build` (or `vite build`), `bun crates/window/tests/ui/fixtures/build.mjs`,
//! `cargo build -p nsv-cli -p nsv-window`, `.vendor/core` and a display; run through
//! crates/window/tests/ui.sh.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]
#![cfg(any(target_os = "linux", target_os = "macos"))]

#[path = "../common/mod.rs"]
#[expect(dead_code, reason = "the checks use part of the harness")]
mod common;
mod daemon;
mod desktop;
mod ipc;
mod site;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use std::{env, fs};

use common::{Helper, TOKEN, wait_until};
use daemon::Daemon;
use desktop::Desktop;
use nsv_domain::helper::{FromHelper, Rect, ToHelper, View, ViewKind};
use serde_json::{Value, json};
use site::{CHECK_TIMEOUT, Content, Event, Site};

/// One helper at a time: they share the display's focus and pointer.
static SERIAL: Mutex<()> = Mutex::new(());

fn checkout() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A build output under `dist/`, which the runner script builds first.
fn built(directory: &str) -> PathBuf {
    let path = checkout().join("dist").join(directory);
    assert!(
        path.is_dir(),
        "{path:?} is missing; run crates/window/tests/ui.sh"
    );
    path
}

fn data_directory(name: &str) -> PathBuf {
    let path = env::temp_dir().join(format!("nsv-ui-window-{name}-{}", std::process::id()));
    fs::remove_dir_all(&path).unwrap_or_default();
    fs::create_dir_all(&path).unwrap();
    path
}

fn quota_view() -> View {
    View::Quota {
        anchor: Rect {
            x: 1200,
            y: 0,
            width: 24,
            height: 24,
        },
    }
}

/// Opens `view` on `site` in a new helper keeping its storage in `data`, and runs the page's
/// check to its end: the data it reported, or a panic with the label of the assertion that
/// failed. A result `then: "close"` asks for the close button, which must close the window;
/// `then: "closes"` says the page's last step closes it.
fn check(name: &str, site: &Site, view: View, token: &str, data: &Path) -> Value {
    let binary = Path::new(env!("CARGO_BIN_EXE_nonstopvibin-window"));
    let mut helper = Helper::launch(binary, &site.origin(), view, None, data, token);
    let mut bridge = Bridge::default();
    loop {
        match helper.next() {
            FromHelper::Ready => break,
            FromHelper::Bridge { call, .. } => bridge.forwarded.push(format!("{call:?}")),
            other => panic!(
                "{name}: {other:?} before Ready; helper log:\n{}",
                helper.log()
            ),
        }
    }
    let desktop = Desktop::open();
    let deadline = Instant::now() + CHECK_TIMEOUT;
    let mut step = Value::Null;
    let result = loop {
        assert!(
            Instant::now() < deadline,
            "{name}: timed out after {step}; helper log:\n{}",
            helper.log()
        );
        let closed = bridge.watch(&helper);
        assert!(!closed, "{name}: the main window closed after {step}");
        match site.poll(Duration::from_millis(50)) {
            None => {}
            Some(Event::Report(report)) => match report["step"].as_str() {
                Some("done") => break report,
                Some("failed") => panic!(
                    "{name}: {}\n  actual: {}\n  expected: {}\n{}",
                    report["label"], report["actual"], report["expected"], report["stack"]
                ),
                _ => step = report,
            },
            Some(Event::Act(action, reply)) => {
                let answer = perform(&desktop, &helper, &mut bridge, &action);
                reply.send(answer).unwrap_or_default();
            }
        }
    };
    if result["then"] == "close" {
        desktop.close_main();
    }
    if result["then"].is_string() {
        let closed = helper.next_event();
        assert!(
            matches!(
                closed,
                FromHelper::Closed {
                    view: ViewKind::Main,
                    ..
                }
            ),
            "{name}: {closed:?}"
        );
    } else {
        helper.send(&ToHelper::Quit);
    }
    assert!(helper.wait().success(), "{}", helper.log());
    result
}

/// What reached the daemon's end of the bridge, what the helper refused, and the helper's
/// own requests (its menu's, on macOS).
#[derive(Default)]
struct Bridge {
    forwarded: Vec<String>,
    sent: Vec<String>,
    logged: usize,
    closed: bool,
}

impl Bridge {
    /// Takes the helper's messages; whether the main window has closed.
    fn watch(&mut self, helper: &Helper) -> bool {
        for event in helper.drain(Duration::ZERO) {
            match event {
                FromHelper::Bridge { call, .. } => self.forwarded.push(format!("{call:?}")),
                FromHelper::Closed {
                    view: ViewKind::Main,
                    ..
                } => self.closed = true,
                other => self.sent.push(format!("{other:?}")),
            }
        }
        self.closed
    }

    /// The calls forwarded and the messages dropped since the last time.
    fn since(&mut self, helper: &Helper) -> Value {
        self.watch(helper);
        let log = helper.log();
        let dropped: Vec<&str> = log
            .get(self.logged..)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once("dropped a bridge message: "))
            .map(|(_, reason)| reason)
            .collect();
        let answer = json!({ "forwarded": self.forwarded, "dropped": dropped });
        self.logged = log.len();
        self.forwarded.clear();
        answer
    }
}

fn perform(desktop: &Desktop, helper: &Helper, bridge: &mut Bridge, action: &Value) -> Value {
    if action["bridge"] == true {
        // Long enough for the helper to have answered or dropped what the page posted.
        common::pause(Duration::from_millis(300));
        return bridge.since(helper);
    }
    if action["sent"] == true {
        common::pause(Duration::from_millis(300));
        bridge.watch(helper);
        return json!({ "sent": std::mem::take(&mut bridge.sent) });
    }
    #[cfg(target_os = "macos")]
    if let Some(answer) = desktop.mac_action(action) {
        return answer;
    }
    if let Some(key) = action["key"].as_str() {
        desktop.key(key);
    } else if let Some([x, y]) = action["click"].as_array().map(Vec::as_slice) {
        desktop.click(x.as_f64().unwrap(), y.as_f64().unwrap());
    } else if action["close"] == true {
        desktop.close_main();
        wait_until("the closed window hides", || !desktop.main_visible());
    } else if action["show"] == true {
        helper.send(&ToHelper::Show {
            view: View::Main { profile: None },
        });
        wait_until("the window shows again", || desktop.main_visible());
    } else if let Some(ms) = action["settle"].as_u64() {
        common::pause(Duration::from_millis(ms));
    } else {
        panic!("unknown action {action}");
    }
    json!({ "visible": desktop.main_visible() })
}

/// A check in a fresh data directory.
fn check_once(name: &str, site: &Site, view: View, token: &str) -> Value {
    let data = data_directory(name);
    let result = check(name, site, view, token, &data);
    fs::remove_dir_all(&data).unwrap_or_default();
    result
}

fn fixtures(page: &'static str) -> Content {
    Content::Files(built("ui-fixtures"), page)
}

#[test]
#[ignore = "needs the built interface, its fixtures and a desktop: run tests/ui.sh"]
fn focus_rings_follow_the_keyboard_not_the_mouse() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let site = Site::start(fixtures("focus.html"), "focus");
    check_once("focus", &site, View::Main { profile: None }, TOKEN);
}

/// The application menu the helper installs on macOS: Edit's shortcuts in a text field
/// through the system pasteboard, View's accelerators, and Quit.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "needs the built interface, its fixtures and a desktop: run tests/ui.sh"]
fn the_app_menu_edits_text_and_reaches_the_daemon() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let site = Site::start(fixtures("focus.html"), "app-menu");
    check_once("app-menu", &site, View::Main { profile: None }, TOKEN);
}

/// The macOS main window moves when a drag starts in a drag region, not on a button in one.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "needs the built interface, its fixtures and a desktop: run tests/ui.sh"]
fn drag_regions_move_the_main_window() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let site = Site::start(fixtures("logo.html"), "drag-region");
    check_once("drag-region", &site, View::Main { profile: None }, TOKEN);
}

#[test]
#[ignore = "needs the built interface, its fixtures and a desktop: run tests/ui.sh"]
fn the_logo_replays_once_and_rests() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let site = Site::start(fixtures("logo.html"), "logo-animation");
    check_once("logo", &site, View::Main { profile: None }, TOKEN);
}

/// The built entry loads the main or tray root as a separate chunk; when that request fails
/// the window shows the recovery view instead of staying blank.
#[test]
#[ignore = "needs the built interface, its fixtures and a desktop: run tests/ui.sh"]
fn a_failed_root_chunk_shows_the_recovery_view() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let root_chunk = |path: &str| {
        path.strip_prefix("/assets/")
            .is_some_and(|name| name.starts_with("App-") || name.starts_with("Tray-"))
            && Path::new(path)
                .extension()
                .is_some_and(|extension| extension == "js")
    };
    let content = Content::Files(built("client"), "index.html");
    let site = Site::refusing(content, "root-load", root_chunk);
    for (window, view) in [
        ("main", View::Main { profile: None }),
        ("tray", quota_view()),
    ] {
        let before = site.refused().len();
        check_once("root-load", &site, view, TOKEN);
        assert!(
            site.refused()[before..].iter().any(|path| root_chunk(path)),
            "the {window} root chunk request was failed: {:?}",
            site.refused()
        );
    }
}

/// A failed /state poll must not install the close guard in the tray view.
#[test]
#[ignore = "needs the built interface, its fixtures and a desktop: run tests/ui.sh"]
fn failed_polls_leave_the_tray_closable() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let content = Content::Files(built("client"), "index.html");
    let site = Site::start(content, "tray-guard");
    let main = check_once("tray-guard", &site, View::Main { profile: None }, TOKEN);
    assert_eq!(
        main["guards"], 1,
        "control: the main window keeps a visible poll failure as a draft"
    );
    let tray = check_once("tray-guard", &site, quota_view(), TOKEN);
    assert_eq!(
        tray["guards"], 0,
        "the tray installs no beforeunload guard when a poll fails"
    );
}

/// The built app with one running profile keeps page geometry identical across pages and
/// widths.
#[test]
#[ignore = "needs the built interface, its fixtures and a desktop: run tests/ui.sh"]
fn pages_keep_their_geometry_at_every_width() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    built("client");
    let daemon = Daemon::serve("layout");
    daemon.running_profile("Personal");
    let site = Site::start(Content::Framed(daemon.port), "page-layout");
    site.check_with("page-layout", &json!({ "token": daemon.token }));
    check_once("page-layout", &site, View::Main { profile: None }, TOKEN);
}

/// What desktop-smoke drove in the full app, in three openings of the main window as the
/// tray's "Open main window" makes them: drafts defer closing, a background failure stays
/// readable, and the settings page edits one profile and one global draft at a time.
#[test]
#[ignore = "needs the built interface, its fixtures and a desktop: run tests/ui.sh"]
fn drafts_and_settings_survive_closing_the_window() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    built("client");
    let daemon = Daemon::serve("flows");
    let site = Site::start(Content::Daemon(daemon.port), "drafts");
    // One data directory, as the shell keeps for every helper it starts.
    let data = data_directory("flows");
    for name in ["drafts", "settings", "settings-restored"] {
        site.check(name);
        check(
            name,
            &site,
            View::Main { profile: None },
            &daemon.token,
            &data,
        );
    }
    fs::remove_dir_all(&data).unwrap_or_default();
}
