//! The harness that plays the daemon: an HTTP server for the page and its reports, and a
//! driver for the helper's pipe. Shared by the end-to-end tests and the latency example.

#[cfg(target_os = "linux")]
pub(crate) mod x11;

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nsv_domain::helper::{self, DaemonCall, FromHelper, Origin, Outcome, Rect, ToHelper, View};
use serde_json::{Value, json};

pub(crate) const TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const TOKEN: &str = "session-token-for-the-harness";
/// `src/server/server.ts`'s header, byte for byte.
pub(crate) const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'";
const PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>harness</title><body><script src="/page.js"></script></body>"#;
const SCRIPT: &str = include_str!("page.js");

/// Waits without a runtime to block: the tests and the example are plain threads.
#[expect(clippy::disallowed_methods, reason = "harness threads, not a runtime")]
pub(crate) fn pause(duration: Duration) {
    thread::sleep(duration);
}

/// Polls `ready` every 50 ms until it holds or `TIMEOUT` passes.
pub(crate) fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        pause(Duration::from_millis(50));
    }
}

pub(crate) struct Server {
    pub(crate) port: u16,
    results: Receiver<Value>,
    seen: Vec<Value>,
    commands: Sender<String>,
}

impl Server {
    pub(crate) fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sender, results) = mpsc::channel();
        let (commands, pending) = mpsc::channel::<String>();
        let pending = Arc::new(Mutex::new(pending));
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (sender, pending) = (sender.clone(), Arc::clone(&pending));
                thread::spawn(move || serve(stream, port, &sender, &pending));
            }
        });
        Self {
            port,
            results,
            seen: Vec::new(),
            commands,
        }
    }

    pub(crate) fn origin(&self) -> Origin {
        Origin::parse(&format!("http://127.0.0.1:{}", self.port)).unwrap()
    }

    /// Answers the page's next `GET /next`.
    pub(crate) fn command(&self, command: &str) {
        self.commands.send(command.to_owned()).unwrap();
    }

    /// The page's report of `step`, waiting for it.
    pub(crate) fn result(&mut self, step: &str) -> Value {
        self.any(&[step])
    }

    /// The first report of any of `steps`.
    pub(crate) fn any(&mut self, steps: &[&str]) -> Value {
        let wanted = |value: &Value| steps.iter().any(|step| value["step"] == *step);
        if let Some(index) = self.seen.iter().position(wanted) {
            return self.seen.remove(index);
        }
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(value) = self.results.recv_timeout(left) else {
                panic!("no report of {steps:?}; reports so far: {:?}", self.seen);
            };
            if wanted(&value) {
                return value;
            }
            self.seen.push(value);
        }
    }
}

fn serve(
    mut stream: TcpStream,
    port: u16,
    results: &Sender<Value>,
    pending: &Mutex<Receiver<String>>,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return;
    }
    let (mut host, mut length) = (String::new(), 0);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap_or_default();
        match name.to_ascii_lowercase().as_str() {
            "host" => value.trim().clone_into(&mut host),
            "content-length" => length = value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let mut parts = request.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = target.split('?').next().unwrap_or("");
    let html = ("Content-Type", "text/html; charset=utf-8".to_owned());
    let (status, headers, body): (&str, Vec<(&str, String)>, Vec<u8>) = match (method, path) {
        ("POST", "/result") => {
            let report = serde_json::from_slice(&body).unwrap_or(json!({ "step": "unparsable" }));
            results.send(report).unwrap_or_default();
            ("204 No Content", Vec::new(), Vec::new())
        }
        ("GET", "/next") => {
            let command = pending
                .lock()
                .unwrap()
                .recv_timeout(TIMEOUT)
                .unwrap_or_default();
            (
                "200 OK",
                vec![("Content-Type", "text/plain".to_owned())],
                command.into_bytes(),
            )
        }
        ("GET", "/") if host == format!("127.0.0.1:{port}") => (
            "200 OK",
            vec![html, ("Content-Security-Policy", CSP.to_owned())],
            PAGE.into(),
        ),
        // The probe's sandboxed frame and the page a redirect leads to: no CSP of their own.
        ("GET", "/sandboxed" | "/stray") => ("200 OK", vec![html], PAGE.into()),
        ("GET", "/page.js") => (
            "200 OK",
            vec![("Content-Type", "text/javascript".to_owned())],
            SCRIPT.into(),
        ),
        ("GET", "/other") => (
            "302 Found",
            vec![("Location", format!("http://localhost:{port}/stray"))],
            Vec::new(),
        ),
        ("GET", "/download") => (
            "200 OK",
            vec![
                ("Content-Type", "application/octet-stream".to_owned()),
                (
                    "Content-Disposition",
                    "attachment; filename=\"harness.bin\"".to_owned(),
                ),
            ],
            b"not to be saved".to_vec(),
        ),
        _ => ("404 Not Found", Vec::new(), Vec::new()),
    };
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        write!(head, "{name}: {value}\r\n").unwrap();
    }
    head.push_str("\r\n");
    let written = stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(&body));
    written.unwrap_or_default();
}

/// The daemon's answers to forwarded bridge calls.
pub(crate) fn answer(call: &DaemonCall) -> Outcome {
    match call {
        DaemonCall::OpenExternal(_) => {
            Outcome::Error("This external URL is not an approved provider sign-in page.".to_owned())
        }
        DaemonCall::GetLoginItem | DaemonCall::SetLoginItem(_) => Outcome::Result(json!(true)),
        DaemonCall::RevealData => Outcome::Result(Value::Null),
    }
}

/// A running helper, with the daemon's end of its pipe. Bridge calls are answered by
/// [`answer`] as they arrive; every message is also kept for the test to read.
pub(crate) struct Helper {
    child: Child,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    events: Receiver<FromHelper>,
    stderr: Arc<Mutex<String>>,
}

impl Helper {
    pub(crate) fn start(binary: &Path, origin: &Origin, view: View, directory: &Path) -> Self {
        Self::start_at(binary, origin, view, None, directory)
    }

    /// Starts the helper with the main window's last `bounds`.
    pub(crate) fn start_at(
        binary: &Path,
        origin: &Origin,
        view: View,
        bounds: Option<Rect>,
        directory: &Path,
    ) -> Self {
        Self::launch(binary, origin, view, bounds, directory, TOKEN)
    }

    /// Starts the helper with the daemon's session `token`.
    pub(crate) fn launch(
        binary: &Path,
        origin: &Origin,
        view: View,
        bounds: Option<Rect>,
        directory: &Path,
        token: &str,
    ) -> Self {
        let home = directory.join("home");
        fs::create_dir_all(home.join("Downloads")).unwrap();
        let mut child = Command::new(binary)
            .env("HOME", &home)
            .env("XDG_DOWNLOAD_DIR", home.join("Downloads"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let start = ToHelper::Start {
            origin: origin.clone(),
            view,
            bounds,
            data_directory: directory.to_owned(),
            updates: false,
        };
        helper::write(&mut input, &start).unwrap();
        writeln!(input, "{token}").unwrap();
        let stdin = Arc::new(Mutex::new(Some(input)));
        let (sender, events) = mpsc::channel();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let replies = Arc::clone(&stdin);
        thread::spawn(move || {
            while let Ok(Some(event)) = helper::read::<FromHelper>(&mut output) {
                if let FromHelper::Bridge { id, call } = &event {
                    let reply = ToHelper::Reply {
                        id: *id,
                        outcome: answer(call),
                    };
                    if let Some(input) = replies.lock().unwrap().as_mut() {
                        helper::write(input, &reply).unwrap_or_default();
                    }
                }
                if sender.send(event).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let log = Arc::clone(&stderr);
        let mut errors = BufReader::new(child.stderr.take().unwrap());
        thread::spawn(move || {
            let mut line = String::new();
            while errors.read_line(&mut line).unwrap_or(0) > 0 {
                log.lock().unwrap().push_str(&line);
                line.clear();
            }
        });
        Self {
            child,
            stdin,
            events,
            stderr,
        }
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn send(&self, message: &ToHelper) {
        let mut stdin = self.stdin.lock().unwrap();
        helper::write(stdin.as_mut().unwrap(), message).unwrap();
    }

    pub(crate) fn close_stdin(&self) {
        self.stdin.lock().unwrap().take();
    }

    /// The next message, waiting for it.
    pub(crate) fn next(&self) -> FromHelper {
        self.events.recv_timeout(TIMEOUT).unwrap_or_else(|error| {
            panic!(
                "no message from the helper ({error}); its log:\n{}",
                self.log()
            )
        })
    }

    /// The next message that is not a bridge call.
    pub(crate) fn next_event(&self) -> FromHelper {
        loop {
            match self.next() {
                FromHelper::Bridge { .. } => {}
                event => return event,
            }
        }
    }

    /// Every message that arrives within `quiet`.
    pub(crate) fn drain(&self, quiet: Duration) -> Vec<FromHelper> {
        let mut events = Vec::new();
        loop {
            match self.events.recv_timeout(quiet) {
                Ok(event) => events.push(event),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return events,
            }
        }
    }

    pub(crate) fn log(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    pub(crate) fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the helper did not exit; its log:\n{}",
                self.log()
            );
            pause(Duration::from_millis(20));
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.child.kill().unwrap_or_default();
            self.child.wait().unwrap_or_else(|error| panic!("{error}"));
        }
    }
}

/// Live processes (not zombies), as pid → (parent pid, command name).
pub(crate) fn processes() -> HashMap<u32, (u32, String)> {
    let mut processes = HashMap::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let mut fields = stat.get(close + 2..).unwrap_or("").split_whitespace();
        let (state, parent) = (fields.next().unwrap_or("Z"), fields.next().unwrap_or("0"));
        if state != "Z" {
            processes.insert(
                pid,
                (
                    parent.parse().unwrap_or(0),
                    stat.get(open + 1..close).unwrap_or("").to_owned(),
                ),
            );
        }
    }
    processes
}

/// `root` and every live process below it.
pub(crate) fn tree(root: u32) -> HashSet<u32> {
    let processes = processes();
    let mut found = HashSet::from([root]);
    loop {
        let more: Vec<u32> = processes
            .iter()
            .filter(|(pid, (parent, _))| found.contains(parent) && !found.contains(pid))
            .map(|(pid, _)| *pid)
            .collect();
        if more.is_empty() {
            return found
                .into_iter()
                .filter(|pid| processes.contains_key(pid))
                .collect();
        }
        found.extend(more);
    }
}
