//! The desktop app, `nonstopvibin` beside the helper, under an X server: the error box a
//! desktop user sees when it cannot start or stop, and that a signal leaves no window helper
//! behind. Run by tests/ui.sh, which builds both binaries.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, fs, thread};

use nsv_testkit::Port;

use crate::common::x11::{self, Window, X};
use crate::common::{pause, processes, tree, wait_until};

/// `nonstopvibin` beside the helper, where `cargo build -p nsv-cli` puts it.
fn app_binary() -> PathBuf {
    let helper = Path::new(env!("CARGO_BIN_EXE_nonstopvibin-window"));
    let binary = helper.with_file_name("nonstopvibin");
    assert!(
        binary.is_file(),
        "{binary:?} is missing; build it first with `cargo build -p nsv-cli`"
    );
    binary
}

/// The desktop app on a data directory of its own, as a launcher starts it: the display and
/// session bus of the test, nothing else from its environment.
struct App {
    child: Child,
    data: PathBuf,
    stderr: Arc<Mutex<String>>,
}

impl App {
    fn launch(name: &str, args: &[&str], prepare: impl FnOnce(&Path)) -> Self {
        let root = env::temp_dir().join(format!("nsv-app-{name}-{}", std::process::id()));
        fs::remove_dir_all(&root).unwrap_or_default();
        let data = root.join("data");
        fs::create_dir_all(root.join("home")).unwrap();
        prepare(&data);
        let mut command = Command::new(app_binary());
        command
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.join("home"))
            .env("NONSTOPVIBIN_DATA_DIR", &data)
            .env("NONSTOPVIBIN_PORT", Port::reserve().release().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for name in [
            "DISPLAY",
            "XAUTHORITY",
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_RUNTIME_DIR",
        ] {
            if let Some(value) = env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        let log = Arc::clone(&stderr);
        let mut errors = child.stderr.take().unwrap();
        thread::spawn(move || {
            let mut text = String::new();
            errors.read_to_string(&mut text).unwrap_or_default();
            log.lock().unwrap().push_str(&text);
        });
        Self {
            child,
            data,
            stderr,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn signal(&self, name: &str) {
        let status = Command::new("kill")
            .args([&format!("-{name}"), &self.pid().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn wait(&mut self) -> ExitStatus {
        let mut status = None;
        wait_until("the app exits", || {
            status = self.child.try_wait().unwrap();
            status.is_some()
        });
        status.unwrap()
    }

    /// Standard error once the app has exited and the reader has drained it.
    fn stderr(&self) -> String {
        pause(Duration::from_millis(100));
        self.stderr.lock().unwrap().clone()
    }

    /// The window helper the app started, with everything below it.
    fn helper_processes(&self) -> HashSet<u32> {
        let mut found = tree(self.pid());
        found.remove(&self.pid());
        found
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.child.kill().unwrap_or_default();
            self.child.wait().unwrap_or_else(|error| panic!("{error}"));
        }
    }
}

fn shown_window(x: &X, title: &str) -> Window {
    let mut found = None;
    wait_until(&format!("{title:?} shows"), || {
        found = x.find(title).filter(|&window| x.mapped(window));
        found.is_some()
    });
    found.unwrap()
}

fn assert_all_gone(ran: &HashSet<u32>) {
    wait_until("the window helper's processes exit", || {
        let alive = processes();
        ran.iter().all(|pid| !alive.contains_key(pid))
    });
}

/// The error box holds the exit until the user dismisses it; then the app exits 1 and its
/// helper is gone.
fn dismiss_error(x: &X, app: &mut App) {
    let dialog = shown_window(x, "NonstopVibin");
    wait_until("the error has the focus", || x.has_focus(dialog));
    let helper = app.helper_processes();
    assert!(!helper.is_empty(), "no helper shows the error");
    pause(Duration::from_millis(300));
    assert!(
        app.child.try_wait().unwrap().is_none(),
        "exited under the error"
    );
    x.key(x11::RETURN);
    assert_eq!(app.wait().code(), Some(1));
    assert_all_gone(&helper);
}

static SERIAL: Mutex<()> = Mutex::new(());

/// A database whose `vault.key` is gone: main.ts showed the store's reason in its error box.
#[test]
#[ignore = "needs the nonstopvibin binary and a desktop: run tests/ui.sh"]
fn a_failed_start_shows_its_reason_until_dismissed_then_exits_1() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let x = X::open();
    let mut app = App::launch("start", &[], |data| {
        fs::create_dir_all(data).unwrap();
        fs::write(data.join("nonstopvibin.sqlite"), b"").unwrap();
    });
    dismiss_error(&x, &mut app);
    let stderr = app.stderr();
    assert!(
        stderr.contains(
            "NonstopVibin could not start: vault.key is missing for an existing database. Restore its matching key from your backup.\n"
        ),
        "{stderr}"
    );
    assert!(!app.data.join("vault.key").exists());
}

/// `server.json` turned into a folder cannot be removed on the way out, so closing fails.
#[test]
#[ignore = "needs the nonstopvibin binary and a desktop: run tests/ui.sh"]
fn a_failed_quit_shows_its_reason_until_dismissed_then_exits_1() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let x = X::open();
    let mut app = App::launch("quit", &["--background"], |_| {});
    let session = app.data.join("server.json");
    wait_until("the app writes server.json", || session.is_file());
    fs::remove_file(&session).unwrap();
    fs::create_dir(&session).unwrap();
    app.signal("TERM");
    dismiss_error(&x, &mut app);
    let stderr = app.stderr();
    // A line of its own; what else the app logged first depends on timing (a tray without a
    // StatusNotifierWatcher reports it, unless the quit comes first).
    assert!(
        stderr
            .lines()
            .any(|line| line
                == "server.json must be an owner-only ordinary file you own (chmod 600)."),
        "{stderr}"
    );
}

/// Each signal stops the app as Quit does: the main window's helper and every process it
/// started exit with it.
#[test]
#[ignore = "needs the nonstopvibin binary and a desktop: run tests/ui.sh"]
fn each_signal_stops_the_app_and_leaves_no_window_helper() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let x = X::open();
    for signal in ["TERM", "INT", "HUP"] {
        let mut app = App::launch(&format!("signal-{signal}"), &[], |_| {});
        shown_window(&x, "NonstopVibin");
        let helper = app.helper_processes();
        assert!(!helper.is_empty(), "no helper shows the window");
        app.signal(signal);
        assert_eq!(app.wait().code(), Some(0), "{signal}: {}", app.stderr());
        assert_all_gone(&helper);
        wait_until("the window closes", || x.find("NonstopVibin").is_none());
    }
}
