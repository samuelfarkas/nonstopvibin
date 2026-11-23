//! The daemon as the desktop shell sees it: a second launch's `POST /api/desktop/show`, which
//! only the desktop serves, and the tray's status, where a sleeping profile still counts as
//! running because its core starts on the next request.

use nsv_agents::AgentDirectories;
use nsv_domain::secret::ApiKey;
use nsv_domain::wire::ApiAccount;
use nsv_files::DataDir;
use nsv_server::{CoreOptions, Daemon, Options, TIMING, processes};
use nsv_shell::{HostEvent, ShellHost};
use nsv_store::{Store, Vault};
use nsv_testkit::TempDir;
use serde_json::json;

use crate::common::{ModelsDev, now, send};

/// A fresh data directory with one enabled API-key profile, which sleeps until a request.
async fn seeded(root: &TempDir) -> std::path::PathBuf {
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

async fn daemon(root: &TempDir, desktop: bool) -> Daemon<ModelsDev> {
    let data = seeded(root).await;
    let options = Options {
        port: 0,
        allow_hosts: Vec::new(),
        development: false,
        desktop,
        client: root.path().join("client"),
        core: CoreOptions {
            binary: root.path().join("no-core"),
            pinned: false,
            feed: None,
            allow_missing: true,
        },
        agents: AgentDirectories {
            home: root.path().join("home"),
            codex: None,
            opencode: None,
            pi: None,
        },
        backends: processes(),
        fetch: ModelsDev::new(),
        clock: now,
        timing: *TIMING,
    };
    let lock = DataDir::new(&data).lock().unwrap();
    let daemon = Daemon::start(options, lock).await.unwrap();
    daemon.restore().await.unwrap();
    daemon
}

#[tokio::test]
async fn a_second_launch_brings_the_desktop_window_forward() {
    let root = TempDir::new("desktop-show");
    let daemon = daemon(&root, true).await;
    let mut events = daemon.events();
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
    assert_eq!(daemon.tray_status().borrow().running, ["Sleeper"]);
    assert!(!daemon.tray_status().borrow().streaming);
    daemon.close().await.unwrap();
}

/// The shell reports a tray open for a profile that no longer exists ("Profile not found.",
/// supervisor.rs); the UI shows it from the state's errors, as `Application.report` did.
#[tokio::test]
async fn a_failed_open_reaches_the_errors_the_ui_shows() {
    let root = TempDir::new("desktop-report");
    let daemon = daemon(&root, true).await;
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

#[tokio::test]
async fn the_headless_server_has_no_desktop_route() {
    let root = TempDir::new("headless-show");
    let daemon = daemon(&root, false).await;
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
