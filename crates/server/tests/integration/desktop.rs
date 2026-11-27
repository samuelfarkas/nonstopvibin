//! The daemon as the desktop shell sees it: a second launch's `POST /api/desktop/show`, which
//! only the desktop serves, and the tray's status, where a sleeping profile still counts as
//! running because its core starts on the next request.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nsv_accounts::Backends;
use nsv_agents::AgentDirectories;
use nsv_core::install::CoreSource;
use nsv_core::pool::Backend;
use nsv_desktop::HostEvent;
use nsv_domain::secret::{ApiKey, SessionToken};
use nsv_domain::wire::{Account, ApiAccount};
use nsv_files::DataDir;
use nsv_server::{Boundaries, CoreOptions, Daemon, Desktop, Options, Session};
use nsv_store::{Store, Vault};
use nsv_testkit::fake::{Directory, FakeCores};
use nsv_testkit::world::bundled_core;
use nsv_testkit::{TempDir, set_mode};
use serde_json::json;

use crate::common::{ModelsDev, now, send};

/// A fresh data directory with one enabled API-key profile, which sleeps until a request.
async fn seeded(root: &TempDir) -> PathBuf {
    let data = root.path().join("data");
    nsv_files::owner_only_directory(&data, || Ok::<(), std::io::Error>(())).unwrap();
    let directory = DataDir::new(&data);
    let store = Store::open(&directory, Vault::open(&directory).unwrap())
        .await
        .unwrap();
    store
        .call(|db| {
            let mut profile = db.create_profile("Sleeper")?;
            profile.enabled = true;
            db.save_profile(&profile)?;
            let account: ApiAccount = serde_json::from_value(json!({
                "id": "account", "name": "stub", "provider": "custom",
                "baseUrl": "http://127.0.0.1:9/v1", "prefix": "",
                "models": [{ "id": "stub-model", "protocol": "openai" }], "disabled": false,
            }))?;
            db.save_api_account(&profile.id, &account, Some(&ApiKey::new("upstream-key")))
        })
        .await
        .unwrap();
    store.close().await;
    data
}

async fn daemon(root: &TempDir, desktop: Option<Desktop>) -> Daemon {
    let daemon = start(root, &seeded(root).await, desktop, None).await;
    daemon.restore().await.unwrap();
    daemon
}

/// The daemon of `data`, accepting but not yet restored. Without `fake` cores it has no core.
async fn start(
    root: &TempDir,
    data: &Path,
    desktop: Option<Desktop>,
    fake: Option<&FakeCores>,
) -> Daemon {
    let production = Boundaries::production().unwrap();
    let (binary, backends) = match fake {
        None => (root.path().join("no-core"), production.backends),
        Some(fake) => {
            let fake = fake.clone();
            let backends: Backends = Arc::new(move |data: &DataDir| -> Arc<dyn Backend> {
                Arc::new(Directory(fake.clone(), data.to_owned()))
            });
            (bundled_core(root.path()), backends)
        }
    };
    let options = Options {
        port: 0,
        allow_hosts: Vec::new(),
        development: false,
        desktop,
        client: root.path().join("client"),
        core: CoreOptions {
            binary,
            source: CoreSource::Development,
        },
        agents: AgentDirectories::new(root.path().join("home")),
    };
    let boundaries = Boundaries {
        backends,
        outside: Arc::new(ModelsDev::new()),
        clock: now,
        ..production
    };
    let lock = DataDir::new(data).lock().unwrap();
    Daemon::start(options, boundaries, lock).await.unwrap()
}

/// Claude sign-ins are deleted before the daemon answers anything: a request during restore
/// could otherwise start a core that loads one.
#[tokio::test]
async fn claude_sign_ins_are_gone_before_the_daemon_serves() {
    let root = TempDir::new("desktop-retired");
    let data = seeded(&root).await;
    let directory = DataDir::new(&data);
    let store = Store::open(&directory, Vault::open(&directory).unwrap())
        .await
        .unwrap();
    let profile = store.call(|db| db.profiles()).await.unwrap()[0].id.clone();
    store.close().await;
    let credential = directory.credentials(&profile).join("claude-a.json");
    std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
    std::fs::write(
        &credential,
        json!({ "type": "claude", "access_token": "synthetic" }).to_string(),
    )
    .unwrap();
    let daemon = start(&root, &data, None, None).await;
    assert!(!credential.exists());
    daemon.close().await.unwrap();
}

#[tokio::test]
async fn a_second_launch_brings_the_desktop_window_forward() {
    let root = TempDir::new("desktop-show");
    let desktop = Desktop::default();
    let daemon = daemon(&root, Some(desktop.clone())).await;
    let mut events = desktop.events();
    let token = format!("Bearer {}", daemon.session_token().expose_secret());
    let refused = send(daemon.port(), "POST", "/api/desktop/show", &[], b"").await;
    assert_eq!(refused.status, 401);
    let secret = daemon.session_token().expose_secret().to_owned();
    for presented in [
        format!("bearer {secret}"),
        format!("Bearer {secret}x"),
        format!("Bearer  {secret}"),
        secret,
    ] {
        let refused = send(
            daemon.port(),
            "POST",
            "/api/desktop/show",
            &[("Authorization".to_owned(), presented)],
            b"",
        )
        .await;
        assert_eq!(refused.status, 401);
    }
    let shown = send(
        daemon.port(),
        "POST",
        "/api/desktop/show",
        &[("Authorization".to_owned(), token.clone())],
        b"",
    )
    .await;
    assert_eq!(shown.status, 200);
    assert_eq!(events.try_recv().unwrap(), HostEvent::ShowMain);
    // The Host names the port exactly: no sign or leading zero.
    let port = daemon.port();
    for host in [format!("127.0.0.1:0{port}"), format!("localhost:+{port}")] {
        let refused = send(
            port,
            "POST",
            "/api/desktop/show",
            &[
                ("Host".to_owned(), host),
                ("Authorization".to_owned(), token.clone()),
            ],
            b"",
        )
        .await;
        assert_eq!(refused.status, 403);
    }
    // The sleeping profile serves, so the tray shows it running.
    assert_eq!(desktop.tray_status().borrow().running, ["Sleeper"]);
    assert!(!desktop.tray_status().borrow().streaming);
    daemon.close().await.unwrap();
}

/// A second desktop launch finds the running daemon through its `server.json`, read and
/// validated as `url` and `status` read it, and brings its window forward with its token.
#[tokio::test]
async fn a_second_launch_shows_the_running_window_through_server_json() {
    let root = TempDir::new("desktop-second");
    let desktop = Desktop::default();
    let daemon = daemon(&root, Some(desktop.clone())).await;
    let data = DataDir::new(root.path().join("data"));
    let mut events = desktop.events();
    Session::show_main(&data).await.unwrap();
    assert_eq!(events.try_recv().unwrap(), HostEvent::ShowMain);
    // A server.json others may write is never trusted.
    set_mode(&data.session(), 0o644);
    assert_eq!(
        Session::show_main(&data).await.unwrap_err().to_string(),
        "server.json must be an owner-only ordinary file you own (chmod 600)."
    );
    // A token the daemon does not hold, as an earlier run's file holds, is refused and nothing
    // shows; the launch waits until server.json names the daemon's own.
    let mut stale = daemon.session().unwrap();
    stale.token = SessionToken::new("stale-token");
    stale.write(&data).unwrap();
    let (shown, ()) = tokio::join!(Session::show_main(&data), async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(events.try_recv().is_err());
        daemon.session().unwrap().write(&data).unwrap();
    });
    shown.unwrap();
    assert_eq!(events.try_recv().unwrap(), HostEvent::ShowMain);
    daemon.close().await.unwrap();
}

/// Holding the data directory's lock proves an earlier run's `server.json` stale: it is gone
/// once the daemon starts, long before restore writes the new one.
#[tokio::test]
async fn a_start_removes_the_server_json_an_unclean_exit_left() {
    let root = TempDir::new("desktop-stale");
    let data = seeded(&root).await;
    let directory = DataDir::new(&data);
    let stale = Session {
        pid: 4242,
        port: 9,
        token: SessionToken::new("stale-token"),
        allow_hosts: Vec::new(),
        version: "0.2.0".to_owned(),
        started_at: now(),
    };
    stale.write(&directory).unwrap();
    let daemon = start(&root, &data, Some(Desktop::default()), None).await;
    assert!(!directory.session().exists());
    daemon.close().await.unwrap();
}

/// A second launch while the first still starts the cores it keeps running, up to 15 s each:
/// server.json is there once restore begins, and the shell, which starts after restore, still
/// hears the request.
#[tokio::test]
async fn a_second_launch_during_restore_shows_the_window_once_the_shell_listens() {
    let root = TempDir::new("desktop-restoring");
    let data = seeded(&root).await;
    let directory = DataDir::new(&data);
    let store = Store::open(&directory, Vault::open(&directory).unwrap())
        .await
        .unwrap();
    // A subscription keeps its profile running, so restore starts that core.
    store
        .call(|db| {
            let mut profile = db.create_profile("Kept")?;
            profile.enabled = true;
            db.save_profile(&profile)?;
            let account: Account = serde_json::from_value(json!({
                "id": "codex.json", "name": "Codex", "provider": "codex", "kind": "oauth",
                "disabled": false, "status": "Ready", "priority": 0, "authIndex": "1",
            }))?;
            db.save_account_metadata(&profile.id, &[account])
        })
        .await
        .unwrap();
    store.close().await;
    let fake = FakeCores::default();
    fake.knobs(|knobs| knobs.silent = true);
    let desktop = Desktop::default();
    let daemon = start(&root, &data, Some(desktop.clone()), Some(&fake)).await;
    tokio::select! {
        restored = daemon.restore() => panic!("restore ended first: {restored:?}"),
        shown = Session::show_main(&directory) => shown.unwrap(),
    }
    assert_eq!(desktop.events().try_recv().unwrap(), HostEvent::ShowMain);
    daemon.close().await.unwrap();
}

/// The shell reports a tray open for a profile that no longer exists ("Profile not found.",
/// supervisor.rs); the UI shows it from the state's errors, as `Application.report` did.
#[tokio::test]
async fn a_failed_open_reaches_the_errors_the_ui_shows() {
    let root = TempDir::new("desktop-report");
    let desktop = Desktop::default();
    let daemon = daemon(&root, Some(desktop.clone())).await;
    daemon.report("Profile not found.");
    let token = format!("Bearer {}", daemon.session_token().expose_secret());
    let state = send(
        daemon.port(),
        "GET",
        "/api/state",
        &[("Authorization".to_owned(), token)],
        b"",
    )
    .await;
    assert_eq!(state.status, 200);
    let state: serde_json::Value = serde_json::from_slice(&state.body).unwrap();
    assert_eq!(state["errors"], json!(["Profile not found."]));
    daemon.close().await.unwrap();
}

/// Node joined repeated Origin fields, so a request with a second one matched no accepted
/// origin: the management API and the agent gateway refuse it.
#[tokio::test]
async fn a_request_with_two_origins_is_refused() {
    let root = TempDir::new("two-origins");
    let daemon = daemon(&root, None).await;
    let port = daemon.port();
    let own = format!("http://127.0.0.1:{port}");
    let token = format!("Bearer {}", daemon.session_token().expose_secret());
    let field = |name: &str, value: &str| (name.to_owned(), value.to_owned());
    let state = |origins: &[&str]| {
        let mut headers = vec![field("Authorization", &token)];
        headers.extend(origins.iter().map(|origin| field("Origin", origin)));
        async move { send(port, "GET", "/api/state", &headers, b"").await }
    };
    assert_eq!(state(&[&own]).await.status, 200);
    for origins in [[own.as_str(), "https://example.invalid"], ["", &own]] {
        let refused = state(&origins).await;
        assert_eq!(refused.status, 403, "{origins:?}");
        let body: serde_json::Value = serde_json::from_slice(&refused.body).unwrap();
        assert_eq!(
            body["error"]["message"],
            "Cross-origin management requests are not allowed."
        );
        let headers: Vec<_> = origins
            .iter()
            .map(|origin| field("Origin", origin))
            .collect();
        let gateway = send(port, "GET", "/v1/models", &headers, b"").await;
        assert_eq!(gateway.status, 403, "{origins:?}");
    }
    daemon.close().await.unwrap();
}

#[tokio::test]
async fn the_headless_server_has_no_desktop_route() {
    let root = TempDir::new("headless-show");
    let daemon = daemon(&root, None).await;
    let token = format!("Bearer {}", daemon.session_token().expose_secret());
    let missing = send(
        daemon.port(),
        "POST",
        "/api/desktop/show",
        &[("Authorization".to_owned(), token)],
        b"",
    )
    .await;
    assert_eq!(missing.status, 404);
    // A WebSocket handshake with a browser Origin never reaches the gateway.
    let upgrade = send(
        daemon.port(),
        "GET",
        "/v1/responses",
        &[
            (
                "Origin".to_owned(),
                format!("http://127.0.0.1:{}", daemon.port()),
            ),
            ("Connection".to_owned(), "Upgrade".to_owned()),
            ("Upgrade".to_owned(), "websocket".to_owned()),
        ],
        b"",
    )
    .await;
    assert_eq!(upgrade.status, 403);
    assert_eq!(upgrade.header("content-length"), Some("0"));
    daemon.close().await.unwrap();
    // Closing removed server.json and freed the directory.
    assert!(!root.path().join("data/server.json").exists());
    DataDir::new(root.path().join("data"))
        .lock()
        .unwrap()
        .release();
}
