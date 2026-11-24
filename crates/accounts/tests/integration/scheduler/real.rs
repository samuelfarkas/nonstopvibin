//! The scheduler driving the real pinned core (`bun run core:install` puts it at
//! `.vendor/core`) for an API-key profile on a stub upstream, in real time with short periods:
//! the first request starts the dormant core, its accounting reaches the store, a killed core
//! is started again, and an idle core stops. No OAuth credential is ever given to it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nsv_accounts::accounts::Accounts;
use nsv_accounts::restart::RestartTiming;
use nsv_accounts::scheduler::{Host, Scheduler, TIMING, Timing};
use nsv_core::binary::{CoreTarget, bundled_pin};
use nsv_core::pool::{BoxFuture, Pool, Processes, READY};
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::secret::ApiKey;
use nsv_domain::wire::ApiAccount;
use nsv_files::DataDir;
use nsv_net::{Loopback, LoopbackRequest, Method, loopback};
use nsv_store::{Store, Vault};
use nsv_testkit::TempDir;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Instant;

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
        nsv_core::binary::Binary::Pinned,
    )
    .unwrap();
    directory.join("cli-proxy-api")
}

/// An OpenAI-compatible upstream that answers every chat completion with fixed usage.
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
                                    .map(|value| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if body.len() >= length {
                            break;
                        }
                    }
                }
                let body = json!({
                    "id": "chatcmpl-stub", "object": "chat.completion", "created": 1, "model": "stub-model",
                    "choices": [{ "index": 0, "message": { "role": "assistant", "content": "hello from the stub" }, "finish_reason": "stop" }],
                    "usage": { "prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14 },
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
    port
}

#[derive(Default)]
struct Reports(Mutex<Vec<String>>);

impl Host for Reports {
    fn report(&self, message: String) {
        self.0.lock().unwrap().push(message);
    }

    fn now(&self) -> i64 {
        nsv_domain::time::epoch_ms(std::time::SystemTime::now())
    }

    fn signing_in(&self, _: &ProfileId) -> bool {
        false
    }

    fn refresh_quotas(&self, _: Pool, _: ProfileId) -> BoxFuture<'static, Result<(), AppError>> {
        Box::pin(async { Ok(()) })
    }
}

/// A process's command line from ps, not /proc, which macOS lacks. No argument the core gets
/// holds a space.
fn command_line(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// PIDs recorded in the data directory's core ledger.
fn ledger(data: &Path) -> Vec<u32> {
    std::fs::read_dir(DataDir::new(data).core_ledger()).map_or_else(
        |_| Vec::new(),
        |entries| {
            entries
                .filter_map(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_str()?
                        .strip_suffix(".json")?
                        .parse()
                        .ok()
                })
                .collect()
        },
    )
}

async fn until(what: &str, limit: Duration, mut condition: impl AsyncFnMut() -> bool) {
    let started = Instant::now();
    while !condition().await {
        assert!(started.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A data directory whose one profile an earlier run started, with an API-key subscription
/// on the stub upstream and no OAuth account, and a scheduler on the pinned core.
async fn real_world(
    root: &TempDir,
    idle: Duration,
) -> (PathBuf, Store, ProfileId, Scheduler, Arc<Reports>) {
    let binary = pinned_core();
    let data = root.path().join("data");
    nsv_files::owner_only_directory(&data, || Ok::<(), std::io::Error>(())).unwrap();
    let directory = DataDir::new(&data);
    let store = Store::open(&directory, Vault::open(&directory).unwrap())
        .await
        .unwrap();
    let upstream = stub_upstream().await;
    // A profile an earlier run started, with an API-key subscription and no OAuth account.
    let profile = store
        .call(move |db| {
            let mut profile = db.create_profile("Real core")?;
            profile.enabled = true;
            db.save_profile(&profile)?;
            let account: ApiAccount = serde_json::from_value(json!({
                "id": "stub-account", "name": "stub", "provider": "custom",
                "baseUrl": format!("http://127.0.0.1:{upstream}/v1"), "prefix": "",
                "models": [{ "id": "stub-model", "protocol": "openai" }], "disabled": false,
            }))?;
            db.save_api_account(&profile.id, &account, Some(&ApiKey::new("upstream-key")))?;
            Ok(profile.id)
        })
        .await
        .unwrap();
    // The default activity window puts the trailing usage drain long after the test's kill. A
    // drain in flight at the kill fails, and the scheduler reports a failed drain, as main did.
    let timing = Timing {
        restart: RestartTiming {
            coalesce: 200,
            ..TIMING.restart
        },
        usage_retry: Duration::from_millis(200),
        idle,
        ..*TIMING
    };
    let reports = Arc::new(Reports::default());
    let host: Arc<dyn Host> = Arc::<Reports>::clone(&reports);
    let (scheduler, ()) = Scheduler::new(
        store.clone(),
        binary,
        Arc::new(Processes::new(&directory)),
        READY,
        Arc::new(Accounts::new(store.clone())),
        timing,
        |_| (host, ()),
    );
    (data, store, profile, scheduler, reports)
}

#[tokio::test]
async fn the_pinned_core_starts_on_demand_reports_usage_restarts_after_a_kill_and_stops_when_idle()
{
    let root = TempDir::new("real-core");
    let idle = Duration::from_secs(3);
    let (data, store, profile, scheduler, reports) = real_world(&root, idle).await;
    let pool = scheduler.pool().clone();
    pool.reap_orphans().await.unwrap();
    scheduler.launch().await.unwrap();
    assert!(scheduler.is_dormant(&profile));
    assert!(!pool.state().alive(&profile), "not at launch");

    // A gateway request: counted in, the dormant core woken, sent to it, counted out.
    scheduler.activity(&profile, true);
    scheduler.wake(&profile).await.unwrap();
    let port = pool.port(&profile).unwrap();
    let id = profile.clone();
    let key = store.call(move |db| db.core_key(&id)).await.unwrap();
    let authorization = format!("Bearer {}", key.expose_secret());
    let response = loopback(LoopbackRequest {
        to: Loopback::v4(port),
        method: &Method::POST,
        target: "/v1/chat/completions",
        headers: &[
            ("Authorization", &authorization),
            ("Content-Type", "application/json"),
        ],
        body: Some(br#"{"model":"stub-model","messages":[{"role":"user","content":"hi"}]}"#),
        timeout: Duration::from_secs(15),
        limit: 1 << 20,
    })
    .await
    .unwrap();
    scheduler.activity(&profile, false);
    let text = String::from_utf8_lossy(&response.body).into_owned();
    assert!(response.ok(), "{} {text}", response.status);
    assert!(text.contains("hello from the stub"), "{text}");

    // Embedded models only, and the pinned core's own Claude fingerprint is left alone.
    let running = ledger(&data);
    let arguments = command_line(running[0]);
    assert!(arguments.contains("cli-proxy-api"), "{arguments}");
    assert!(
        !arguments
            .split_whitespace()
            .any(|argument| argument == "--local-model"),
        "{arguments}"
    );
    let reported = pool
        .management(&profile, &Method::GET, "/config", None)
        .await
        .unwrap();
    assert_eq!(
        reported["claude-header-defaults"]["user-agent"], "",
        "{reported}"
    );

    let rows = async || {
        let id = profile.clone();
        store.call(move |db| db.usage(&id, "", 100)).await.unwrap()
    };
    until(
        "the request's accounting",
        Duration::from_secs(10),
        async || !rows().await.is_empty(),
    )
    .await;
    let stored = rows().await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert_eq!(stored[0].model, "stub-model");
    assert_eq!((stored[0].input_tokens, stored[0].output_tokens), (11, 3));
    assert!(!stored[0].failed && stored[0].status_code == 200);

    // A crash: the restart policy starts it again, since it has a subscription.
    let crashed = ledger(&data);
    assert_eq!(crashed.len(), 1, "the running core is recorded");
    let killed = Command::new("kill")
        .args(["-KILL", &crashed[0].to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    until("the restarted core", Duration::from_secs(10), async || {
        let recorded = ledger(&data);
        pool.state().usable(&profile) && recorded.len() == 1 && recorded != crashed
    })
    .await;
    let restarted = Instant::now();

    // Unused, it stops once idle; its usage is drained first and it stays dormant.
    let stopped = async || !pool.state().alive(&profile);
    until("the idle stop", idle * 4, stopped).await;
    // Seen within one 50 ms poll after the new core started, so up to that much early.
    let up = restarted.elapsed() + Duration::from_millis(100);
    assert!(up >= idle, "stopped {up:?} after the restart");
    assert!(scheduler.is_dormant(&profile));
    assert_eq!(
        ledger(&data),
        Vec::<u32>::new(),
        "a stopped core leaves no record"
    );
    assert_eq!(rows().await.len(), 1, "accounting is stored exactly once");
    assert!(
        reports.0.lock().unwrap().is_empty(),
        "{:?}",
        reports.0.lock().unwrap()
    );
    scheduler.shutdown().await.unwrap();
    store.close().await;
}
