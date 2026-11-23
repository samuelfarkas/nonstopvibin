//! What the replays share: temporary directories, the goldens, a stub for outside requests,
//! raw HTTP and the recorder's placeholders.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use nsv_accounts::fetch::{Fetch, Request};
use nsv_net::{HeaderMap, Loopback, NetError, Response, StatusCode};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub(crate) const REPOSITORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// The recorder's fixed clock, `2026-11-22T12:00:00.000Z`.
pub(crate) const NOW: i64 = 1_795_348_800_000;

pub(crate) fn now() -> i64 {
    NOW
}

/// A directory under the system temporary directory, removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "nsv-server-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The cases of `tests/golden/<area>.json`.
pub(crate) fn golden(area: &str) -> Vec<Value> {
    let path = Path::new(REPOSITORY).join(format!("tests/golden/{area}.json"));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// models.dev answers with the recorder's fixed catalog; every other outside request fails.
#[derive(Clone)]
pub(crate) struct ModelsDev(Arc<Vec<u8>>);

impl ModelsDev {
    pub(crate) fn new() -> Self {
        let usage = golden("usage");
        let payload = usage
            .iter()
            .find(|case| case["name"] == "catalog payload")
            .unwrap()["output"]
            .clone();
        Self(Arc::new(serde_json::to_vec(&payload).unwrap()))
    }
}

impl Fetch for ModelsDev {
    fn https(&self, request: Request) -> impl Future<Output = Result<Response, NetError>> + Send {
        std::future::ready(if request.url == "https://models.dev/api.json" {
            Ok(Response {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: self.0.to_vec(),
            })
        } else {
            Err(NetError::Invalid("offline"))
        })
    }

    fn loopback(
        &self,
        _: Loopback,
        _: String,
        _: Request,
    ) -> impl Future<Output = Result<Response, NetError>> + Send {
        std::future::ready(Err(NetError::Invalid("offline")))
    }
}

pub(crate) struct RawResponse {
    pub(crate) status: u16,
    /// Lowercase names, in order.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl RawResponse {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

fn dechunk(mut rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line) = rest.windows(2).position(|window| window == b"\r\n") {
        let size =
            usize::from_str_radix(std::str::from_utf8(&rest[..line]).unwrap().trim(), 16).unwrap();
        if size == 0 {
            break;
        }
        out.extend_from_slice(&rest[line + 2..line + 2 + size]);
        rest = &rest[line + 2 + size + 2..];
    }
    out
}

/// One request on a new connection, as the recorder's `send` wrote it, read to EOF.
pub(crate) async fn send(
    port: u16,
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> RawResponse {
    let mut head = format!("{method} {target} HTTP/1.1\r\n");
    if !headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("host"))
    {
        write!(head, "Host: 127.0.0.1:{port}\r\n").unwrap();
    }
    for (key, value) in headers {
        write!(head, "{key}: {value}\r\n").unwrap();
    }
    if !body.is_empty() {
        write!(head, "Content-Length: {}\r\n", body.len()).unwrap();
    }
    head.push_str("Connection: close\r\n\r\n");
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    // A refusal may close the connection before reading a large body.
    let _ = stream.write_all(body).await;
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await;
    let end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let text = String::from_utf8_lossy(&raw[..end]).into_owned();
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers: Vec<(String, String)> = lines
        .map(|line| {
            let (key, value) = line.split_once(':').unwrap();
            (key.to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect();
    let rest = &raw[end + 4..];
    let chunked = headers
        .iter()
        .any(|(key, value)| key == "transfer-encoding" && value == "chunked");
    RawResponse {
        status,
        headers,
        body: if chunked {
            dechunk(rest)
        } else {
            rest.to_vec()
        },
    }
}

/// Generated values and their placeholders, replaced longest first.
#[derive(Default)]
pub(crate) struct Placeholders(Vec<(String, String)>);

impl Placeholders {
    pub(crate) fn add(&mut self, value: impl Into<String>, placeholder: &str) {
        let value = value.into();
        if !value.is_empty() {
            self.0.push((value, placeholder.to_owned()));
            self.0
                .sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
        }
    }

    pub(crate) fn has(&self, placeholder: &str) -> bool {
        self.0.iter().any(|(_, known)| known == placeholder)
    }

    pub(crate) fn text(&self, value: &str) -> String {
        let mut text = value.to_owned();
        for (from, to) in &self.0 {
            text = text.replace(from, to);
        }
        text
    }

    /// The other way: the recorder's input with this run's values.
    pub(crate) fn fill(&self, value: &str) -> String {
        let mut text = value.to_owned();
        for (to, from) in &self.0 {
            text = text.replace(from, to);
        }
        text
    }

    pub(crate) fn json(&self, value: &Value) -> Value {
        serde_json::from_str(&self.text(&value.to_string())).unwrap()
    }
}
