//! The helper end to end under an X server (Xvfb in CI and Docker): a harness plays the
//! daemon, a page under the production CSP drives the bridge, and Xlib plays the user.
//! Linux only; the macOS paths run on the macOS CI runner.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]
#![cfg(target_os = "linux")]

#[expect(dead_code, reason = "the pointer is the UI checks'")]
mod common;

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use common::x11::{self, Window, X};
use common::{Helper, Server, TOKEN, pause, processes, tree, wait_until};
use nsv_domain::helper::{DaemonCall, DialogKind, FromHelper, Rect, ToHelper, View, ViewKind};
use nsv_domain::id::ProfileId;
use serde_json::{Value, json};

/// One helper at a time: they share the X server's focus, and the exit checks look at every
/// WebKit process on the machine.
static SERIAL: Mutex<()> = Mutex::new(());

fn binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_nonstopvibin-window"))
}

fn directory(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("nsv-window-{name}-{}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn shown_window(x: &X, title: &str) -> Window {
    let mut found = None;
    wait_until(&format!("{title:?} shows"), || {
        found = x.find(title).filter(|&window| x.mapped(window));
        found.is_some()
    });
    found.unwrap()
}

/// After the helper exits, nothing it started lives on, and no WebKit process at all does.
fn assert_all_gone(ran: &HashSet<u32>) {
    wait_until("the helper's processes exit", || {
        let alive = processes();
        ran.iter().all(|pid| !alive.contains_key(pid))
            && !alive.values().any(|(_, name)| name.starts_with("WebKit"))
    });
}

fn webkit_processes(pids: &HashSet<u32>) -> usize {
    let alive = processes();
    pids.iter()
        .filter(|pid| {
            alive
                .get(pid)
                .is_some_and(|(_, name)| name.starts_with("WebKit"))
        })
        .count()
}

fn files_named(directory: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(directory).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_named(&path, name));
        } else if entry.file_name() == name {
            found.push(path);
        }
    }
    found
}

fn clipboard_text() -> String {
    gtk::init().unwrap();
    gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD)
        .wait_for_text()
        .unwrap()
        .into()
}

/// Every bridge method answers as `main.ts`'s handlers did, under the production CSP.
fn assert_bridge_calls(server: &mut Server) {
    let calls = server.result("calls");
    assert_eq!(
        calls["calls"],
        json!({
            "bootstrap": { "value": { "token": TOKEN } },
            "copy": { "value": null },
            "releaseWindow": { "value": null },
            "showWindow": { "value": null },
            "getLoginItem": { "value": true },
            "setLoginItem": { "value": true },
            "revealData": { "value": null },
            "openExternal": { "error": "This external URL is not an approved provider sign-in page." },
        })
    );
    assert_eq!(
        calls["invalid"],
        json!({
            "openExternal": { "error": "Invalid argument for openExternal." },
            "copy": { "error": "Invalid argument for copy." },
            "showWindow": { "error": "Invalid argument for showWindow." },
            "setLoginItem": { "error": "Invalid argument for setLoginItem." },
            "bootstrap": { "error": "Invalid argument for bootstrap." },
        })
    );
    assert_eq!(calls["forged"], "posted");
    assert_eq!(calls["afterForgery"], json!({ "value": null }));
    assert_eq!(clipboard_text(), "copied by the bridge ✓");
}

/// Navigation stays on the origin; new windows and downloads go nowhere but the daemon.
fn assert_locked_down(server: &mut Server, helper: &Helper, data: &Path) {
    let origin = server.origin();
    let stayed = server.result("stayed");
    assert_eq!(
        stayed["href"],
        format!("{}/", origin.as_str()),
        "navigation left the origin"
    );
    assert_eq!(stayed["opened"], true, "window.open returned a window");
    let events = helper.drain(Duration::from_millis(500));
    let forwarded: Vec<&DaemonCall> = events
        .iter()
        .filter_map(|event| match event {
            FromHelper::Bridge { call, .. } => Some(call),
            _ => None,
        })
        .collect();
    // The forged getLoginItem never reaches the daemon.
    assert_eq!(
        forwarded,
        [
            &DaemonCall::GetLoginItem,
            &DaemonCall::SetLoginItem(true),
            &DaemonCall::RevealData,
            &DaemonCall::OpenExternal("https://example.com/".to_owned()),
        ]
    );
    let shown = ProfileId::parse("0b6f4f5e-6a7b-4c8d-9e0f-1a2b3c4d5e6f").unwrap();
    assert!(
        events.contains(&FromHelper::OpenMain {
            profile: Some(shown)
        }),
        "{events:?}"
    );
    let popup = FromHelper::OpenExternal {
        url: "https://example.com/popup".to_owned(),
    };
    assert!(events.contains(&popup), "{events:?}");
    assert!(
        helper
            .log()
            .contains("dropped a bridge message: the message did not come from the bridge script"),
        "{}",
        helper.log()
    );
    assert_eq!(
        files_named(data, "harness.bin"),
        Vec::<PathBuf>::new(),
        "a download was saved"
    );
}

/// A vetoed close hides the window, showing it again cancels the deferred close, and a
/// release after a second veto leaves it to close.
fn assert_draft_guard_defers_closing(x: &X, server: &mut Server, helper: &Helper) {
    let main = shown_window(x, "NonstopVibin");
    x.close(main);
    assert_eq!(server.result("vetoed")["vetoes"], 1);
    wait_until("the vetoed window hides", || !x.mapped(main));
    helper.send(&ToHelper::Show {
        view: View::Main { profile: None },
    });
    wait_until("the window shows again", || x.mapped(main));
    server.command("release");
    assert_eq!(server.result("released")["value"], Value::Null);
    pause(Duration::from_millis(300));
    assert!(
        x.mapped(main),
        "releaseWindow closed a window that was shown again"
    );

    // Vetoed again, then released: now it closes, and the helper exits.
    x.close(main);
    assert_eq!(server.result("vetoed")["vetoes"], 2);
    wait_until("the vetoed window hides", || !x.mapped(main));
    let early = helper.drain(Duration::from_millis(300));
    assert!(
        !early
            .iter()
            .any(|event| matches!(event, FromHelper::Closed { .. })),
        "{early:?}"
    );
}

#[test]
fn bridge_works_under_the_production_csp_and_the_draft_guard_defers_closing() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let x = X::open();
    let mut server = Server::start();
    let data = directory("main");
    let origin = server.origin();
    let mut helper = Helper::start(binary(), &origin, View::Main { profile: None }, &data);
    assert_eq!(helper.next(), FromHelper::Ready);

    let loaded = server.result("loaded");
    assert_eq!(loaded["bridge"], "object");
    let methods = [
        "bootstrap",
        "chooseProject",
        "copy",
        "getLoginItem",
        "openExternal",
        "releaseWindow",
        "revealData",
        "setLoginItem",
        "showWindow",
    ];
    assert_eq!(loaded["methods"], json!(methods));
    assert_eq!(loaded["frozen"], true);
    assert_eq!(loaded["contextMenuCancelled"], true);
    let ran = tree(helper.pid());
    assert!(
        webkit_processes(&ran) >= 1,
        "no WebKit process under the helper: {ran:?}"
    );

    assert_bridge_calls(&mut server);

    server.result("choosing");
    let chooser = shown_window(&x, "Connect Claude Code in a project");
    x.close(chooser);
    assert_eq!(server.result("chose")["value"], Value::Null);

    assert_locked_down(&mut server, &helper, &data);

    // Show with a profile reloads the open window to it.
    let profile = ProfileId::parse("1c2d3e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f").unwrap();
    helper.send(&ToHelper::Show {
        view: View::Main {
            profile: Some(profile.clone()),
        },
    });
    let reloaded = server.result("profile");
    assert_eq!(
        (&reloaded["profile"], &reloaded["token"]),
        (&json!(profile.as_str()), &json!(TOKEN))
    );
    server.result("guarded");

    assert_draft_guard_defers_closing(&x, &mut server, &helper);
    server.command("finish");
    let closed = helper.next_event();
    let bounds = Rect {
        x: 0,
        y: 0,
        width: 1240,
        height: 880,
    };
    assert_eq!(
        closed,
        FromHelper::Closed {
            view: ViewKind::Main,
            bounds: Some(bounds)
        }
    );
    assert!(helper.wait().success());
    assert_all_gone(&ran);
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

/// A shown quota popup with the keyboard focus, and the processes it runs.
fn focused_popup(x: &X, server: &mut Server, helper: &Helper) -> (Window, HashSet<u32>) {
    assert_eq!(helper.next(), FromHelper::Ready);
    assert_eq!(server.result("tray")["bridge"], "object");
    let popup = shown_window(x, "NonstopVibin quotas");
    wait_until("the popup has the focus", || x.has_focus(popup));
    (popup, tree(helper.pid()))
}

#[test]
fn quota_popup_sits_beside_the_tray_and_closes_on_escape() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let x = X::open();
    let mut server = Server::start();
    let mut helper = Helper::start(
        binary(),
        &server.origin(),
        quota_view(),
        &directory("escape"),
    );
    let (popup, ran) = focused_popup(&x, &mut server, &helper);
    // main.ts's placement on Linux: centred on the icon, clamped 8 px inside the right edge,
    // 8 px above the bottom; 640 tall.
    assert_eq!(x.geometry(popup), (886, 152, 386, 640));
    x.key(x11::ESCAPE);
    assert_eq!(
        helper.next_event(),
        FromHelper::Closed {
            view: ViewKind::Quota,
            bounds: None
        }
    );
    assert!(helper.wait().success());
    assert_all_gone(&ran);
}

#[test]
fn quota_popup_closes_when_it_loses_the_focus() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let x = X::open();
    let mut server = Server::start();
    let mut helper = Helper::start(binary(), &server.origin(), quota_view(), &directory("blur"));
    let (_popup, ran) = focused_popup(&x, &mut server, &helper);
    x.unfocus();
    assert_eq!(
        helper.next_event(),
        FromHelper::Closed {
            view: ViewKind::Quota,
            bounds: None
        }
    );
    assert!(helper.wait().success());
    assert_all_gone(&ran);
}

#[test]
#[expect(
    clippy::print_stderr,
    reason = "which way WebKit refused the redirect is worth seeing"
)]
fn pages_off_the_daemon_origin_get_no_bridge() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let mut server = Server::start();
    let origin = server.origin();
    // The nil profile puts the page in probe mode.
    let probe = ProfileId::parse("00000000-0000-0000-0000-000000000000").unwrap();
    let view = View::Main {
        profile: Some(probe.clone()),
    };
    let mut helper = Helper::start(binary(), &origin, view, &directory("origin"));
    assert_eq!(helper.next(), FromHelper::Ready);

    // A sandboxed frame has an opaque origin: no bridge of its own, none through its parent,
    // and what it posts to the IPC channel itself is dropped.
    let frame = server.result("frame");
    assert_eq!(
        (&frame["bridge"], &frame["parentBridge"]),
        (&json!("undefined"), &json!("blocked"))
    );
    assert_eq!(frame["origin"], "null");

    // A redirect to another origin is refused, or the page it reaches has no bridge.
    let outcome = server.any(&["stray", "stayed-after-redirect"]);
    eprintln!("cross-origin redirect: {outcome}");
    if outcome["step"] == "stray" {
        assert_eq!(outcome["bridge"], "undefined");
    } else {
        assert_eq!(
            outcome["href"],
            format!("{}/?profile={probe}", origin.as_str())
        );
    }
    helper.send(&ToHelper::Quit);
    assert!(helper.wait().success());
    let events = helper.drain(Duration::from_millis(200));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, FromHelper::Bridge { .. })),
        "{events:?}"
    );
    if frame["forged"] == "posted" {
        assert!(
            helper.log().contains("dropped a bridge message"),
            "{}",
            helper.log()
        );
    }
}

#[test]
fn update_dialog_defaults_to_later() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let x = X::open();
    let server = Server::start();
    let view = View::Dialog {
        kind: DialogKind::UpdateReady,
    };
    let mut helper = Helper::start(binary(), &server.origin(), view, &directory("dialog"));
    assert_eq!(helper.next(), FromHelper::Ready);
    let dialog = shown_window(&x, "NonstopVibin");
    wait_until("the dialog has the focus", || x.has_focus(dialog));
    x.key(x11::RETURN);
    let kind = DialogKind::UpdateReady;
    assert_eq!(
        helper.next(),
        FromHelper::Closed {
            view: ViewKind::Dialog { kind },
            bounds: None
        }
    );
    assert!(helper.wait().success());
}

#[test]
fn main_window_restores_its_bounds_and_exits_when_the_daemon_closes_the_pipe() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let x = X::open();
    let mut server = Server::start();
    let data = directory("pipe");
    let bounds = Rect {
        x: 40,
        y: 30,
        width: 900,
        height: 700,
    };
    let view = View::Main { profile: None };
    let mut helper = Helper::start_at(binary(), &server.origin(), view, Some(bounds), &data);
    assert_eq!(helper.next(), FromHelper::Ready);
    server.result("loaded");
    let main = shown_window(&x, "NonstopVibin");
    assert_eq!(x.geometry(main), (40, 30, 900, 700));
    // public/icon.png, which GTK hands the window manager as the window's icon.
    assert!(
        x.icon_size(main)
            .is_some_and(|(width, height)| width > 0 && width == height)
    );
    let webview_data = fs::symlink_metadata(data.join("webview")).unwrap();
    assert_eq!(webview_data.permissions().mode() & 0o777, 0o700);
    let ran = tree(helper.pid());
    helper.close_stdin();
    assert!(helper.wait().success());
    assert_all_gone(&ran);
}
