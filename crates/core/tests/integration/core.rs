//! Generated configs loaded into the real pinned core (`bun run core:install` puts it at
//! `.vendor/core`): it starts from a written config.yaml, routes a request for an API-key
//! account to a stub upstream, and reports every golden config's live entries as written.
//! No OAuth credential is ever given to it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nsv_core::binary::{CoreTarget, bundled_pin};
use nsv_core::config::{CoreConfiguration, applied, write_config};
use nsv_core::processes::{CoreProcess, spawn_core};
use nsv_domain::id::ProfileId;
use nsv_domain::secret::ApiKey;
use nsv_domain::wire::ApiAccount;
use nsv_net::{Loopback, LoopbackRequest, Method, Response, loopback};
use nsv_store::layout::{core_config_path, credential_directory};
use nsv_store::{Store, Vault};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::common::TempDir;
use crate::config::golden_config_cases;

/// The pinned core of this checkout, checked against the bundled pin before it runs.
fn pinned_core() -> PathBuf {
    let directory = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.vendor/core"));
    assert!(
        directory.join("cli-proxy-api").exists(),
        "the pinned core is missing: run bun run core:install"
    );
    nsv_core::check_core_directory(
        bundled_pin().unwrap(),
        directory,
        CoreTarget::current().unwrap(),
    )
    .unwrap();
    directory.join("cli-proxy-api")
}

/// A free loopback port; the kernel's ephemeral range is above 20000.
async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(port > 20000, "{port}");
    port
}

async fn call(
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

/// An OpenAI-compatible upstream that answers every chat completion and records each
/// request head it receives.
async fn stub_upstream() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&heads);
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0; 8192];
                // The head, then as many body bytes as it declares.
                let head = loop {
                    let read = stream.read(&mut buffer).await.unwrap();
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
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if body.len() >= length {
                            break head.to_owned();
                        }
                    }
                };
                seen.lock().unwrap().push(head);
                let body = json!({
                    "id": "chatcmpl-stub", "object": "chat.completion", "created": 1, "model": "stub-model",
                    "choices": [{ "index": 0, "message": { "role": "assistant", "content": "hello from the stub" }, "finish_reason": "stop" }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                let _ = stream.shutdown().await;
            });
        }
    });
    (port, heads)
}

async fn open(directory: &Path) -> Store {
    nsv_store::files::owner_only_directory(directory, || Ok::<(), std::io::Error>(())).unwrap();
    Store::open(directory, Vault::open(directory).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn the_pinned_core_loads_a_generated_config_and_answers_through_an_api_key_account() {
    let binary = pinned_core();
    let root = TempDir::new("real-core");
    let data = root.path().join("data");
    let store = open(&data).await;
    let (upstream, heads) = stub_upstream().await;
    let profile = store
        .call(|db| db.create_profile("Core check"))
        .await
        .unwrap();
    let account: ApiAccount = serde_json::from_value(json!({
        "id": "stub-account", "name": "stub", "provider": "custom",
        "baseUrl": format!("http://127.0.0.1:{upstream}/v1"), "prefix": "",
        "models": [{ "id": "stub-model", "protocol": "openai" }], "disabled": false,
    }))
    .unwrap();
    let id = profile.id.clone();
    store
        .call(move |db| db.save_api_account(&id, &account, Some(&ApiKey::new("upstream-key"))))
        .await
        .unwrap();
    let port = free_port().await;
    let config = write_config(&store, profile.id.clone(), port)
        .await
        .unwrap();
    let mut core = spawn_core(&binary, &data, &profile.id).unwrap();
    let reported = reported_config(port, config.management_key.expose_secret()).await;
    assert!(applied(&config, &reported), "{reported}");
    let response = call(
        port,
        &Method::POST,
        "/v1/chat/completions",
        config.core_key.expose_secret(),
        Some(br#"{"model":"stub-model","messages":[{"role":"user","content":"hi"}]}"#),
    )
    .await
    .unwrap();
    let text = String::from_utf8_lossy(&response.body).into_owned();
    assert!(response.ok(), "{} {text}", response.status);
    assert!(text.contains("hello from the stub"), "{text}");
    let heads = heads.lock().unwrap().clone();
    assert_eq!(heads.len(), 1, "{heads:?}");
    assert!(
        heads[0].starts_with("POST /v1/chat/completions HTTP/1.1"),
        "{}",
        heads[0]
    );
    assert!(
        heads[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer upstream-key"),
        "{}",
        heads[0]
    );
    // The client key of another profile, or none, is refused.
    let refused = call(port, &Method::GET, "/v1/models", "not-the-core-key", None)
        .await
        .unwrap();
    assert_eq!(refused.status.as_u16(), 401);
    core.terminate().await.unwrap();
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
    std::fs::create_dir_all(credential_directory(data, profile)).unwrap();
    std::fs::write(core_config_path(data, profile), config.to_yaml()).unwrap();
    let mut core: CoreProcess = spawn_core(binary, data, profile).unwrap();
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
        config.port = free_port().await;
        config.auth_dir = credential_directory(&data, &profile)
            .to_str()
            .unwrap()
            .to_owned();
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
