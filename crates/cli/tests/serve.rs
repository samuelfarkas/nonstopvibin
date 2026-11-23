//! `nonstopvibin serve` as a process with the real pinned core (`bun run core:install` puts it
//! in `.vendor/core`), ported from the TypeScript server-mode, server-lifecycle, lazy-cores and
//! native-anthropic suites: signals and orderly shutdown, one instance per data directory, a
//! stale `server.json`, a killed daemon's lock, `--allow-host`, dormant cores, profile-key
//! isolation and a streamed request through the gateway to a stub upstream.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nsv_domain::secret::ApiKey;
use nsv_domain::wire::ApiAccount;
use nsv_store::{ServerLock, Store, Vault};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

struct Root(PathBuf);

impl Root {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("nsv-serve-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("home")).unwrap();
        Self(path)
    }

    fn data(&self) -> PathBuf {
        self.0.join("data")
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A loopback port nothing listens on now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Seeded {
    slug: String,
    key: String,
}

/// Enabled profiles, each with one API-key account on `upstream` and no OAuth account, as an
/// earlier run left them: they sleep until their first request.
async fn seed(data: &Path, count: usize, upstream: u16) -> Vec<Seeded> {
    nsv_store::files::owner_only_directory(data, || Ok::<(), std::io::Error>(())).unwrap();
    let store = Store::open(data, Vault::open(data).unwrap()).await.unwrap();
    let mut seeded = Vec::new();
    for index in 0..count {
        let seed = store
            .call(move |db| {
                let mut profile = db.create_profile(&format!("Bench {index}"))?;
                profile.enabled = true;
                db.save_profile(&profile)?;
                let account: ApiAccount = serde_json::from_value(json!({
                    "id": format!("account-{index}"), "name": "stub", "provider": "custom",
                    "baseUrl": format!("http://127.0.0.1:{upstream}/v1"), "prefix": "",
                    "models": [{ "id": "stub-model", "protocol": "openai" }], "disabled": false,
                }))?;
                db.save_api_account(&profile.id, &account, Some(&ApiKey::new("upstream-key")))?;
                let key = db.client_key(&profile.id)?;
                Ok(Seeded {
                    slug: profile.slug.as_str().to_owned(),
                    key: key.expose_secret().to_owned(),
                })
            })
            .await
            .unwrap();
        seeded.push(seed);
    }
    store.close().await;
    seeded
}

/// An OpenAI-compatible upstream that answers every chat completion with 11 prompt (5 of them
/// cached) and 3 completion tokens, a streamed one in three deltas. A chat whose prompt is `hang` gets no
/// answer.
struct Upstream {
    port: u16,
    /// Chat requests read so far.
    requests: Arc<AtomicUsize>,
    /// While false, a stream waits after its first delta.
    release: Arc<watch::Sender<bool>>,
    /// Connections of unanswered chats that the core still holds open.
    hanging: Arc<AtomicUsize>,
}

impl Upstream {
    fn hold(&self) {
        self.release.send_replace(false);
    }

    fn release(&self) {
        self.release.send_replace(true);
    }
}

async fn stub_upstream() -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = Upstream {
        port: listener.local_addr().unwrap().port(),
        requests: Arc::default(),
        release: Arc::new(watch::Sender::new(true)),
        hanging: Arc::default(),
    };
    let (requests, release, hanging) = (
        Arc::clone(&upstream.requests),
        Arc::clone(&upstream.release),
        Arc::clone(&upstream.hanging),
    );
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (requests, mut released, hanging) = (
                Arc::clone(&requests),
                release.subscribe(),
                Arc::clone(&hanging),
            );
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = vec![0; 8192];
                let text = loop {
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request).into_owned();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if body.len() >= length {
                            break text;
                        }
                    }
                };
                requests.fetch_add(1, Ordering::SeqCst);
                if text.contains(r#""content":"hang""#) {
                    hanging.fetch_add(1, Ordering::SeqCst);
                    while matches!(stream.read(&mut buffer).await, Ok(read) if read > 0) {}
                    hanging.fetch_sub(1, Ordering::SeqCst);
                    return;
                }
                if !text.contains(r#""stream":true"#) {
                    let body = json!({
                        "id": "chatcmpl-stub", "object": "chat.completion", "created": 1, "model": "stub-model",
                        "choices": [{ "index": 0, "message": { "role": "assistant", "content": "Hello from the stub" }, "finish_reason": "stop" }],
                        "usage": { "prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14, "prompt_tokens_details": { "cached_tokens": 5 } },
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    return;
                }
                let mut events = Vec::new();
                for (index, word) in ["Hello", " from", " the stub"].iter().enumerate() {
                    let chunk = json!({
                        "id": "chatcmpl-stub", "object": "chat.completion.chunk", "created": 1,
                        "model": "stub-model",
                        "choices": [{ "index": 0, "delta": { "content": word }, "finish_reason": null }],
                    });
                    let mut event = format!("data: {chunk}\n\n");
                    if index == 2 {
                        let done = json!({
                            "id": "chatcmpl-stub", "object": "chat.completion.chunk", "created": 1,
                            "model": "stub-model",
                            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
                            "usage": { "prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14, "prompt_tokens_details": { "cached_tokens": 5 } },
                        });
                        write!(event, "data: {done}\n\ndata: [DONE]\n\n").unwrap();
                    }
                    events.push(event);
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}",
                    events[0]
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = released.wait_for(|released| *released).await;
                let _ = stream.write_all(events[1..].concat().as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    upstream
}

struct Daemon {
    child: Child,
    port: u16,
    token: String,
    data: PathBuf,
}

/// A test that fails part-way leaves no daemon behind.
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    /// Runs `serve` and waits until its `server.json` names it.
    async fn serve(root: &Root, port: u16, extra: &[&str]) -> Self {
        Self::serve_with(command(root), root, port, extra).await
    }

    /// [`Daemon::serve`] with another executable or environment.
    async fn serve_with(mut command: Command, root: &Root, port: u16, extra: &[&str]) -> Self {
        let mut daemon = Self {
            child: command
                .args(["serve", "--port", &port.to_string()])
                .args(extra)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
            port,
            token: String::new(),
            data: root.data(),
        };
        let started = Instant::now();
        loop {
            if let Ok(text) = std::fs::read_to_string(daemon.data.join("server.json")) {
                let session: Value = serde_json::from_str(&text).unwrap();
                if session["pid"] == daemon.child.id() {
                    session["token"]
                        .as_str()
                        .unwrap()
                        .clone_into(&mut daemon.token);
                    return daemon;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "serve did not start"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn signal(&self, name: &str) {
        let status = Command::new("kill")
            .args([format!("-{name}"), self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Waits for the process to exit; its code and standard error.
    async fn exited(&mut self) -> (i32, String) {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                let mut stderr = String::new();
                std::io::Read::read_to_string(&mut self.child.stderr.take().unwrap(), &mut stderr)
                    .unwrap();
                return (status.code().unwrap_or(-1), stderr);
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the daemon did not exit"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn api(&self, method: &str, path: &str) -> Response {
        let authorization = format!("Bearer {}", self.token);
        send(
            self.port,
            method,
            path,
            &[("Authorization", &authorization)],
            "",
        )
        .await
    }
}

fn command(root: &Root) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nonstopvibin"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.0.join("home"))
        .env("NONSTOPVIBIN_DATA_DIR", root.data())
        .stdin(Stdio::null());
    command
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

/// One request on a new connection; a chunked body is joined.
async fn send(
    port: u16,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut wire = head(port, method, target, headers, body.len());
    wire.push_str("Connection: close\r\n\r\n");
    wire.push_str(body);
    stream.write_all(wire.as_bytes()).await.unwrap();
    read_response(&mut stream).await
}

/// A request head without its final empty line, with a Host unless `headers` name one.
fn head(port: u16, method: &str, target: &str, headers: &[(&str, &str)], length: usize) -> String {
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
    if length > 0 {
        write!(
            head,
            "Content-Type: application/json\r\nContent-Length: {length}\r\n"
        )
        .unwrap();
    }
    head
}

/// The status and headers of one response; the bytes read past them.
async fn read_head(stream: &mut TcpStream) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut raw = Vec::new();
    let mut buffer = vec![0; 8192];
    let end = loop {
        if let Some(end) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        let read = stream.read(&mut buffer).await.unwrap();
        assert_ne!(read, 0, "the connection closed before a response");
        raw.extend_from_slice(&buffer[..read]);
    };
    let head = String::from_utf8_lossy(&raw[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .map(|line| {
            let (key, value) = line.split_once(':').unwrap();
            (key.to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect();
    (status, headers, raw[end + 4..].to_vec())
}

/// One response: its body by Content-Length, by chunks, or to EOF.
async fn read_response(stream: &mut TcpStream) -> Response {
    let (status, headers, mut raw) = read_head(stream).await;
    let find = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let mut buffer = vec![0; 8192];
    let mut bytes = Vec::new();
    if find("transfer-encoding").is_some_and(|value| value == "chunked") {
        loop {
            let mut rest = raw.as_slice();
            bytes.clear();
            let complete = loop {
                let Some(line) = rest.windows(2).position(|window| window == b"\r\n") else {
                    break false;
                };
                let size = std::str::from_utf8(&rest[..line]).unwrap().trim();
                let size = usize::from_str_radix(size, 16).unwrap();
                if size == 0 {
                    break rest.len() >= line + 4;
                }
                if rest.len() < line + 4 + size {
                    break false;
                }
                bytes.extend_from_slice(&rest[line + 2..line + 2 + size]);
                rest = &rest[line + 4 + size..];
            };
            if complete {
                break;
            }
            let read = stream.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0, "the connection closed inside a chunked body");
            raw.extend_from_slice(&buffer[..read]);
        }
    } else if let Some(length) = find("content-length") {
        let length: usize = length.parse().unwrap();
        while raw.len() < length {
            let read = stream.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0, "the connection closed inside a body");
            raw.extend_from_slice(&buffer[..read]);
        }
        bytes = raw;
    } else {
        let _ = stream.read_to_end(&mut raw).await;
        bytes = raw;
    }
    let body = String::from_utf8_lossy(&bytes).into_owned();
    Response {
        status,
        headers,
        body,
    }
}

fn ledger(data: &Path) -> usize {
    std::fs::read_dir(data.join("core-processes")).map_or(0, Iterator::count)
}

fn runtime(state: &Value, slug: &str) -> String {
    state["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|profile| profile["slug"] == slug)
        .unwrap()["runtime"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn chat(stream: bool) -> String {
    json!({
        "model": "stub-model", "stream": stream,
        "messages": [{ "role": "user", "content": "hi" }],
    })
    .to_string()
}

#[tokio::test]
async fn a_dormant_profile_streams_through_the_gateway_and_each_signal_stops_in_order() {
    let upstream = stub_upstream().await.port;
    for signal in ["INT", "TERM", "HUP"] {
        let root = Root::new(&format!("signal-{signal}"));
        let seeded = seed(&root.data(), 1, upstream).await;
        let mut daemon = Daemon::serve(&root, free_port(), &[]).await;
        let profile = &seeded[0];
        // Dormant: no core until the first request.
        assert_eq!(
            runtime(&daemon.api("GET", "/api/state").await.json(), &profile.slug),
            "stopped"
        );
        assert_eq!(ledger(&daemon.data), 0);

        let bearer = format!("Bearer {}", profile.key);
        let target = format!("/p/{}/v1/chat/completions", profile.slug);
        let streamed = send(
            daemon.port,
            "POST",
            &target,
            &[("Authorization", &bearer)],
            &chat(true),
        )
        .await;
        assert_eq!(streamed.status, 200, "{}", streamed.body);
        assert_eq!(streamed.header("content-type"), Some("text/event-stream"));
        assert_eq!(
            streamed.header("x-nonstopvibin-profile"),
            Some(profile.slug.as_str())
        );
        let words: String = streamed
            .body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .filter_map(|event| {
                event["choices"][0]["delta"]["content"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect();
        assert_eq!(words, "Hello from the stub");
        assert!(streamed.body.contains("data: [DONE]"));
        assert_eq!(
            runtime(&daemon.api("GET", "/api/state").await.json(), &profile.slug),
            "running"
        );
        assert_eq!(ledger(&daemon.data), 1);

        daemon.signal(signal);
        let data = daemon.data.clone();
        let (code, stderr) = daemon.exited().await;
        assert_eq!(code, 0, "{stderr}");
        assert!(
            !data.join("server.json").exists(),
            "server.json outlived {signal}"
        );
        assert_eq!(ledger(&data), 0, "a core outlived {signal}");
        // The lock is free again.
        ServerLock::acquire(&data).unwrap().release();
    }
}

#[tokio::test]
async fn one_instance_runs_per_data_directory() {
    let root = Root::new("instance");
    // A stale server.json of a process that is gone does not block a start.
    std::fs::create_dir_all(root.data()).unwrap();
    nsv_store::files::owner_only_directory(&root.data(), || Ok::<(), std::io::Error>(())).unwrap();
    nsv_store::files::write_atomic(
        &root.data().join("server.json"),
        json!({ "pid": 4242, "port": free_port(), "token": "stale", "allowHosts": [], "version": "0.2.0", "startedAt": "2026-11-22T11:00:00.000Z" })
            .to_string()
            .as_bytes(),
        0o600,
    )
    .unwrap();
    let mut daemon = Daemon::serve(&root, free_port(), &[]).await;

    // A second serve, on another port, finds the first through server.json.
    let second = command(&root)
        .args(["serve", "--port", &free_port().to_string()])
        .output()
        .unwrap();
    assert_eq!(second.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&second.stderr),
        format!(
            "NonstopVibin is already running on 127.0.0.1:{} for {}. Run `nonstopvibin url` for its link.\n",
            daemon.port,
            root.data().display()
        )
    );
    // Killed, it leaves its lock free and its lock file in place, without a new inode.
    let lock = root.data().join("server.lock.sqlite");
    let inode = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&lock).unwrap());
    daemon.signal("KILL");
    let _ = daemon.exited().await;
    let held = ServerLock::acquire(&root.data()).unwrap();
    // Held by another process (this test), a start is refused before it opens anything.
    let refused = command(&root)
        .args(["serve", "--port", &free_port().to_string()])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(1));
    let canonical = std::fs::canonicalize(root.data()).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&refused.stderr),
        format!(
            "NonstopVibin is already starting, running or stopping for {}. Run `nonstopvibin url` for its link.\n",
            canonical.display()
        )
    );
    held.release();
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&lock).unwrap()),
        inode
    );
    let mut again = Daemon::serve(&root, free_port(), &[]).await;
    again.signal("TERM");
    assert_eq!(again.exited().await.0, 0);
}

#[tokio::test]
async fn an_allowed_host_reaches_the_interface_and_management_but_never_the_gateway() {
    let upstream = stub_upstream().await.port;
    let root = Root::new("allow-host");
    let seeded = seed(&root.data(), 1, upstream).await;
    let mut daemon =
        Daemon::serve(&root, free_port(), &["--allow-host", "Box.Tailnet.ts.net"]).await;
    let host = "box.tailnet.ts.net";
    let authorization = format!("Bearer {}", daemon.token);
    let api = |origin: &'static str| {
        let authorization = authorization.clone();
        async move {
            send(
                daemon.port,
                "GET",
                "/api/state",
                &[
                    ("Host", host),
                    ("Authorization", &authorization),
                    ("Origin", origin),
                ],
                "",
            )
            .await
            .status
        }
    };
    assert_eq!(api("https://box.tailnet.ts.net").await, 200);
    assert_eq!(api("http://box.tailnet.ts.net").await, 403);
    assert_eq!(api("http://127.0.0.1").await, 403);
    // Lookalike, suffixed and ported names are not the allowed host.
    for other in [
        "box.tailnet.ts.net.evil.com",
        "box.tailnet.ts.net:443",
        "BOX.tailnet.ts.net",
    ] {
        let refused = send(daemon.port, "GET", "/", &[("Host", other)], "").await;
        assert_eq!(refused.status, 403, "{other}");
    }
    let page = send(daemon.port, "GET", "/", &[("Host", host)], "").await;
    assert_ne!(page.status, 403);
    let missing = send(daemon.port, "GET", "/api/state", &[("Host", host)], "").await;
    assert_eq!(missing.status, 401);
    for (origin, status) in [
        ("https://box.tailnet.ts.net", 201),
        ("https://box.tailnet.ts.net:8443", 403),
        ("https://evil.example", 403),
    ] {
        let created = send(
            daemon.port,
            "POST",
            "/api/profiles",
            &[
                ("Host", host),
                ("Authorization", &authorization),
                ("Origin", origin),
            ],
            r#"{"name":"Remote"}"#,
        )
        .await;
        assert_eq!(created.status, status, "{origin}: {}", created.body);
    }
    // server.json is owner-only and names the allowed hosts.
    let session = daemon.data.join("server.json");
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&session).unwrap().permissions(),
    );
    assert_eq!(mode & 0o777, 0o600);
    let session: Value = serde_json::from_str(&std::fs::read_to_string(session).unwrap()).unwrap();
    assert_eq!(session["allowHosts"], json!([host]));

    let bearer = format!("Bearer {}", seeded[0].key);
    let models = send(
        daemon.port,
        "GET",
        "/v1/models",
        &[("Host", host), ("Authorization", &bearer)],
        "",
    )
    .await;
    assert_eq!(models.status, 403);
    assert_eq!(
        models.json()["error"]["message"],
        "The agent API is only available on the loopback address."
    );
    let upgrade = send(
        daemon.port,
        "GET",
        "/v1/responses",
        &[
            ("Host", host),
            ("Authorization", &bearer),
            ("Connection", "Upgrade"),
            ("Upgrade", "websocket"),
        ],
        "",
    )
    .await;
    assert_eq!(upgrade.status, 403);
    daemon.signal("TERM");
    assert_eq!(daemon.exited().await.0, 0);
}

#[tokio::test]
async fn profile_keys_never_manage_or_cross_profiles() {
    let upstream = stub_upstream().await.port;
    let root = Root::new("keys");
    let seeded = seed(&root.data(), 2, upstream).await;
    let mut daemon = Daemon::serve(&root, free_port(), &[]).await;
    let (first, second) = (&seeded[0], &seeded[1]);
    let bearer = format!("Bearer {}", first.key);
    let manage = send(
        daemon.port,
        "GET",
        "/api/state",
        &[("Authorization", &bearer)],
        "",
    )
    .await;
    assert_eq!(manage.status, 401);
    let crossed = send(
        daemon.port,
        "POST",
        &format!("/p/{}/v1/chat/completions", second.slug),
        &[("Authorization", &bearer)],
        &chat(false),
    )
    .await;
    assert_eq!(crossed.status, 403);
    assert_eq!(
        crossed.json()["error"]["message"],
        "This API key does not belong to the requested profile."
    );
    // The shared /v1 routes by key alone; the key's own profile starts for it.
    let own = send(
        daemon.port,
        "POST",
        "/v1/chat/completions",
        &[("Authorization", &bearer)],
        &chat(false),
    )
    .await;
    assert_eq!(own.status, 200, "{}", own.body);
    assert_eq!(
        own.header("x-nonstopvibin-profile"),
        Some(first.slug.as_str())
    );
    // A key created after the gateway served a request is known at once.
    let authorization = format!("Bearer {}", daemon.token);
    let created = send(
        daemon.port,
        "POST",
        "/api/profiles",
        &[
            ("Authorization", &authorization),
            ("Content-Type", "application/json"),
        ],
        r#"{"name":"Later"}"#,
    )
    .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let later = created.json()["id"].as_str().unwrap().to_owned();
    let later_key = daemon
        .api("GET", &format!("/api/profiles/{later}/key"))
        .await
        .json()["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let later_bearer = format!("Bearer {later_key}");
    let mismatched = send(
        daemon.port,
        "POST",
        &format!("/p/{}/v1/chat/completions", first.slug),
        &[("Authorization", &later_bearer)],
        &chat(false),
    )
    .await;
    assert_eq!(mismatched.status, 403, "{}", mismatched.body);
    let state = daemon.api("GET", "/api/state").await.json();
    assert_eq!(runtime(&state, &first.slug), "running");
    assert_eq!(runtime(&state, &second.slug), "stopped");
    // A UI poll with watch starts the dormant ones, so it shows them running.
    let watched = daemon.api("GET", "/api/state?watch=1").await.json();
    assert_eq!(runtime(&watched, &second.slug), "running");
    daemon.signal("TERM");
    assert_eq!(daemon.exited().await.0, 0);
}

#[tokio::test]
async fn only_a_websocket_switch_takes_the_upgrade_path() {
    let upstream = stub_upstream().await.port;
    let root = Root::new("switch");
    let seeded = seed(&root.data(), 1, upstream).await;
    let mut daemon = Daemon::serve(&root, free_port(), &[]).await;
    let bearer = format!("Bearer {}", seeded[0].key);
    // An Upgrade without `Connection: upgrade` is an ordinary request; a switch to anything
    // but WebSocket is refused rather than sent on as a bodiless GET.
    let ignored = send(
        daemon.port,
        "POST",
        "/v1/chat/completions",
        &[("Authorization", &bearer), ("Upgrade", "websocket")],
        &chat(false),
    )
    .await;
    assert_eq!(ignored.status, 200, "{}", ignored.body);
    let h2c = send(
        daemon.port,
        "POST",
        "/v1/chat/completions",
        &[
            ("Authorization", &bearer),
            ("Connection", "Upgrade, HTTP2-Settings"),
            ("Upgrade", "h2c"),
            ("HTTP2-Settings", "AAMAAABkAAQCAAAAAAIAAAAA"),
        ],
        &chat(false),
    )
    .await;
    assert_eq!(h2c.status, 400);
    daemon.signal("TERM");
    assert_eq!(daemon.exited().await.0, 0);
}

/// A Responses WebSocket handshake through the gateway: the answer's status and headers.
async fn handshake(port: u16, slug: &str, key: &str) -> (u16, Vec<(String, String)>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let bearer = format!("Bearer {key}");
    let mut wire = head(
        port,
        "GET",
        &format!("/p/{slug}/v1/responses"),
        &[
            ("Authorization", &bearer),
            ("Connection", "Upgrade"),
            ("Upgrade", "websocket"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("Sec-WebSocket-Version", "13"),
        ],
        0,
    );
    wire.push_str("\r\n");
    stream.write_all(wire.as_bytes()).await.unwrap();
    let (status, headers, _) = read_head(&mut stream).await;
    (status, headers)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn profile_id(state: &Value, slug: &str) -> String {
    state["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|profile| profile["slug"] == slug)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Polls `check` every 20 ms until it holds, for at most `seconds`.
async fn until(what: &str, seconds: u64, mut check: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !check().await {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_dormant_profile_s_first_websocket_starts_its_core_and_concurrent_requests_share_it() {
    let upstream = stub_upstream().await;
    let root = Root::new("websocket");
    let seeded = seed(&root.data(), 2, upstream.port).await;
    let mut daemon = Daemon::serve(&root, free_port(), &[]).await;
    let (first, second) = (&seeded[0], &seeded[1]);
    // Another profile's key is refused before anything wakes.
    let (crossed, _) = handshake(daemon.port, &first.slug, &second.key).await;
    assert_eq!(crossed, 403);
    assert_eq!(ledger(&daemon.data), 0);

    let (status, headers) = handshake(daemon.port, &first.slug, &first.key).await;
    assert_eq!(status, 101);
    assert_eq!(
        header(&headers, "x-nonstopvibin-profile"),
        Some(first.slug.as_str())
    );
    let state = daemon.api("GET", "/api/state").await.json();
    assert_eq!(runtime(&state, &first.slug), "running");
    assert_eq!(ledger(&daemon.data), 1);

    // Three chats and an upgrade that arrive together at a dormant profile wait for its one core.
    let bearer = format!("Bearer {}", second.key);
    let target = format!("/p/{}/v1/chat/completions", second.slug);
    let body = chat(false);
    let headers = [("Authorization", bearer.as_str())];
    let request = || send(daemon.port, "POST", &target, &headers, &body);
    let (one, two, three, (upgrade, _)) = tokio::join!(
        request(),
        request(),
        request(),
        handshake(daemon.port, &second.slug, &second.key)
    );
    for response in [one, two, three] {
        assert_eq!(response.status, 200, "{}", response.body);
    }
    assert_eq!(upgrade, 101);
    assert_eq!(ledger(&daemon.data), 2);
    daemon.signal("TERM");
    assert_eq!(daemon.exited().await.0, 0);
}

#[tokio::test]
async fn a_profile_with_32_requests_in_flight_refuses_the_next_until_one_finishes() {
    let upstream = stub_upstream().await;
    upstream.hold();
    let root = Root::new("cap");
    let seeded = seed(&root.data(), 1, upstream.port).await;
    let mut daemon = Daemon::serve(&root, free_port(), &[]).await;
    let profile = &seeded[0];
    let bearer = format!("Bearer {}", profile.key);
    let target = format!("/p/{}/v1/chat/completions", profile.slug);
    let body = chat(true);
    let mut open = Vec::new();
    for _ in 0..32 {
        let mut stream = TcpStream::connect(("127.0.0.1", daemon.port))
            .await
            .unwrap();
        let mut wire = head(
            daemon.port,
            "POST",
            &target,
            &[("Authorization", &bearer), ("Connection", "close")],
            body.len(),
        );
        wire.push_str("\r\n");
        wire.push_str(&body);
        stream.write_all(wire.as_bytes()).await.unwrap();
        let (status, _, _) = read_head(&mut stream).await;
        assert_eq!(status, 200);
        open.push(stream);
    }
    let refused = send(
        daemon.port,
        "POST",
        &target,
        &[("Authorization", &bearer)],
        &body,
    )
    .await;
    assert_eq!(refused.status, 429);
    assert_eq!(refused.header("retry-after"), Some("2"));
    assert_eq!(
        refused.json()["error"]["message"],
        "This profile has 32 requests in flight. Retry shortly."
    );
    assert_eq!(upstream.requests.load(Ordering::SeqCst), 32);

    upstream.release();
    for mut stream in open {
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.unwrap();
        assert!(String::from_utf8_lossy(&rest).contains("[DONE]"));
    }
    // A finished stream frees its slot once the gateway has written its end.
    until("a slot to free", 5, async || {
        let next = send(
            daemon.port,
            "POST",
            &target,
            &[("Authorization", &bearer)],
            &chat(false),
        )
        .await;
        assert!(matches!(next.status, 200 | 429), "{}", next.body);
        next.status == 200
    })
    .await;
    daemon.signal("TERM");
    assert_eq!(daemon.exited().await.0, 0);
}

#[tokio::test]
async fn agent_checks_and_accounting_go_through_the_gateway_without_extra_inference() {
    let upstream = stub_upstream().await;
    let root = Root::new("check");
    let seeded = seed(&root.data(), 1, upstream.port).await;
    let mut daemon = Daemon::serve(&root, free_port(), &[]).await;
    let profile = &seeded[0];
    let id = profile_id(&daemon.api("GET", "/api/state").await.json(), &profile.slug);
    let authorization = format!("Bearer {}", daemon.token);
    let manage = async |method: &str, path: &str, body: &str| {
        send(
            daemon.port,
            method,
            path,
            &[("Authorization", &authorization)],
            body,
        )
        .await
    };
    let check = manage("POST", &format!("/api/profiles/{id}/agent-check"), "{}").await;
    assert_eq!(check.status, 200, "{}", check.body);
    assert_eq!(check.json(), json!({ "ok": true }));
    assert_eq!(upstream.requests.load(Ordering::SeqCst), 0);

    // A profile without models answers that its catalog is empty.
    let created = manage("POST", "/api/profiles", r#"{"name":"Empty"}"#).await;
    assert_eq!(created.status, 201, "{}", created.body);
    let empty = created.json()["id"].as_str().unwrap().to_owned();
    let started = manage("POST", &format!("/api/profiles/{empty}/start"), "").await;
    assert!(started.status < 300, "{} {}", started.status, started.body);
    let check = manage("POST", &format!("/api/profiles/{empty}/agent-check"), "{}").await;
    assert_eq!(check.status, 400, "{}", check.body);
    assert_eq!(
        check.json()["error"]["message"],
        "No models are available. Refresh the profile catalog."
    );

    // A plain and a streamed chat, each 14 tokens of which 5 are cached and 3 output, are
    // stored once each, without the account key or the prompt.
    let bearer = format!("Bearer {}", profile.key);
    let target = format!("/p/{}/v1/chat/completions", profile.slug);
    for stream in [false, true] {
        let body = json!({
            "model": "stub-model", "stream": stream, "stream_options": { "include_usage": true },
            "messages": [{ "role": "user", "content": "accounting prompt fixture" }],
        })
        .to_string();
        let response = send(
            daemon.port,
            "POST",
            &target,
            &[("Authorization", &bearer)],
            &body,
        )
        .await;
        assert_eq!(response.status, 200, "{}", response.body);
    }
    let usage = format!("/api/profiles/{id}/usage?days=1");
    let mut summary = Value::Null;
    until("both requests to be stored", 30, async || {
        summary = manage("GET", &usage, "").await.json()["summary"].clone();
        summary["requests"] == 2
    })
    .await;
    assert_eq!(summary["totalTokens"], 28, "{summary}");
    assert_eq!(summary["outputTokens"], 6, "{summary}");
    assert_eq!(summary["cachedTokens"], 10, "{summary}");
    let mut stored = Vec::new();
    for entry in std::fs::read_dir(&daemon.data).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("nonstopvibin.sqlite")
        {
            stored.extend(std::fs::read(path).unwrap());
        }
    }
    assert_ne!(stored.len(), 0);
    for secret in ["upstream-key", "accounting prompt fixture"] {
        assert!(
            !stored
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "{secret}"
        );
    }
    daemon.signal("TERM");
    assert_eq!(daemon.exited().await.0, 0);
}

#[tokio::test]
async fn clients_that_abort_are_not_reported_as_application_errors() {
    let upstream = stub_upstream().await;
    let root = Root::new("abort");
    let seeded = seed(&root.data(), 1, upstream.port).await;
    let mut daemon = Daemon::serve(&root, free_port(), &[]).await;
    // A management client that closes mid-body.
    let mut partial = TcpStream::connect(("127.0.0.1", daemon.port))
        .await
        .unwrap();
    let authorization = format!("Bearer {}", daemon.token);
    let mut wire = head(
        daemon.port,
        "POST",
        "/api/profiles",
        &[("Authorization", &authorization)],
        1000,
    );
    wire.push_str("\r\n{\"name\":");
    partial.write_all(wire.as_bytes()).await.unwrap();
    drop(partial);
    // An agent that hangs up before the core answers.
    let profile = &seeded[0];
    let mut agent = TcpStream::connect(("127.0.0.1", daemon.port))
        .await
        .unwrap();
    let bearer = format!("Bearer {}", profile.key);
    let body = r#"{"model":"stub-model","messages":[{"role":"user","content":"hang"}]}"#;
    let mut wire = head(
        daemon.port,
        "POST",
        &format!("/p/{}/v1/chat/completions", profile.slug),
        &[("Authorization", &bearer)],
        body.len(),
    );
    wire.push_str("\r\n");
    wire.push_str(body);
    agent.write_all(wire.as_bytes()).await.unwrap();
    until("the chat to reach the provider", 20, async || {
        upstream.hanging.load(Ordering::SeqCst) == 1
    })
    .await;
    drop(agent);
    until("the core to give the chat up", 10, async || {
        upstream.hanging.load(Ordering::SeqCst) == 0
    })
    .await;
    let state = daemon.api("GET", "/api/state").await.json();
    assert_eq!(state["errors"], json!([]));
    daemon.signal("TERM");
    assert_eq!(daemon.exited().await.0, 0);
}

#[tokio::test]
async fn one_agent_connection_is_reauthenticated_for_each_request() {
    let upstream = stub_upstream().await;
    let root = Root::new("keep-alive");
    let seeded = seed(&root.data(), 2, upstream.port).await;
    let mut daemon = Daemon::serve(&root, free_port(), &[]).await;
    let mut stream = TcpStream::connect(("127.0.0.1", daemon.port))
        .await
        .unwrap();
    let body = chat(false);
    // A body in fragments that split its JSON, or with the head in one write: a refusal can
    // answer and close before the rest of a fragmented body arrives.
    let mut request = async |slug: &str, key: &str, fragmented: bool| {
        let bearer = format!("Bearer {key}");
        let mut wire = head(
            daemon.port,
            "POST",
            &format!("/p/{slug}/v1/chat/completions"),
            &[("Authorization", &bearer)],
            body.len(),
        );
        wire.push_str("\r\n");
        let fragments = if fragmented {
            stream.write_all(wire.as_bytes()).await.unwrap();
            let body = body.as_bytes();
            vec![&body[..9], &body[9..30], &body[30..]]
        } else {
            wire.push_str(&body);
            vec![wire.as_bytes()]
        };
        for fragment in fragments {
            stream.write_all(fragment).await.unwrap();
            stream.flush().await.unwrap();
        }
        read_response(&mut stream).await
    };
    for profile in [&seeded[0], &seeded[1], &seeded[0]] {
        let response = request(&profile.slug, &profile.key, true).await;
        assert_eq!(response.status, 200, "{}", response.body);
        assert_eq!(
            response.header("x-nonstopvibin-profile"),
            Some(profile.slug.as_str())
        );
    }
    let wrong = request(&seeded[0].slug, "not-a-profile-key", false).await;
    assert_eq!(wrong.status, 401);
    assert_eq!(upstream.requests.load(Ordering::SeqCst), 3);
    daemon.signal("TERM");
    assert_eq!(daemon.exited().await.0, 0);
}

#[tokio::test]
async fn simultaneous_starts_elect_one_owner() {
    let root = Root::new("race");
    let spawn = |port: u16| Daemon {
        child: command(&root)
            .args(["serve", "--port", &port.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        port,
        token: String::new(),
        data: root.data(),
    };
    let mut started = [spawn(free_port()), spawn(free_port())];
    let mut loser = None;
    until("one contender to exit", 20, async || {
        loser = started
            .iter_mut()
            .position(|daemon| daemon.child.try_wait().unwrap().is_some());
        loser.is_some()
    })
    .await;
    let loser = loser.unwrap();
    let (code, stderr) = started[loser].exited().await;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.starts_with("NonstopVibin is already"), "{stderr}");
    let owner = &mut started[1 - loser];
    let pid = owner.child.id();
    until("the owner's server.json", 20, async || {
        std::fs::read_to_string(root.data().join("server.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .is_some_and(|session| session["pid"] == pid)
    })
    .await;
    assert_eq!(owner.child.try_wait().unwrap(), None);
    owner.signal("TERM");
    assert_eq!(owner.exited().await.0, 0);
    assert!(!root.data().join("server.json").exists());
}

#[tokio::test]
async fn a_stopping_daemon_keeps_its_directory_until_its_cores_have_stopped() {
    let upstream = stub_upstream().await;
    let root = Root::new("stopping");
    let seeded = seed(&root.data(), 1, upstream.port).await;
    let mut daemon = Daemon::serve(&root, free_port(), &[]).await;
    let profile = &seeded[0];
    let bearer = format!("Bearer {}", profile.key);
    let response = send(
        daemon.port,
        "POST",
        &format!("/p/{}/v1/chat/completions", profile.slug),
        &[("Authorization", &bearer)],
        &chat(false),
    )
    .await;
    assert_eq!(response.status, 200);
    let entry = std::fs::read_dir(daemon.data.join("core-processes"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let core_pid = entry
        .file_name()
        .to_string_lossy()
        .trim_end_matches(".json")
        .to_owned();
    let signal = |name: &str| {
        assert!(
            Command::new("kill")
                .args([format!("-{name}"), core_pid.clone()])
                .status()
                .unwrap()
                .success()
        );
    };
    // The core cannot stop while it is suspended, so the daemon stays in its shutdown.
    signal("STOP");
    daemon.signal("TERM");
    // Shutdown began once the listener is gone.
    until("the daemon to stop listening", 10, async || {
        TcpStream::connect(("127.0.0.1", daemon.port))
            .await
            .is_err()
    })
    .await;
    let contender = command(&root)
        .args(["serve", "--port", &free_port().to_string()])
        .output()
        .unwrap();
    signal("CONT");
    assert_eq!(contender.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&contender.stderr);
    assert!(
        stderr.contains("already starting, running or stopping"),
        "{stderr}"
    );
    let data = daemon.data.clone();
    let (code, output) = daemon.exited().await;
    assert_eq!(code, 0, "{output}");
    assert_eq!(ledger(&data), 0, "a core outlived the shutdown");
    let mut successor = Daemon::serve(&root, free_port(), &[]).await;
    successor.signal("TERM");
    assert_eq!(successor.exited().await.0, 0);
}

/// `bun run dev`: the Vite server on 127.0.0.1:5173 proxies `/api` to a daemon started with
/// `NONSTOPVIBIN_DEVELOPMENT=1`, which then accepts that Host and Origin. Without the variable,
/// or in a packaged build, they are refused as before.
#[tokio::test]
async fn only_a_development_checkout_build_accepts_the_vite_server() {
    let root = Root::new("development");
    let vite = |port: u16, token: String, origin: &'static str| async move {
        let authorization = format!("Bearer {token}");
        send(
            port,
            "GET",
            "/api/state",
            &[
                ("Host", "127.0.0.1:5173"),
                ("Origin", origin),
                ("Authorization", &authorization),
            ],
            "",
        )
        .await
        .status
    };

    let mut development = command(&root);
    development.env("NONSTOPVIBIN_DEVELOPMENT", "1");
    let mut daemon = Daemon::serve_with(development, &root, free_port(), &[]).await;
    assert_eq!(
        vite(daemon.port, daemon.token.clone(), "http://127.0.0.1:5173").await,
        200
    );
    assert_eq!(
        vite(daemon.port, daemon.token.clone(), "http://127.0.0.1:5174").await,
        403
    );
    daemon.signal("TERM");
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut daemon.child.stdout.take().unwrap(), &mut stdout).unwrap();
    assert_eq!(daemon.exited().await.0, 0);
    assert_eq!(
        stdout.trim_end(),
        format!(
            "Open the local interface: http://127.0.0.1:5173/#session={}",
            daemon.token
        )
    );

    let mut daemon = Daemon::serve(&root, free_port(), &[]).await;
    assert_eq!(
        vite(daemon.port, daemon.token.clone(), "http://127.0.0.1:5173").await,
        403
    );
    daemon.signal("TERM");
    assert_eq!(daemon.exited().await.0, 0);

    // A packaged layout: the pinned core beside the daemon, or in the bundle's Resources.
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.vendor/core");
    let package = root.0.join("package");
    let (executable, core) = if cfg!(target_os = "macos") {
        (
            package.join("Contents/MacOS/nonstopvibin"),
            package.join("Contents/Resources/core"),
        )
    } else {
        (package.join("nonstopvibin"), package.join("core"))
    };
    std::fs::create_dir_all(&core).unwrap();
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    for file in ["cli-proxy-api", "manifest.json"] {
        std::fs::hard_link(checkout.join(file), core.join(file))
            .or_else(|_| std::fs::copy(checkout.join(file), core.join(file)).map(drop))
            .unwrap();
    }
    std::fs::copy(env!("CARGO_BIN_EXE_nonstopvibin"), &executable).unwrap();
    let mut packaged = Command::new(&executable);
    packaged
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.0.join("home"))
        .env("NONSTOPVIBIN_DATA_DIR", root.data())
        .env("NONSTOPVIBIN_DEVELOPMENT", "1")
        .stdin(Stdio::null());
    let mut daemon = Daemon::serve_with(packaged, &root, free_port(), &[]).await;
    assert_eq!(
        vite(daemon.port, daemon.token.clone(), "http://127.0.0.1:5173").await,
        403
    );
    daemon.signal("TERM");
    assert_eq!(daemon.exited().await.0, 0);
}
