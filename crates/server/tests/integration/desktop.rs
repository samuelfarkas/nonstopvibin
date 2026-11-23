//! The daemon as the desktop shell sees it: a second launch's `POST /api/desktop/show`, which
//! only the desktop serves, and the tray's status, where a sleeping profile still counts as
//! running because its core starts on the next request.

use nsv_agents::AgentDirectories;
use nsv_desktop::HostEvent;
use nsv_domain::secret::{ApiKey, SessionToken};
use nsv_domain::wire::ApiAccount;
use nsv_files::DataDir;
use nsv_server::{Boundaries, CoreOptions, Daemon, Desktop, Options, Session};
use nsv_store::{Store, Vault};
use nsv_testkit::{TempDir, set_mode};
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

async fn daemon(root: &TempDir, desktop: Option<Desktop>) -> Daemon {
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
        agents: AgentDirectories::new(root.path().join("home")),
    };
    let boundaries = Boundaries {
        outside: std::sync::Arc::new(ModelsDev::new()),
        clock: now,
        ..Boundaries::production().unwrap()
    };
    let lock = DataDir::new(&data).lock().unwrap();
    let daemon = Daemon::start(options, boundaries, lock).await.unwrap();
    daemon.restore().await.unwrap();
    daemon
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
    // A token the daemon does not hold is refused, and nothing shows.
    let mut stale = daemon.session().unwrap();
    stale.token = SessionToken::new("stale-token");
    stale.write(&data).unwrap();
    assert_eq!(
        Session::show_main(&data).await.unwrap_err().to_string(),
        "NonstopVibin is already running but did not answer: it answered HTTP/1.1 401"
    );
    assert!(events.try_recv().is_err());
    // A server.json others may write is never trusted.
    set_mode(&data.session(), 0o644);
    assert_eq!(
        Session::show_main(&data).await.unwrap_err().to_string(),
        "server.json must be an owner-only ordinary file you own (chmod 600)."
    );
    daemon.session().unwrap().write(&data).unwrap();
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
