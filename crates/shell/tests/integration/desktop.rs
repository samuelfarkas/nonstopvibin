//! The shell end to end on Linux: the real `nonstopvibin-window` under an X server, the tray
//! registered with a `StatusNotifierWatcher` this test serves on the session bus, and every
//! click made over D-Bus as a tray host would. Run by tests/gui.sh, which provides Xvfb, a
//! session bus, the built helper and a fake `xdg-open`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nsv_shell::TrayStatus;
use nsv_testkit::shell::Host;
use serde_json::{Value, json};
use zbus::zvariant::{OwnedValue, Value as Variant};

use crate::common::{TIMEOUT, directory, exited, options, pause, spawn_shell, wait_until};

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'";
const PAGE: &str =
    r#"<!doctype html><meta charset="utf-8"><title>shell</title><script src="/page.js"></script>"#;
/// The main view calls each method the daemon serves; the quota view reports that it loaded.
const SCRIPT: &str = r#"
const report = (step, data = {}) =>
  fetch("/result", { method: "POST", body: JSON.stringify({ step, ...data }) });
const settle = (promise) => promise.then(
  (value) => ({ value: value ?? null }), (error) => ({ error: String(error.message) }));
(async () => {
  const bridge = window.nonstopvibin;
  if (new URLSearchParams(location.search).get("view") === "tray") return report("tray");
  const calls = {
    getLoginItem: await settle(bridge.getLoginItem()),
    setLoginItem: await settle(bridge.setLoginItem(true)),
    getLoginItemAfter: await settle(bridge.getLoginItem()),
    openExternal: await settle(bridge.openExternal("https://claude.ai/login")),
    openDenied: await settle(bridge.openExternal("https://evil.example/")),
    revealData: await settle(bridge.revealData()),
  };
  window.open("https://chatgpt.com/");
  await report("main", calls);
})();
"#;

/// The page the helper shows, and the reports it posts.
fn serve_page() -> (u16, Receiver<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, results) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let sender = sender.clone();
            thread::spawn(move || answer(stream, &sender));
        }
    });
    (port, results)
}

fn answer(mut stream: TcpStream, results: &Sender<Value>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    let mut length = 0;
    reader.read_line(&mut request).unwrap_or_default();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap_or_default();
    let target = request.split_whitespace().nth(1).unwrap_or("");
    let (kind, content) = match target.split('?').next() {
        Some("/result") => {
            drop(results.send(serde_json::from_slice(&body).unwrap_or(Value::Null)));
            ("text/plain", "")
        }
        Some("/page.js") => ("text/javascript", SCRIPT),
        _ => ("text/html; charset=utf-8", PAGE),
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Security-Policy: {CSP}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        content.len()
    );
    drop(
        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(content.as_bytes())),
    );
}

fn next_report(results: &Receiver<Value>, step: &str) -> Value {
    loop {
        let report = results.recv_timeout(TIMEOUT).unwrap();
        if report["step"] == step {
            return report;
        }
    }
}

/// A tray host's side of the `StatusNotifierItem` protocol.
struct Watcher {
    items: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
#[expect(
    clippy::unused_self,
    reason = "D-Bus methods belong to the served object"
)]
impl Watcher {
    fn register_status_notifier_item(&self, service: String) {
        self.items.lock().unwrap().push(service);
    }
    fn register_status_notifier_host(&self, service: String) {
        drop(service);
    }
    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().unwrap().clone()
    }
    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
}

/// Serves the watcher: tray items register with it.
fn watch_trays(
    runtime: &tokio::runtime::Runtime,
    items: &Arc<Mutex<Vec<String>>>,
) -> zbus::Connection {
    let watcher = Watcher {
        items: Arc::clone(items),
    };
    runtime.block_on(async {
        zbus::connection::Builder::session()
            .unwrap()
            .name("org.kde.StatusNotifierWatcher")
            .unwrap()
            .serve_at("/StatusNotifierWatcher", watcher)
            .unwrap()
            .build()
            .await
            .unwrap()
    })
}

/// The file manager's `ShowItems`, recorded.
struct FileManager {
    shown: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.freedesktop.FileManager1")]
impl FileManager {
    fn show_items(&self, uris: Vec<String>, startup_id: String) {
        drop(startup_id);
        self.shown.lock().unwrap().extend(uris);
    }
}

type Pixmap = (i32, i32, Vec<u8>);

/// The tray item as a host sees it.
struct Item {
    runtime: tokio::runtime::Runtime,
    bus: zbus::Connection,
    service: String,
}

impl Item {
    fn proxy(&self, path: &str, interface: &str) -> zbus::Proxy<'static> {
        self.runtime.block_on(async {
            zbus::proxy::Builder::new(&self.bus)
                .destination(self.service.clone())
                .unwrap()
                .path(path.to_owned())
                .unwrap()
                .interface(interface.to_owned())
                .unwrap()
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build()
                .await
                .unwrap()
        })
    }

    fn sni(&self) -> zbus::Proxy<'static> {
        self.proxy("/StatusNotifierItem", "org.kde.StatusNotifierItem")
    }

    fn look(&self) -> (String, Vec<Pixmap>) {
        let sni = self.sni();
        self.runtime.block_on(async {
            let tooltip: (String, Vec<Pixmap>, String, String) =
                sni.get_property("ToolTip").await.unwrap();
            (tooltip.2, sni.get_property("IconPixmap").await.unwrap())
        })
    }

    fn activate(&self) {
        let sni = self.sni();
        self.runtime
            .block_on(sni.call::<_, _, ()>("Activate", &(10i32, 20i32)))
            .unwrap();
    }

    /// Menu ids by label, in order, with whether each is enabled; separators as "---".
    fn menu(&self) -> Vec<(i32, String, bool)> {
        let menu = self.proxy("/MenuBar", "com.canonical.dbusmenu");
        let items: Vec<(i32, HashMap<String, OwnedValue>)> = self
            .runtime
            .block_on(menu.call(
                "GetGroupProperties",
                &(Vec::<i32>::new(), Vec::<String>::new()),
            ))
            .unwrap();
        let text = |value: &OwnedValue| String::try_from(value.try_clone().unwrap()).unwrap();
        let sorted: BTreeMap<i32, HashMap<String, OwnedValue>> = items.into_iter().collect();
        sorted
            .into_iter()
            .filter(|(id, _)| *id != 0)
            .map(|(id, properties)| {
                let label = if properties.get("type").map(text).as_deref() == Some("separator") {
                    "---".to_owned()
                } else {
                    properties.get("label").map_or_else(String::new, text)
                };
                let enabled = properties
                    .get("enabled")
                    .is_none_or(|value| bool::try_from(value.try_clone().unwrap()).unwrap());
                (id, label, enabled)
            })
            .collect()
    }

    fn click(&self, label: &str) {
        let id = self
            .menu()
            .into_iter()
            .find(|(_, shown, _)| shown == label)
            .unwrap()
            .0;
        let menu = self.proxy("/MenuBar", "com.canonical.dbusmenu");
        self.runtime
            .block_on(menu.call::<_, _, ()>("Event", &(id, "clicked", Variant::from(0i32), 0u32)))
            .unwrap();
    }

    fn registered(&self) -> bool {
        self.runtime.block_on(async {
            let bus = zbus::fdo::DBusProxy::new(&self.bus).await.unwrap();
            let name = zbus::names::BusName::try_from(self.service.as_str()).unwrap();
            bus.name_has_owner(name).await.unwrap()
        })
    }
}

/// The icon a tray host draws: ARGB32 in network byte order.
fn pixmap(png_bytes: &[u8]) -> Vec<Pixmap> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::ALPHA);
    let mut reader = decoder.read_info().unwrap();
    let mut data = vec![0; reader.output_buffer_size().unwrap()];
    let frame = reader.next_frame(&mut data).unwrap();
    for pixel in data.as_chunks_mut::<4>().0 {
        *pixel = [pixel[3], pixel[0], pixel[1], pixel[2]];
    }
    let size = |value: u32| i32::try_from(value).unwrap();
    vec![(size(frame.width), size(frame.height), data)]
}

/// Window-helper processes and the WebKit processes they start.
fn window_processes() -> Vec<u32> {
    fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            fs::read_to_string(format!("/proc/{pid}/comm"))
                .is_ok_and(|comm| comm.starts_with("nonstopvibin-wi") || comm.starts_with("WebKit"))
                && !exited(*pid)
        })
        .collect()
}

fn helper_pid() -> u32 {
    let mut found = None;
    wait_until("a window helper runs", || {
        found = window_processes().into_iter().find(|pid| {
            fs::read_to_string(format!("/proc/{pid}/comm"))
                .is_ok_and(|comm| comm.starts_with("nonstopvibin-wi"))
        });
        found.is_some()
    });
    found.unwrap()
}

/// Where cargo put this profile's executables: the helper and examples/idle.
fn artifacts() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"),
        PathBuf::from,
    );
    target.join(if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    })
}

fn helper_binary() -> PathBuf {
    let path = artifacts().join("nonstopvibin-window");
    assert!(
        path.exists(),
        "build the helper first: cargo build -p nsv-window"
    );
    path
}

fn read_log(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

#[test]
#[ignore = "needs Xvfb, a session bus and the helper: run tests/gui.sh"]
#[expect(clippy::too_many_lines, reason = "one user's session, step by step")]
fn the_tray_drives_the_helper_and_quits_cleanly() {
    let opened = PathBuf::from(std::env::var_os("NSV_OPENED").unwrap());
    drop(fs::remove_file(&opened));
    let data = directory("desktop");
    let config = data.join("config");
    let (port, results) = serve_page();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let items = Arc::new(Mutex::new(Vec::new()));
    let shown = Arc::new(Mutex::new(Vec::new()));
    let _watcher = watch_trays(&runtime, &items);
    let (_files, bus) = runtime.block_on(async {
        let files = zbus::connection::Builder::session()
            .unwrap()
            .name("org.freedesktop.FileManager1")
            .unwrap()
            .serve_at(
                "/org/freedesktop/FileManager1",
                FileManager {
                    shown: Arc::clone(&shown),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        (files, zbus::Connection::session().await.unwrap())
    });

    // --background: the tray registers, and no window or WebKit process starts.
    let host = Host::new(port, &data);
    let shell = spawn_shell(&host, options(helper_binary(), false, true, &config));
    wait_until("the tray registers", || !items.lock().unwrap().is_empty());
    let item = Item {
        runtime,
        bus,
        service: items.lock().unwrap()[0].clone(),
    };
    pause(Duration::from_millis(500));
    assert_eq!(window_processes(), Vec::<u32>::new());

    // The icon and tooltip follow the status.
    let stopped = pixmap(include_bytes!("../../../../public/tray/linux-stopped.png"));
    let ball = pixmap(include_bytes!("../../../../public/tray/linux-ball.png"));
    assert_eq!(
        item.look(),
        ("NonstopVibin · proxy off".to_owned(), stopped)
    );
    host.status.send_replace(TrayStatus {
        running: vec!["Work".to_owned(), "Side".to_owned()],
        streaming: true,
    });
    let streaming = ("NonstopVibin — Work, Side · streaming".to_owned(), ball);
    wait_until("the tray shows the running profiles", || {
        item.look() == streaming
    });

    let menu: Vec<(String, bool)> = item
        .menu()
        .into_iter()
        .map(|(_, label, enabled)| (label, enabled))
        .collect();
    assert_eq!(
        menu,
        [
            ("Open NonstopVibin".to_owned(), true),
            ("Subscription quotas".to_owned(), true),
            ("Check for updates…".to_owned(), false),
            ("---".to_owned(), true),
            ("Quit NonstopVibin".to_owned(), true),
        ]
    );

    // "Open NonstopVibin" starts the helper; the page's daemon calls come back answered.
    item.click("Open NonstopVibin");
    assert_eq!(
        next_report(&results, "main"),
        json!({
            "step": "main",
            "getLoginItem": {"value": false},
            "setLoginItem": {"value": true},
            "getLoginItemAfter": {"value": true},
            "openExternal": {"value": null},
            "openDenied": {"error": "This external URL is not an approved provider sign-in page."},
            "revealData": {"value": null},
        })
    );
    assert!(
        read_log(&config.join("autostart/app.nonstopvibin.desktop"))
            .contains("\nExec=\"/opt/NonstopVibin/nonstopvibin\" --background\n")
    );
    let database = format!(
        "file://{}",
        nsv_store::layout::database_path(&data).display()
    );
    assert_eq!(*shown.lock().unwrap(), [database]);
    // The approved call, then the page's denied window through the same allowlist.
    wait_until("the browser opens both pages", || {
        read_log(&opened) == "https://claude.ai/login\nhttps://chatgpt.com/\n"
    });

    // A click opens the popup, the next closes it, a later one opens it again.
    item.activate();
    next_report(&results, "tray");
    item.activate();
    pause(Duration::from_millis(600));
    item.activate();
    next_report(&results, "tray");

    // A crashed helper is replaced on the next click.
    let crashed = helper_pid();
    kill(crashed);
    wait_until("the helper is gone", || exited(crashed));
    pause(Duration::from_millis(300));
    item.activate();
    next_report(&results, "tray");
    assert_ne!(helper_pid(), crashed);

    // Quit ends the shell, the helper and its WebKit processes, and the tray item.
    item.click("Quit NonstopVibin");
    shell.join().unwrap();
    assert!(host.shut_down.load(Ordering::SeqCst));
    wait_until("no window process remains", || {
        window_processes().is_empty()
    });
    wait_until("the tray item leaves the bus", || !item.registered());
    assert!(host.reports().is_empty(), "{:?}", host.reports());
    fs::remove_dir_all(&data).unwrap();
}

fn kill(pid: u32) {
    let status = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Context switches of every thread of `pid`, by thread name: each one is a wakeup.
fn switches(pid: u32) -> BTreeMap<String, u64> {
    let mut threads = BTreeMap::new();
    for task in fs::read_dir(format!("/proc/{pid}/task")).unwrap().flatten() {
        let status = fs::read_to_string(task.path().join("status")).unwrap();
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .unwrap()
                .trim()
                .to_owned()
        };
        let count = field("voluntary_ctxt_switches:").parse::<u64>().unwrap()
            + field("nonvoluntary_ctxt_switches:").parse::<u64>().unwrap();
        threads.insert(
            format!("{}:{}", field("Name:"), task.file_name().to_string_lossy()),
            count,
        );
    }
    threads
}

fn pss_kib(pid: u32) -> u64 {
    fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("Pss:"))
        .unwrap()
        .trim()
        .trim_end_matches(" kB")
        .parse()
        .unwrap()
}

/// The shell alone (examples/idle.rs), its tray registered, idle for 60 s: its PSS and its
/// wakeups per second, per thread.
#[test]
#[ignore = "takes a minute: run tests/gui.sh with --release and this test's name"]
fn measure_idle() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let items = Arc::new(Mutex::new(Vec::new()));
    let _watcher = watch_trays(&runtime, &items);
    let example = artifacts().join("examples/idle");
    let mut idle = std::process::Command::new(example).spawn().unwrap();
    wait_until("the idle shell's tray registers", || {
        !items.lock().unwrap().is_empty()
    });
    pause(Duration::from_secs(5));
    let before = switches(idle.id());
    let seconds = 60;
    pause(Duration::from_secs(seconds));
    let after = switches(idle.id());
    let pss = pss_kib(idle.id());
    idle.kill().unwrap();
    idle.wait().unwrap();
    let mut total = 0;
    let mut lines = Vec::new();
    for (thread, count) in &after {
        let delta = count - before.get(thread).copied().unwrap_or(0);
        total += delta;
        lines.push(format!("  {thread}: {delta}"));
    }
    #[expect(clippy::print_stderr, reason = "the measurement's report")]
    {
        eprintln!(
            "idle shell: PSS {pss} KiB, {} threads, {total} wakeups in {seconds} s\n{}",
            after.len(),
            lines.join("\n")
        );
    }
}
