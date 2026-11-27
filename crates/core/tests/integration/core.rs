//! Generated configs loaded into the real pinned core (`bun run core:install` puts it at
//! `.vendor/core`): it starts from a written config.yaml, routes a request for an API-key
//! account to a stub upstream, and reports every golden config's live entries as written.
//! No OAuth credential is ever given to it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nsv_core::binary::{CoreTarget, bundled_pin};
use nsv_core::config::{CoreConfiguration, applied, write_config};
use nsv_core::processes::{CoreProcess, spawn_core};
use nsv_domain::id::ProfileId;
use nsv_domain::registry::Strategy;
use nsv_domain::secret::ApiKey;
use nsv_domain::wire::{ApiAccount, Profile};
use nsv_files::DataDir;
use nsv_net::{Loopback, LoopbackRequest, Method, Response, loopback};
use nsv_store::{Store, Vault};
use nsv_testkit::{Port, TempDir};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::golden_config_cases;

/// The pinned core of this checkout, checked against the bundled pin before it runs.
pub(crate) fn pinned_core() -> PathBuf {
    let directory = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.vendor/core"));
    assert!(
        directory.join("cli-proxy-api").exists(),
        "the pinned core is missing: run bun run core:install"
    );
    nsv_core::check_core_directory(
        bundled_pin().unwrap(),
        directory,
        CoreTarget::current().unwrap(),
        nsv_core::binary::Binary::Pinned,
    )
    .unwrap();
    directory.join("cli-proxy-api")
}

pub(crate) async fn call(
    port: u16,
    method: &Method,
    target: &str,
    key: &str,
    body: Option<&[u8]>,
) -> Result<Response, nsv_net::NetError> {
    let authorization = format!("Bearer {key}");
    loopback(LoopbackRequest {
        to: Loopback::v4(port),
        method,
        target,
        headers: &[
            ("Authorization", &authorization),
            ("Content-Type", "application/json"),
        ],
        body,
        timeout: Duration::from_secs(15),
        limit: 16 << 20,
    })
    .await
}

/// Polls the management API until the core answers its config; answers that config.
async fn reported_config(port: u16, management_key: &str) -> Value {
    let started = Instant::now();
    loop {
        if let Ok(response) = call(
            port,
            &Method::GET,
            "/v0/management/config",
            management_key,
            None,
        )
        .await
            && response.ok()
        {
            return serde_json::from_slice(&response.body).unwrap();
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the core did not start"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// What the stub provider read of one request.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub(crate) path: String,
    /// Lowercase names, in the order sent.
    pub(crate) headers: Vec<(String, String)>,
    /// `null` for a request without a JSON body.
    pub(crate) body: Value,
}

impl Seen {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The label a test puts in the first message: its text, or its first text block.
    pub(crate) fn label(&self) -> &str {
        let content = &self.body["messages"][0]["content"];
        content.as_str().map_or_else(
            || {
                content
                    .as_array()
                    .and_then(|blocks| blocks.iter().find(|block| block["type"] == "text"))
                    .and_then(|block| block["text"].as_str())
                    .unwrap_or("")
            },
            |text| text,
        )
    }
}

/// How the stub provider answers one request; every answer closes its connection.
pub(crate) enum Reply {
    Json(u16, Value),
    /// An event stream written in these transport chunks, then ended cleanly.
    Events(Vec<String>),
    /// An event stream whose first chunk is written and which then stays open until the
    /// reader closes it.
    Hold(String),
}

/// A provider on a loopback port: what it saw, and how many held streams are still open.
pub(crate) struct Provider {
    pub(crate) port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
    held: Arc<AtomicUsize>,
}

impl Provider {
    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub(crate) fn last(&self) -> Seen {
        self.seen().pop().unwrap()
    }

    pub(crate) fn held(&self) -> usize {
        self.held.load(Ordering::SeqCst)
    }
}

/// Starts a provider that answers each request as `answer` says.
pub(crate) async fn provider(answer: fn(&Seen) -> Reply) -> Provider {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let held = Arc::new(AtomicUsize::new(0));
    let (log, open) = (Arc::clone(&seen), Arc::clone(&held));
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (log, open) = (Arc::clone(&log), Arc::clone(&open));
            tokio::spawn(async move {
                let Some(request) = read_request(&mut stream).await else {
                    return;
                };
                log.lock().unwrap().push(request.clone());
                let events = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nAccess-Control-Allow-Origin: https://fixture.invalid\r\nSet-Cookie: fixture=not-forwarded\r\nConnection: close\r\n\r\n";
                match answer(&request) {
                    Reply::Json(status, body) => {
                        let body = body.to_string();
                        let retry = if status == 429 {
                            "Retry-After: 60\r\n"
                        } else {
                            ""
                        };
                        let response = format!(
                            "HTTP/1.1 {status} Stub\r\nContent-Type: application/json\r\n{retry}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    }
                    Reply::Events(chunks) => {
                        let _ = stream.write_all(events.as_bytes()).await;
                        for chunk in chunks {
                            let _ = stream.write_all(chunk.as_bytes()).await;
                            let _ = stream.flush().await;
                        }
                    }
                    Reply::Hold(first) => {
                        open.fetch_add(1, Ordering::SeqCst);
                        let _ = stream
                            .write_all(format!("{events}{first}").as_bytes())
                            .await;
                        let mut buffer = [0; 64];
                        while matches!(stream.read(&mut buffer).await, Ok(read) if read > 0) {}
                        open.fetch_sub(1, Ordering::SeqCst);
                        return;
                    }
                }
                let _ = stream.shutdown().await;
            });
        }
    });
    Provider { port, seen, held }
}

/// One request: its head, then a body of the declared length or in chunks.
async fn read_request(stream: &mut TcpStream) -> Option<Seen> {
    let mut raw = Vec::new();
    let mut buffer = vec![0; 16384];
    let mut more = async |raw: &mut Vec<u8>| {
        let read = stream.read(&mut buffer).await.ok()?;
        raw.extend_from_slice(&buffer[..read]);
        (read > 0).then_some(())
    };
    let end = loop {
        if let Some(end) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        more(&mut raw).await?;
    };
    let head = String::from_utf8_lossy(&raw[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let path = lines.next()?.split(' ').nth(1)?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let find = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let body = if find("transfer-encoding").is_some_and(|value| value == "chunked") {
        while !raw.ends_with(b"0\r\n\r\n") {
            more(&mut raw).await?;
        }
        let mut rest = &raw[end + 4..];
        let mut body = Vec::new();
        loop {
            let line = rest.windows(2).position(|window| window == b"\r\n")?;
            let size =
                usize::from_str_radix(std::str::from_utf8(&rest[..line]).ok()?.trim(), 16).ok()?;
            if size == 0 {
                break body;
            }
            body.extend_from_slice(&rest[line + 2..line + 2 + size]);
            rest = &rest[line + 4 + size..];
        }
    } else {
        let length: usize = find("content-length").map_or(0, |value| value.parse().unwrap());
        while raw.len() < end + 4 + length {
            more(&mut raw).await?;
        }
        raw[end + 4..end + 4 + length].to_vec()
    };
    Some(Seen {
        path,
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

/// A chat completion from the provider that names the key it was sent, with 14
/// tokens of usage; a key starting `exhausted` gets a 429 instead.
pub(crate) fn chat_completions(seen: &Seen) -> Reply {
    let key = seen
        .header("authorization")
        .unwrap_or("")
        .trim_start_matches("Bearer ")
        .to_owned();
    if key.starts_with("exhausted") {
        return Reply::Json(
            429,
            json!({ "error": { "message": "quota exhausted", "type": "rate_limit_error" } }),
        );
    }
    Reply::Json(
        200,
        json!({
            "id": "fixture", "object": "chat.completion", "created": 1, "model": "fixture-model",
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": format!("hello from {key}") }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14 },
        }),
    )
}

/// An API-key account of `protocol` on `base`, as the TypeScript tests' `customAccount` made it.
pub(crate) fn account(id: &str, base: &str, models: &[(&str, &str)]) -> ApiAccount {
    let models: Vec<Value> = models
        .iter()
        .map(|(model, protocol)| json!({ "id": model, "protocol": protocol }))
        .collect();
    serde_json::from_value(json!({
        "id": id, "name": id, "provider": "custom", "baseUrl": base, "prefix": "",
        "models": models, "disabled": false,
    }))
    .unwrap()
}

/// A profile's core, started on a config generated for its accounts, answering on `port`.
pub(crate) struct Running {
    pub(crate) port: u16,
    pub(crate) key: String,
    pub(crate) management: String,
    pub(crate) directory: PathBuf,
    core: CoreProcess,
}

impl Running {
    /// Creates a profile in `store` with these accounts and keys, adjusts it with `routing`,
    /// and starts `binary` on its generated config.
    pub(crate) async fn start(
        binary: &Path,
        store: &Store,
        data: &Path,
        accounts: Vec<(ApiAccount, &str)>,
        routing: fn(&mut Profile),
    ) -> Self {
        let accounts: Vec<(ApiAccount, ApiKey)> = accounts
            .into_iter()
            .map(|(account, key)| (account, ApiKey::new(key)))
            .collect();
        let profile = store
            .call(move |db| {
                let mut profile = db.create_profile("Core fixture")?;
                routing(&mut profile);
                db.save_profile(&profile)?;
                for (account, key) in &accounts {
                    db.save_api_account(&profile.id, account, Some(key))?;
                }
                Ok(profile)
            })
            .await
            .unwrap();
        let reserved = Port::reserve();
        let port = reserved.number();
        let config = write_config(store, profile.id.clone(), port, None)
            .await
            .unwrap();
        // The core binds it.
        drop(reserved);
        let core = spawn_core(binary, &DataDir::new(data), &profile.id).unwrap();
        let management = config.management_key.expose_secret().to_owned();
        let reported = reported_config(port, &management).await;
        assert!(applied(&config, &reported), "{reported}");
        Self {
            port,
            key: config.core_key.expose_secret().to_owned(),
            management,
            directory: DataDir::new(data).profile(&profile.id),
            core,
        }
    }

    /// A request with the core key and `headers`.
    pub(crate) async fn request(
        &self,
        method: &Method,
        target: &str,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Response {
        let authorization = format!("Bearer {}", self.key);
        let mut all = vec![
            ("Authorization", authorization.as_str()),
            ("Content-Type", "application/json"),
        ];
        all.extend_from_slice(headers);
        let body = body.map(Value::to_string);
        loopback(LoopbackRequest {
            to: Loopback::v4(self.port),
            method,
            target,
            headers: &all,
            body: body.as_deref().map(str::as_bytes),
            timeout: Duration::from_secs(15),
            limit: 16 << 20,
        })
        .await
        .unwrap()
    }

    pub(crate) async fn post(
        &self,
        target: &str,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Response {
        self.request(&Method::POST, target, headers, Some(body))
            .await
    }

    pub(crate) async fn stop(mut self) {
        self.core.terminate().await.unwrap();
    }
}

/// The JSON of every event in an event stream's text.
pub(crate) fn events(wire: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(wire)
        .lines()
        .filter_map(|line| line.strip_prefix("data: {"))
        .map(|data| serde_json::from_str(&format!("{{{data}")).unwrap())
        .collect()
}

/// One event of a stream whose data carries its own type, as both providers' streams do.
pub(crate) fn frame(kind: &str, fields: Value) -> String {
    let Value::Object(fields) = fields else {
        panic!("{fields}")
    };
    let mut data = json!({ "type": kind });
    data.as_object_mut().unwrap().extend(fields);
    format!("event: {kind}\ndata: {data}\n\n")
}

pub(crate) async fn open(directory: &Path) -> Store {
    nsv_files::owner_only_directory(directory, || Ok::<(), std::io::Error>(())).unwrap();
    let data = DataDir::new(directory);
    Store::open(&data, Vault::open(&data).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn the_pinned_core_loads_a_generated_config_and_answers_through_an_api_key_account() {
    let binary = pinned_core();
    let root = TempDir::new("real-core");
    let data = root.path().join("data");
    let store = open(&data).await;
    let upstream = provider(chat_completions).await;
    let base = format!("http://127.0.0.1:{}/v1", upstream.port);
    let core = Running::start(
        &binary,
        &store,
        &data,
        vec![(
            account("stub-account", &base, &[("stub-model", "openai")]),
            "upstream-key",
        )],
        |_| {},
    )
    .await;
    let response = core
        .post(
            "/v1/chat/completions",
            &[],
            &json!({ "model": "stub-model", "messages": [{ "role": "user", "content": "hi" }] }),
        )
        .await;
    let text = String::from_utf8_lossy(&response.body).into_owned();
    assert!(response.ok(), "{} {text}", response.status);
    assert!(text.contains("hello from upstream-key"), "{text}");
    let seen = upstream.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].path, "/v1/chat/completions");
    assert_eq!(seen[0].header("authorization"), Some("Bearer upstream-key"));
    // The client key of another profile, or none, is refused.
    let refused = call(
        core.port,
        &Method::GET,
        "/v1/models",
        "not-the-core-key",
        None,
    )
    .await
    .unwrap();
    assert_eq!(refused.status.as_u16(), 401);
    // Traffic leaves no request-body capture or error log in the profile's directory.
    let mut files = Vec::new();
    let mut pending = vec![core.directory.clone()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            files.push(path.to_string_lossy().into_owned());
        }
    }
    let captures: Vec<&String> = files
        .iter()
        .filter(|file| {
            Path::new(file)
                .extension()
                .is_some_and(|extension| extension == "log")
                || ["request-body", "api-request", "api-response"]
                    .iter()
                    .any(|name| file.contains(name))
        })
        .collect();
    assert!(captures.is_empty(), "{captures:?}");
    core.stop().await;
    store.close().await;
}

#[tokio::test]
async fn the_pinned_core_serves_codex_s_catalog_and_translates_messages_and_responses() {
    let binary = pinned_core();
    let root = TempDir::new("translate");
    let data = root.path().join("data");
    let store = open(&data).await;
    let upstream = provider(chat_completions).await;
    let base = format!("http://127.0.0.1:{}/v1", upstream.port);
    let core = Running::start(
        &binary,
        &store,
        &data,
        vec![(
            account("personal-one", &base, &[("fixture-model", "openai")]),
            "personal-one",
        )],
        |_| {},
    )
    .await;
    let catalog = call(
        core.port,
        &Method::GET,
        "/v1/models?client_version=0.140.0",
        &core.key,
        None,
    )
    .await
    .unwrap();
    assert_eq!(catalog.status.as_u16(), 200);
    let catalog: Value = serde_json::from_slice(&catalog.body).unwrap();
    let models = catalog["models"].as_array().unwrap();
    let slugs: Vec<&Value> = models.iter().map(|model| &model["slug"]).collect();
    assert_eq!(slugs, [&json!("fixture-model")], "{catalog}");
    assert_eq!(models[0]["visibility"], "list");
    assert_ne!(models[0]["base_instructions"].as_str().unwrap(), "");
    for (path, payload) in [
        (
            "/v1/messages",
            json!({ "model": "fixture-model", "max_tokens": 20, "messages": [{ "role": "user", "content": "hello" }] }),
        ),
        (
            "/v1/responses",
            json!({ "model": "fixture-model", "input": "hello", "stream": false }),
        ),
    ] {
        let response = core
            .post(path, &[("anthropic-version", "2023-06-01")], &payload)
            .await;
        let text = String::from_utf8_lossy(&response.body).into_owned();
        assert_eq!(response.status.as_u16(), 200, "{path}: {text}");
        assert!(text.contains("hello from personal-one"), "{path}: {text}");
    }
    core.stop().await;
    store.close().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "three routings over one provider, in order"
)]
async fn routing_spreads_or_fills_across_a_profile_s_keys_and_a_429_stays_in_its_core() {
    let binary = pinned_core();
    let root = TempDir::new("routing");
    let data = root.path().join("data");
    let store = open(&data).await;
    let upstream = provider(chat_completions).await;
    let base = format!("http://127.0.0.1:{}/v1", upstream.port);
    let keys = |first: &'static str, second: &'static str| {
        vec![
            (account(first, &base, &[("fixture-model", "openai")]), first),
            (
                account(second, &base, &[("fixture-model", "openai")]),
                second,
            ),
        ]
    };
    let chat =
        json!({ "model": "fixture-model", "messages": [{ "role": "user", "content": "hello" }] });
    // The keys the provider saw from here on.
    let sent = |before: usize| -> Vec<String> {
        upstream.seen()[before..]
            .iter()
            .map(|seen| seen.header("authorization").unwrap().replace("Bearer ", ""))
            .collect()
    };

    let spread = Running::start(
        &binary,
        &store,
        &data,
        keys("company-one", "company-two"),
        |profile| {
            profile.strategy = Strategy::RoundRobin;
            profile.session_affinity = false;
        },
    )
    .await;
    let before = upstream.seen().len();
    for _ in 0..6 {
        let response = spread.post("/v1/chat/completions", &[], &chat).await;
        assert_eq!(response.status.as_u16(), 200);
    }
    let selected = sent(before);
    assert_eq!(
        selected.iter().filter(|key| *key == "company-one").count(),
        3,
        "{selected:?}"
    );
    assert_eq!(
        selected.iter().filter(|key| *key == "company-two").count(),
        3,
        "{selected:?}"
    );
    spread.stop().await;

    let filled = Running::start(
        &binary,
        &store,
        &data,
        keys("company-one", "company-two"),
        |profile| {
            profile.strategy = Strategy::FillFirst;
            profile.session_affinity = true;
        },
    )
    .await;
    let before = upstream.seen().len();
    for _ in 0..4 {
        let response = filled.post("/v1/chat/completions", &[], &chat).await;
        assert_eq!(response.status.as_u16(), 200);
    }
    let mut selected = sent(before);
    selected.dedup();
    assert_eq!(selected.len(), 1, "{selected:?}");
    filled.stop().await;

    // Every key of one profile exhausted: its core answers 429 without another profile's key,
    // and another profile's core still answers.
    let exhausted = Running::start(
        &binary,
        &store,
        &data,
        keys("exhausted-one", "exhausted-two"),
        |_| {},
    )
    .await;
    let personal = Running::start(
        &binary,
        &store,
        &data,
        vec![(
            account("personal-one", &base, &[("fixture-model", "openai")]),
            "personal-one",
        )],
        |_| {},
    )
    .await;
    let before = upstream.seen().len();
    let refused = exhausted.post("/v1/chat/completions", &[], &chat).await;
    assert_eq!(refused.status.as_u16(), 429);
    let selected = sent(before);
    assert!(
        !selected.is_empty() && selected.iter().all(|key| key.starts_with("exhausted")),
        "{selected:?}"
    );
    let other = personal.post("/v1/chat/completions", &[], &chat).await;
    assert_eq!(other.status.as_u16(), 200);
    assert!(String::from_utf8_lossy(&other.body).contains("personal-one"));
    exhausted.stop().await;
    personal.stop().await;
    store.close().await;
}

/// Writes `config` for a profile of `data`, starts the core on it and answers what the core
/// reports, then stops it.
async fn load(
    binary: &Path,
    data: &Path,
    profile: &ProfileId,
    config: &CoreConfiguration,
) -> Value {
    std::fs::create_dir_all(DataDir::new(data).credentials(profile)).unwrap();
    std::fs::write(DataDir::new(data).core_config(profile), config.to_yaml()).unwrap();
    let mut core: CoreProcess = spawn_core(binary, &DataDir::new(data), profile).unwrap();
    let reported = reported_config(config.port, config.management_key.expose_secret()).await;
    core.terminate().await.unwrap();
    reported
}

#[tokio::test]
async fn the_pinned_core_reads_every_golden_config_as_written() {
    let binary = pinned_core();
    let root = TempDir::new("golden-core");
    let data = root.path().join("data");
    let profile = ProfileId::parse("00000000-0000-4000-8000-00000000c0fe").unwrap();
    let mut loaded = 0;
    for (name, mut config, _) in golden_config_cases() {
        let reserved = Port::reserve();
        config.port = reserved.number();
        config.auth_dir = DataDir::new(&data)
            .credentials(&profile)
            .to_str()
            .unwrap()
            .to_owned();
        // The core binds it.
        drop(reserved);
        let reported = load(&binary, &data, &profile, &config).await;
        assert!(applied(&config, &reported), "{name}: {reported}");
        // The fixed entries the core reports back. It answered on the written port with the
        // written management key; it does not report host, port, auth-dir or that key, and it
        // reports redis-usage-queue-retention-seconds as 3600 whatever the file says.
        let written = config.to_json();
        for key in [
            "api-keys",
            "debug",
            "commercial-mode",
            "logging-to-file",
            "request-log",
            "error-logs-max-files",
            "usage-statistics-enabled",
            "request-retry",
            "max-retry-interval",
            "ws-auth",
            "quota-exceeded",
        ] {
            assert_eq!(reported[key], written[key], "{name}: {key}");
        }
        assert_eq!(reported["plugins"]["enabled"], false, "{name}");
        loaded += 1;
    }
    assert_eq!(loaded, 15);
}

#[tokio::test]
async fn the_pinned_core_applies_a_claude_agent_entry_with_its_never_cloak() {
    let binary = pinned_core();
    let root = TempDir::new("claude-agent-core");
    let data = root.path().join("data");
    let store = open(&data).await;
    let profile = store
        .call(|db| {
            let profile = db.create_profile("Claude Agent")?;
            let account: ApiAccount = serde_json::from_value(json!({
                "id": "agent", "name": "Claude Agent", "provider": "claude-agent", "baseUrl": "",
                "prefix": "", "models": [{ "id": "claude-opus-5-5", "protocol": "anthropic" }],
                "disabled": false,
            }))
            .unwrap();
            db.save_api_account(&profile.id, &account, Some(&ApiKey::new("agent-key")))?;
            Ok(profile)
        })
        .await
        .unwrap();
    let reserved = Port::reserve();
    let port = reserved.number();
    let config = write_config(&store, profile.id.clone(), port, Some(8317))
        .await
        .unwrap();
    drop(reserved);
    let mut core = spawn_core(&binary, &DataDir::new(&data), &profile.id).unwrap();
    let reported = reported_config(port, config.management_key.expose_secret()).await;
    core.terminate().await.unwrap();
    store.close().await;
    assert!(applied(&config, &reported), "{reported}");
    let entry = &reported["claude-api-key"][0];
    assert_eq!(entry["cloak"], json!({ "mode": "never" }), "{reported}");
    assert_eq!(
        entry["base-url"],
        format!("http://127.0.0.1:8317/claude-agent/{}", profile.id)
    );
}
