//! The in-process daemon's start, revalidation and idle stop, ported from
//! tests/server-lifecycle.test.ts, server-mode.test.ts, integration.test.ts and
//! lazy-cores.test.ts: a failed start leaves the data directory free, unhashed assets
//! revalidate across launches and edits, and an idle core stops and starts again over HTTP.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nsv_accounts::scheduler::{TIMING, Timing};
use nsv_agents::AgentDirectories;
use nsv_core::install::CoreSource;
use nsv_domain::secret::ApiKey;
use nsv_domain::wire::ApiAccount;
use nsv_files::DataDir;
use nsv_server::{Boundaries, CoreOptions, Daemon, Options};
use nsv_store::{Store, Vault};
use nsv_testkit::TempDir;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::common::{ModelsDev, REPOSITORY, RawResponse, now, send};

fn pinned_core() -> PathBuf {
    Path::new(REPOSITORY).join(".vendor/core/cli-proxy-api")
}

/// A desktop-less daemon on `data` and its options, serving the interface from `root/client`.
struct Run {
    data: DataDir,
    options: Options,
    boundaries: Boundaries,
}

fn options(root: &TempDir, data: &Path, core: CoreOptions, timing: Timing) -> Run {
    nsv_files::owner_only_directory(data, || Ok::<(), std::io::Error>(())).unwrap();
    let options = Options {
        port: 0,
        allow_hosts: Vec::new(),
        development: false,
        desktop: None,
        client: root.path().join("client"),
        core,
        agents: AgentDirectories::new(root.path().join("home")),
    };
    Run {
        data: DataDir::new(data),
        options,
        boundaries: Boundaries {
            outside: std::sync::Arc::new(ModelsDev::new()),
            clock: now,
            timing,
            ..Boundaries::production().unwrap()
        },
    }
}

fn core(binary: PathBuf, source: CoreSource) -> CoreOptions {
    CoreOptions { binary, source }
}

async fn start(run: Run) -> Daemon {
    let lock = run.data.lock().unwrap();
    let daemon = Daemon::start(run.options, run.boundaries, lock)
        .await
        .unwrap();
    daemon.restore().await.unwrap();
    daemon
}

/// A start or restore that fails, closed as the CLI closes it: its message, once the data
/// directory is free again.
async fn refused(run: Run) -> String {
    let lock = run.data.lock().unwrap();
    let message = match Daemon::start(run.options, run.boundaries, lock).await {
        Err(error) => error.to_string(),
        Ok(daemon) => {
            let Err(error) = daemon.restore().await else {
                panic!("the daemon started");
            };
            daemon.close().await.unwrap();
            error.to_string()
        }
    };
    run.data.lock().unwrap().release();
    message
}

async fn get(port: u16, path: &str, headers: &[(&str, &str)]) -> RawResponse {
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    send(port, "GET", path, &headers, b"").await
}

#[tokio::test]
async fn a_start_that_fails_leaves_the_data_directory_free() {
    let root = TempDir::new("failed-start");
    let missing = root.path().join("absent-core/cli-proxy-api");
    let message = refused(options(
        &root,
        &root.path().join("strict"),
        core(missing.clone(), CoreSource::Checkout),
        *TIMING,
    ))
    .await;
    assert_eq!(
        message,
        format!(
            "The proxy core is missing at {}. Reinstall NonstopVibin.",
            missing.display()
        )
    );

    // A core that matches only its adjacent manifest fails the compiled-in pin.
    let swapped = root.path().join("swapped-core");
    std::fs::create_dir_all(&swapped).unwrap();
    std::fs::write(swapped.join("cli-proxy-api"), "#!/bin/sh\necho replaced\n").unwrap();
    std::fs::write(
        swapped.join("manifest.json"),
        json!({ "binarySha256": "5fd216da978480337276a1c5ef8d633fa621213941913ffed1bdad0e906836d1" })
            .to_string(),
    )
    .unwrap();
    let message = refused(options(
        &root,
        &root.path().join("pinned"),
        core(swapped.join("cli-proxy-api"), CoreSource::Checkout),
        *TIMING,
    ))
    .await;
    assert_eq!(
        message,
        "The proxy core failed its integrity check. Reinstall NonstopVibin."
    );

    let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = taken.local_addr().unwrap().port();
    let mut busy = options(
        &root,
        &root.path().join("bind"),
        core(pinned_core(), CoreSource::Checkout),
        *TIMING,
    );
    busy.options.port = port;
    let message = refused(busy).await;
    assert!(message.contains("already in use"), "{message}");

    // A development run keeps the allowance: it opens without its core.
    let lenient = start(options(
        &root,
        &root.path().join("lenient"),
        core(missing, CoreSource::Development),
        *TIMING,
    ))
    .await;
    // Two closes at once both finish, and leave the directory free.
    let (first, second) = tokio::join!(lenient.close(), lenient.close());
    first.unwrap();
    second.unwrap();
    DataDir::new(root.path().join("lenient"))
        .lock()
        .unwrap()
        .release();
}

#[tokio::test]
async fn unhashed_assets_revalidate_across_launches_and_edits_but_management_is_never_cached() {
    let root = TempDir::new("static");
    let client = root.path().join("client");
    std::fs::create_dir_all(&client).unwrap();
    std::fs::write(client.join("index.html"), "fixture page").unwrap();
    let launch = |name: &str| {
        options(
            &root,
            &root.path().join(name),
            core(root.path().join("no-core"), CoreSource::Development),
            *TIMING,
        )
    };
    let served = start(launch("served")).await;
    let first = get(served.port(), "/", &[]).await;
    assert_eq!(first.body, b"fixture page");
    let etag = first.header("etag").unwrap().to_owned();

    // Unhashed assets revalidate across launches, even with identical timestamps.
    let restarted = start(launch("restarted")).await;
    let fresh = get(restarted.port(), "/", &[("If-None-Match", &etag)]).await;
    assert_eq!(fresh.status, 200);
    assert_ne!(fresh.header("etag"), Some(etag.as_str()));
    restarted.close().await.unwrap();

    std::fs::write(client.join("index.html"), "updated fixture page").unwrap();
    let updated = get(served.port(), "/", &[("If-None-Match", &etag)]).await;
    assert_eq!(updated.status, 200);
    assert_eq!(updated.body, b"updated fixture page");

    let token = format!("Bearer {}", served.session_token().expose_secret());
    let state = get(
        served.port(),
        "/api/state",
        &[("Authorization", &token), ("If-None-Match", "*")],
    )
    .await;
    assert_eq!(state.status, 200);
    assert_eq!(state.header("cache-control"), Some("no-store"));
    served.close().await.unwrap();
}

/// An OpenAI-compatible provider that answers every chat completion with 14 tokens.
async fn provider() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = vec![0; 8192];
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
                let body = json!({
                    "id": "fixture", "object": "chat.completion", "created": 1, "model": "fixture-model",
                    "choices": [{ "index": 0, "message": { "role": "assistant", "content": "hello" }, "finish_reason": "stop" }],
                    "usage": { "prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14 },
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    port
}

#[tokio::test]
async fn an_idle_core_stops_after_its_last_request_keeps_its_usage_and_starts_again() {
    let root = TempDir::new("idle");
    let data = root.path().join("data");
    let timing = Timing {
        idle: Duration::from_millis(400),
        ..*TIMING
    };
    let options = options(
        &root,
        &data,
        core(pinned_core(), CoreSource::Checkout),
        timing,
    );
    let upstream = provider().await;
    let store = Store::open(&options.data, Vault::open(&options.data).unwrap())
        .await
        .unwrap();
    let (id, slug, key) = store
        .call(move |db| {
            let mut profile = db.create_profile("Lazy fixture")?;
            profile.enabled = true;
            db.save_profile(&profile)?;
            let account: ApiAccount = serde_json::from_value(json!({
                "id": "lazy-key", "name": "lazy-key", "provider": "custom",
                "baseUrl": format!("http://127.0.0.1:{upstream}/v1"), "prefix": "",
                "models": [{ "id": "fixture-model", "protocol": "openai" }], "disabled": false,
            }))?;
            db.save_api_account(&profile.id, &account, Some(&ApiKey::new("lazy-key")))?;
            let key = db.client_key(&profile.id)?;
            Ok((
                profile.id.to_string(),
                profile.slug.as_str().to_owned(),
                key.expose_secret().to_owned(),
            ))
        })
        .await
        .unwrap();
    store.close().await;
    let daemon = start(options).await;
    let port = daemon.port();
    let token = format!("Bearer {}", daemon.session_token().expose_secret());
    let api = async |path: &str| -> Value {
        serde_json::from_slice(&get(port, path, &[("Authorization", &token)]).await.body).unwrap()
    };
    let runtime = async || -> String {
        api("/api/state").await["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|profile| profile["slug"] == slug)
            .unwrap()["runtime"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let chat = async || {
        let headers = [
            ("Authorization".to_owned(), format!("Bearer {key}")),
            ("Content-Type".to_owned(), "application/json".to_owned()),
        ];
        let body = br#"{"model":"fixture-model","messages":[{"role":"user","content":"hello"}]}"#;
        send(
            port,
            "POST",
            &format!("/p/{slug}/v1/chat/completions"),
            &headers,
            body,
        )
        .await
        .status
    };
    assert_eq!(runtime().await, "stopped");
    assert_eq!(chat().await, 200);
    let deadline = Instant::now() + Duration::from_secs(20);
    while runtime().await != "stopped" {
        assert!(Instant::now() < deadline, "the idle core did not stop");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Every request was collected before the core stopped.
    let usage = api(&format!("/api/profiles/{id}/usage?days=1")).await;
    assert_eq!(usage["summary"]["requests"], 1, "{usage}");

    assert_eq!(chat().await, 200);
    assert_eq!(runtime().await, "running");
    daemon.close().await.unwrap();
}
