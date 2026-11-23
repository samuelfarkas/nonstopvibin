//! The origin the helper opens for a UI check: files from a directory, or the daemon behind a
//! proxy. Either way every HTML page gets the check script (`checks/<name>.js` after
//! `checks/prelude.js`) before its own scripts, and the page reaches the test through two
//! endpoints: `POST /__report` (a step's result) and `POST /__act` (a native action the test
//! performs before it answers).

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nsv_domain::helper::Origin;
use serde_json::{Value, json};

const PRELUDE: &str = include_str!("checks/prelude.js");
/// How long a check may take overall, and so how long a page waits for an action.
pub(crate) const CHECK_TIMEOUT: Duration = Duration::from_secs(90);

pub(crate) enum Content {
    /// A directory, with the page `/` serves.
    Files(PathBuf, &'static str),
    /// The daemon on this loopback port. The proxy rewrites Host and Origin to the daemon's,
    /// so its checks pass as they do for the helper it opens itself.
    Daemon(u16),
    /// The daemon, framed: `/` is an empty page for the check, and the daemon's pages may be
    /// framed by it.
    Framed(u16),
}

const HOST_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>host</title><body style="margin:0"><script src="/__check.js"></script></body>"#;

/// What the page sent.
pub(crate) enum Event {
    Report(Value),
    /// An action; the page waits for the answer sent back on the channel.
    Act(Value, Sender<Value>),
}

struct Shared {
    content: Content,
    script: Mutex<String>,
    /// Paths answered 404 on purpose, and the requests for them.
    refuse: fn(&str) -> bool,
    refused: Mutex<Vec<String>>,
}

pub(crate) struct Site {
    pub(crate) port: u16,
    events: Receiver<Event>,
    shared: Arc<Shared>,
}

impl Site {
    pub(crate) fn start(content: Content, check: &str) -> Self {
        Self::refusing(content, check, |_| false)
    }

    /// Like `start`, but answers 404 for every path `refuse` matches.
    pub(crate) fn refusing(content: Content, check: &str, refuse: fn(&str) -> bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = Arc::new(Shared {
            content,
            script: Mutex::new(String::new()),
            refuse,
            refused: Mutex::new(Vec::new()),
        });
        let (sender, events) = mpsc::channel();
        let serving = Arc::clone(&shared);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (sender, shared) = (sender.clone(), Arc::clone(&serving));
                thread::spawn(move || serve(stream, port, &shared, &sender));
            }
        });
        let site = Self {
            port,
            events,
            shared,
        };
        site.check(check);
        site
    }

    /// The check later page loads run.
    pub(crate) fn check(&self, name: &str) {
        self.check_with(name, &Value::Null);
    }

    /// The check later page loads run, which reads `config`.
    pub(crate) fn check_with(&self, name: &str, config: &Value) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/ui/checks/{name}.js"));
        let check = fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path:?}: {error}"));
        *self.shared.script.lock().unwrap() =
            format!("const config = {config};\n{PRELUDE}\n{check}");
    }

    pub(crate) fn origin(&self) -> Origin {
        Origin::parse(&format!("http://127.0.0.1:{}", self.port)).unwrap()
    }

    /// What the page sent within `wait`, if anything.
    pub(crate) fn poll(&self, wait: Duration) -> Option<Event> {
        self.events.recv_timeout(wait).ok()
    }

    /// Requests answered 404 by `refusing`, in order.
    pub(crate) fn refused(&self) -> Vec<String> {
        self.shared.refused.lock().unwrap().clone()
    }
}

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn read(stream: &TcpStream) -> Option<Self> {
        let mut reader = BufReader::new(stream.try_clone().ok()?);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let mut parts = line.split_whitespace();
        let (method, target) = (parts.next()?.to_owned(), parts.next()?.to_owned());
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 || line == "\r\n" {
                break;
            }
            let (name, value) = line.split_once(':')?;
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
        let length = header(&headers, "content-length").map_or(0, |value| value.parse().unwrap());
        let mut body = vec![0; length];
        reader.read_exact(&mut body).ok()?;
        Some(Self {
            method,
            target,
            headers,
            body,
        })
    }

    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

struct Response {
    status: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn new(status: &str, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status: status.to_owned(),
            headers: vec![("content-type".to_owned(), content_type.to_owned())],
            body,
        }
    }

    fn write(mut self, stream: &mut TcpStream) {
        self.headers
            .retain(|(name, _)| !matches!(name.as_str(), "content-length" | "connection"));
        let mut head = format!(
            "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.status,
            self.body.len()
        );
        for (name, value) in &self.headers {
            write!(head, "{name}: {value}\r\n").unwrap();
        }
        head.push_str("\r\n");
        let written = stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(&self.body));
        // The page may have navigated away from a request it no longer wants.
        written.unwrap_or_default();
    }

    /// Puts the check script ahead of the page's first script.
    fn inject(&mut self) {
        let html =
            header(&self.headers, "content-type").is_some_and(|kind| kind.starts_with("text/html"));
        if !html {
            return;
        }
        let page = String::from_utf8(std::mem::take(&mut self.body)).unwrap();
        let tag = r#"<script src="/__check.js"></script>"#;
        let (before, after) = page.split_at(page.find("<script").unwrap_or(page.len()));
        self.body = format!("{before}{tag}{after}").into_bytes();
    }
}

fn serve(mut stream: TcpStream, port: u16, shared: &Shared, events: &Sender<Event>) {
    let Some(request) = Request::read(&stream) else {
        return;
    };
    let path = request.path().to_owned();
    let response = match (request.method.as_str(), path.as_str()) {
        ("GET", "/__check.js") => Response::new(
            "200 OK",
            "text/javascript",
            shared.script.lock().unwrap().clone().into_bytes(),
        ),
        ("POST", "/__report") => {
            let report =
                serde_json::from_slice(&request.body).unwrap_or(json!({ "step": "unparsable" }));
            events.send(Event::Report(report)).unwrap_or_default();
            Response::new("204 No Content", "text/plain", Vec::new())
        }
        ("POST", "/__act") => {
            let action = serde_json::from_slice(&request.body).unwrap();
            let (reply, answer) = mpsc::channel();
            events.send(Event::Act(action, reply)).unwrap_or_default();
            let answer = answer.recv_timeout(CHECK_TIMEOUT).unwrap_or(Value::Null);
            Response::new(
                "200 OK",
                "application/json",
                answer.to_string().into_bytes(),
            )
        }
        _ if (shared.refuse)(&path) => {
            shared.refused.lock().unwrap().push(path);
            Response::new("404 Not Found", "text/plain", Vec::new())
        }
        _ => {
            let mut response = match &shared.content {
                Content::Files(root, index) => file(root, if path == "/" { index } else { &path }),
                Content::Framed(_) if request.target == "/" => {
                    Response::new("200 OK", "text/html; charset=utf-8", HOST_PAGE.into())
                }
                Content::Daemon(daemon) => forward(&request, port, *daemon),
                Content::Framed(daemon) => {
                    let mut response = forward(&request, port, *daemon);
                    for (name, value) in &mut response.headers {
                        if name == "content-security-policy" {
                            *value =
                                value.replace("frame-ancestors 'none'", "frame-ancestors 'self'");
                        }
                    }
                    response
                }
            };
            response.inject();
            response
        }
    };
    response.write(&mut stream);
}

fn file(root: &Path, path: &str) -> Response {
    let relative = path.trim_start_matches('/');
    let Ok(body) = fs::read(root.join(relative)) else {
        return Response::new("404 Not Found", "text/plain", Vec::new());
    };
    let kind = match Path::new(relative)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    Response::new("200 OK", kind, body)
}

/// One request to the daemon over HTTP/1.0, so its answer is never chunked.
fn forward(request: &Request, port: u16, daemon: u16) -> Response {
    let mut upstream = TcpStream::connect(("127.0.0.1", daemon)).unwrap();
    let mut head = format!("{} {} HTTP/1.0\r\n", request.method, request.target);
    let (ours, theirs) = (
        format!("http://127.0.0.1:{port}"),
        format!("http://127.0.0.1:{daemon}"),
    );
    for (name, value) in &request.headers {
        let value = match name.as_str() {
            "connection" | "keep-alive" => continue,
            "host" => theirs.trim_start_matches("http://"),
            "origin" | "referer" => &value.replacen(&ours, &theirs, 1),
            _ => value,
        };
        write!(head, "{name}: {value}\r\n").unwrap();
    }
    head.push_str("\r\n");
    upstream.write_all(head.as_bytes()).unwrap();
    upstream.write_all(&request.body).unwrap();
    let mut answer = Vec::new();
    upstream.read_to_end(&mut answer).unwrap();
    let split = answer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let head = String::from_utf8_lossy(&answer[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_once(' '))
        .map(|(_, status)| status.to_owned())
        .unwrap();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Response {
        status,
        headers,
        body: answer[split + 4..].to_vec(),
    }
}
