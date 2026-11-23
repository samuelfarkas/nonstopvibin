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
use std::time::{Duration, Instant};

use nsv_domain::secret::ApiKey;
use nsv_domain::wire::ApiAccount;
use nsv_store::{ServerLock, Store, Vault};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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

/// An OpenAI-compatible upstream that streams three deltas for every chat completion.
async fn stub_upstream() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0; 8192];
                loop {
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
                            break;
                        }
                    }
                }
                let mut events = String::new();
                for (index, word) in ["Hello", " from", " the stub"].iter().enumerate() {
                    let chunk = json!({
                        "id": "chatcmpl-stub", "object": "chat.completion.chunk", "created": 1,
                        "model": "stub-model",
                        "choices": [{ "index": 0, "delta": { "content": word }, "finish_reason": null }],
                    });
                    write!(events, "data: {chunk}\n\n").unwrap();
                    if index == 2 {
                        let done = json!({
                            "id": "chatcmpl-stub", "object": "chat.completion.chunk", "created": 1,
                            "model": "stub-model",
                            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
                            "usage": { "prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14 },
                        });
                        write!(events, "data: {done}\n\ndata: [DONE]\n\n").unwrap();
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
                    events.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    port
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
        let mut daemon = Self {
            child: command(root)
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

/// One request on a new connection, read to EOF; a chunked body is joined.
async fn send(
    port: u16,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Response {
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
        write!(
            head,
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        )
        .unwrap();
    }
    head.push_str("Connection: close\r\n\r\n");
    head.push_str(body);
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await;
    let end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
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
    let headers: Vec<(String, String)> = lines
        .map(|line| {
            let (key, value) = line.split_once(':').unwrap();
            (key.to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect();
    let chunked = headers
        .iter()
        .any(|(key, value)| key == "transfer-encoding" && value == "chunked");
    let mut rest = &raw[end + 4..];
    let mut bytes = Vec::new();
    if chunked {
        while let Some(line) = rest.windows(2).position(|window| window == b"\r\n") {
            let size = std::str::from_utf8(&rest[..line]).unwrap().trim();
            let size = usize::from_str_radix(size, 16).unwrap();
            if size == 0 {
                break;
            }
            bytes.extend_from_slice(&rest[line + 2..line + 2 + size]);
            rest = &rest[line + 2 + size + 2..];
        }
    } else {
        bytes.extend_from_slice(rest);
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
    let upstream = stub_upstream().await;
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
    let upstream = stub_upstream().await;
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
    let upstream = stub_upstream().await;
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
    let state = daemon.api("GET", "/api/state").await.json();
    assert_eq!(runtime(&state, &first.slug), "running");
    assert_eq!(runtime(&state, &second.slug), "stopped");
    // A UI poll with watch starts the dormant ones, so it shows them running.
    let watched = daemon.api("GET", "/api/state?watch=1").await.json();
    assert_eq!(runtime(&watched, &second.slug), "running");
    daemon.signal("TERM");
    assert_eq!(daemon.exited().await.0, 0);
}
