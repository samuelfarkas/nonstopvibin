//! Sign-in, credentials, resets, API accounts and core updates through the daemon's HTTP API:
//! the daemon assembles its own `Cores` over the fake cores and the stub outside world of
//! `nsv_testkit`. No provider is contacted and no real core runs.

use std::sync::Arc;

use nsv_agents::AgentDirectories;
use nsv_core::binary::bundled_pin;
use nsv_core::install::CoreSource;
use nsv_core::pool::Backend;
use nsv_desktop::HostEvent;
use nsv_files::DataDir;
use nsv_server::{AllowedHost, Boundaries, CoreOptions, Daemon, Desktop, Options};
use nsv_testkit::fake::{Directory, FakeCores, response};
use nsv_testkit::world::{StubFetch, UPDATE, bundled_core, release};
use nsv_testkit::{Port, TempDir};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::common::now;

const FEED: &str = "https://feed.example.invalid/core-release.json";

pub(crate) struct Running {
    pub(crate) root: TempDir,
    pub(crate) daemon: Daemon,
    desktop: Desktop,
    pub(crate) fake: FakeCores,
    pub(crate) outside: Arc<StubFetch>,
    token: String,
}

/// A desktop daemon on a fresh data directory; `feed` makes it an installed build.
async fn running(name: &str, feed: Option<&str>) -> Running {
    start(TempDir::new(name), feed, Vec::new()).await
}

/// A desktop daemon on `root/data`, which may hold a store already, answering `allow_hosts`
/// too; `feed` makes it an installed build.
pub(crate) async fn start(
    root: TempDir,
    feed: Option<&str>,
    allow_hosts: Vec<AllowedHost>,
) -> Running {
    start_with(root, feed, allow_hosts, None).await
}

/// [`start`], with Claude Agent running `claude`.
pub(crate) async fn start_with(
    root: TempDir,
    feed: Option<&str>,
    allow_hosts: Vec<AllowedHost>,
    claude: Option<std::path::PathBuf>,
) -> Running {
    let data = root.path().join("data");
    nsv_files::owner_only_directory(&data, || Ok::<(), std::io::Error>(())).unwrap();
    let fake = FakeCores::default();
    let outside = Arc::new(StubFetch::new(|_| Ok(response(404, "{}"))));
    let backends_fake = fake.clone();
    let desktop = Desktop::default();
    let options = Options {
        port: 0,
        allow_hosts,
        development: false,
        desktop: Some(desktop.clone()),
        client: root.path().join("client"),
        core: CoreOptions {
            binary: bundled_core(root.path()),
            source: feed.map_or(CoreSource::Development, |feed| CoreSource::Package {
                feed: feed.to_owned(),
                pinned: false,
            }),
        },
        agents: AgentDirectories::new(root.path().join("home")),
    };
    let boundaries = Boundaries {
        backends: Arc::new(move |data: &DataDir| -> Arc<dyn Backend> {
            Arc::new(Directory(backends_fake.clone(), data.to_owned()))
        }),
        outside: Arc::<StubFetch>::clone(&outside),
        clock: now,
        claude,
        ..Boundaries::production().unwrap()
    };
    let lock = DataDir::new(&data).lock().unwrap();
    let daemon = Daemon::start(options, boundaries, lock).await.unwrap();
    daemon.restore().await.unwrap();
    let token = format!("Bearer {}", daemon.session_token().expose_secret());
    Running {
        root,
        daemon,
        desktop,
        fake,
        outside,
        token,
    }
}

impl Running {
    /// `/api<path>` with the session token: the status and the JSON body.
    pub(crate) async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut headers = vec![("Authorization".to_owned(), self.token.clone())];
        let bytes = body.map_or_else(Vec::new, |body| {
            headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
            body.to_string().into_bytes()
        });
        let answer = crate::common::send(
            self.daemon.port(),
            method,
            &format!("/api{path}"),
            &headers,
            &bytes,
        )
        .await;
        (answer.status, serde_json::from_slice(&answer.body).unwrap())
    }

    pub(crate) async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.call(method, path, body).await;
        assert!(
            (200..300).contains(&status),
            "{method} {path}: {status} {value}"
        );
        value
    }

    /// A new profile's id.
    pub(crate) async fn profile(&self, name: &str) -> String {
        let created = self
            .ok("POST", "/profiles", Some(json!({ "name": name })))
            .await;
        created["id"].as_str().unwrap().to_owned()
    }

    async fn accounts(&self, profile: &str) -> Vec<Value> {
        let state = self.ok("GET", "/state", None).await;
        state["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|listed| listed["id"] == profile)
            .unwrap()["accounts"]
            .as_array()
            .unwrap()
            .clone()
    }

    pub(crate) async fn close(self) {
        self.daemon.close().await.unwrap();
        drop(self.root);
    }
}

/// A profile key removed from the database by another connection, such as another process or a
/// hand edit, is refused on the very next request, as main's gateway read the live rows; other
/// profiles keep theirs.
#[tokio::test]
async fn a_key_removed_outside_the_store_is_refused_on_the_next_request() {
    let running = running("features-removed-key", None).await;
    let removed = running.profile("Removed").await;
    let kept = running.profile("Kept").await;
    let key = async |profile: &str| {
        let answer = running
            .ok("GET", &format!("/profiles/{profile}/key"), None)
            .await;
        answer["key"].as_str().unwrap().to_owned()
    };
    let (removed_key, kept_key) = (key(&removed).await, key(&kept).await);
    let status = async |key: &str| {
        let authorization = ("Authorization".to_owned(), format!("Bearer {key}"));
        crate::common::send(
            running.daemon.port(),
            "GET",
            "/v1/models",
            &[authorization],
            b"",
        )
        .await
        .status
    };
    // Accepted keys reach the stopped-profile check; refused ones never do.
    assert_eq!(status(&removed_key).await, 503);
    let database = running.root.path().join("data/nonstopvibin.sqlite");
    let outside = rusqlite::Connection::open(database).unwrap();
    outside
        .execute("DELETE FROM profiles WHERE id=?", [&removed])
        .unwrap();
    assert_eq!(status(&removed_key).await, 401);
    outside
        .execute(
            "DELETE FROM secrets WHERE id LIKE ?",
            [format!("{removed}:%")],
        )
        .unwrap();
    assert_eq!(status(&removed_key).await, 401);
    assert_eq!(status(&kept_key).await, 503);
    running.close().await;
}

fn codex() -> Value {
    json!({
        "type": "codex",
        "email": "codex@example.invalid",
        "access_token": "synthetic-codex-access",
        "refresh_token": "synthetic-codex-refresh",
    })
}

fn pasted(state: &str) -> Value {
    json!({ "url": format!("http://localhost:1455/auth/callback?code=synthetic&state={state}") })
}

#[tokio::test]
async fn a_pasted_callback_signs_in_and_a_pending_sign_in_can_be_cancelled() {
    let app = running("http-signin", None).await;
    let profile = app.profile("Headless").await;
    app.fake.knobs(|knobs| knobs.credential = codex());
    let oauth = format!("/profiles/{profile}/oauth");
    let session = app
        .ok("POST", &oauth, Some(json!({ "provider": "codex" })))
        .await;
    let state = session["state"].as_str().unwrap();
    let pending = app
        .ok("GET", &format!("/profiles/{profile}/oauth-session"), None)
        .await;
    assert_eq!(pending["state"], state);
    app.ok(
        "POST",
        &format!("/profiles/{profile}/callback"),
        Some(pasted(state)),
    )
    .await;
    let status = app.ok("GET", &format!("{oauth}?state={state}"), None).await;
    assert_eq!(status["status"], "ok", "{status}");
    assert_eq!(app.accounts(&profile).await.len(), 1);

    let next = app
        .ok("POST", &oauth, Some(json!({ "provider": "codex" })))
        .await;
    let state = next["state"].as_str().unwrap();
    app.ok("DELETE", &format!("{oauth}?state={state}"), None)
        .await;
    let none = app
        .ok("GET", &format!("/profiles/{profile}/oauth-session"), None)
        .await;
    assert_eq!(none, Value::Null);
    let (status, gone) = app
        .call("GET", &format!("{oauth}?state={state}"), None)
        .await;
    assert_eq!(status, 404, "{gone}");
    app.close().await;
}

#[tokio::test]
async fn a_browser_callback_signs_in_and_brings_the_desktop_window_forward() {
    let app = running("http-browser", None).await;
    let profile = app.profile("Desktop").await;
    let reserved = Port::reserve();
    let port = reserved.number();
    assert!(port > 20000, "{port}");
    app.fake.knobs(|knobs| {
        knobs.redirect = Some(format!("http://localhost:{port}/auth/callback"));
        knobs.credential = codex();
    });
    let mut events = app.desktop.events();
    let oauth = format!("/profiles/{profile}/oauth");
    // The sign-in's callback listener binds it.
    drop(reserved);
    let session = app
        .ok("POST", &oauth, Some(json!({ "provider": "codex" })))
        .await;
    let state = session["state"].as_str().unwrap();
    let mut browser = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    browser
        .write_all(
            format!(
                "GET /auth/callback?code=synthetic&state={state} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut answer = String::new();
    browser.read_to_string(&mut answer).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert_eq!(events.recv().await.unwrap(), HostEvent::SignInAccepted);
    let status = app.ok("GET", &format!("{oauth}?state={state}"), None).await;
    assert_eq!(status["status"], "ok", "{status}");
    app.close().await;
}

#[tokio::test]
async fn imported_accounts_change_and_go_away() {
    let app = running("http-credentials", None).await;
    let profile = app.profile("Edits").await;
    // A CLIProxyAPI credential the import catalog finds under the agents' home.
    let sources = app.root.path().join("home/.cli-proxy-api");
    std::fs::create_dir_all(&sources).unwrap();
    std::fs::write(sources.join("codex.json"), codex().to_string()).unwrap();
    let listed = app.ok("GET", "/import-sources", None).await;
    let source = listed.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    app.ok(
        "POST",
        &format!("/profiles/{profile}/import-source"),
        Some(json!({ "sourceId": source })),
    )
    .await;
    // A credential file the user picked.
    let kimi = json!({ "type": "kimi", "access_token": "synthetic-kimi-access" });
    app.ok(
        "POST",
        &format!("/profiles/{profile}/import"),
        Some(json!({ "contents": kimi })),
    )
    .await;
    let accounts = app.accounts(&profile).await;
    assert_eq!(accounts.len(), 2, "{accounts:?}");
    let account = accounts
        .iter()
        .find(|account| account["provider"] == "codex")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let path = format!("/profiles/{profile}/accounts/{account}");
    app.ok(
        "PATCH",
        &path,
        Some(json!({ "disabled": true, "name": "Work", "priority": 3 })),
    )
    .await;
    let changed = app
        .accounts(&profile)
        .await
        .into_iter()
        .find(|listed| listed["id"] == account.as_str())
        .unwrap();
    assert_eq!(
        (&changed["status"], &changed["name"], &changed["priority"]),
        (&json!("Paused"), &json!("Work"), &json!(3))
    );
    app.ok("DELETE", &path, None).await;
    assert_eq!(app.accounts(&profile).await.len(), 1);
    let (status, missing) = app.call("DELETE", &path, None).await;
    assert_eq!(
        (status, &missing["error"]["message"]),
        (404, &json!("Account not found in this profile."))
    );
    app.close().await;
}

#[tokio::test]
async fn an_api_account_is_added_once_and_its_profile_starts() {
    let app = running("http-api-account", None).await;
    let profile = app.profile("Company").await;
    let input = json!({
        "name": "Company API",
        "provider": "custom",
        "baseUrl": "https://api.example.invalid/v1",
        "apiKey": "synthetic-upstream-key",
        "prefix": "",
        "models": [],
    });
    // Without model ids, the stub upstream lists them.
    app.outside.answer(|url| {
        assert_eq!(url, "https://api.example.invalid/v1/models");
        Ok(response(
            200,
            json!({ "data": [{ "id": "discovered-a" }, { "id": "discovered-b" }] }).to_string(),
        ))
    });
    let path = format!("/profiles/{profile}/api-account");
    let (status, state) = app.call("POST", &path, Some(input.clone())).await;
    assert_eq!(status, 201, "{state}");
    let listed = &state["profiles"][0];
    assert_eq!(listed["runtime"], "running");
    assert_eq!(listed["accounts"][0]["kind"], "api-key");
    assert_eq!(listed["accounts"][0]["modelCount"], 2);
    let (status, duplicate) = app.call("POST", &path, Some(input)).await;
    assert_eq!(
        (status, &duplicate["error"]["message"]),
        (
            409,
            &json!("This API key is already connected to this profile.")
        )
    );
    let seen = app.outside.seen.lock().unwrap().clone();
    let authorization = seen[0]
        .request
        .headers
        .iter()
        .find(|(name, _)| *name == "Authorization")
        .map(|(_, value)| value.as_str());
    assert_eq!(authorization, Some("Bearer synthetic-upstream-key"));
    app.close().await;
}

#[tokio::test]
async fn codex_reset_credits_are_listed_and_redeemed_through_the_core() {
    let app = running("http-resets", None).await;
    let profile = app.profile("Resets").await;
    let path = format!("/profiles/{profile}/accounts/codex.json/reset-credits");
    let (status, stopped) = app.call("GET", &path, None).await;
    assert_eq!(status, 409, "{stopped}");
    app.fake.knobs(|knobs| {
        knobs.upstream = Arc::new(|call: &Value| {
            assert_eq!(call["header"]["Chatgpt-Account-Id"], "fixture-identity");
            let url = call["url"].as_str().unwrap();
            if url == "https://chatgpt.com/backend-api/wham/usage" {
                json!({ "status_code": 200, "body": { "rate_limit": { "primary_window": { "used_percent": 0, "limit_window_seconds": 18000 } } } })
            } else if call["method"] == "GET" {
                json!({ "status_code": 200, "body": { "available_count": 1, "credits": [{
                    "id": "credit-one", "reset_type": "rate_limit", "status": "available",
                    "expires_at": "2026-12-01T00:00:00+00:00", "title": null,
                }] } })
            } else {
                json!({ "status_code": 200, "body": { "code": "reset" } })
            }
        });
    });
    let raw = json!({
        "type": "codex", "access_token": "synthetic-codex",
        "id_token": { "chatgpt_account_id": "fixture-identity" },
    });
    app.ok(
        "POST",
        &format!("/profiles/{profile}/import"),
        Some(json!({ "contents": raw })),
    )
    .await;
    let account = app.accounts(&profile).await[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = format!("/profiles/{profile}/accounts/{account}/reset-credits");
    let credits = app.ok("GET", &path, None).await;
    assert_eq!(credits["available_count"], 1, "{credits}");
    let result = app
        .ok("POST", &path, Some(json!({ "creditId": "credit-one" })))
        .await;
    assert_eq!(
        (&result["code"], &result["quotaRefreshed"]),
        (&json!("reset"), &json!(true)),
        "{result}"
    );
    app.close().await;
}

#[tokio::test]
async fn an_installed_build_checks_installs_and_reverts_its_core() {
    let development = running("http-core-dev", None).await;
    let (status, refused) = development.call("POST", "/core/check", None).await;
    assert_eq!(status, 409, "{refused}");
    development.close().await;

    let app = running("http-core", Some(FEED)).await;
    let (archive, pin) = release(app.root.path(), b"update core");
    *app.outside.releases.feed.lock().unwrap() = pin;
    *app.outside.releases.archive.lock().unwrap() = archive;
    let bundled = bundled_pin().unwrap().version.to_string();
    let checked = app.ok("POST", "/core/check", None).await;
    assert_eq!(
        (&checked["coreVersion"], &checked["core"]["available"]),
        (&json!(bundled), &json!(UPDATE))
    );
    let installed = app
        .ok("POST", "/core/install", Some(json!({ "version": UPDATE })))
        .await;
    assert_eq!(installed["coreVersion"], UPDATE, "{installed}");
    assert_eq!(installed["core"]["available"], Value::Null);
    let reverted = app.ok("DELETE", "/core/install", None).await;
    assert_eq!(reverted["coreVersion"], bundled.as_str(), "{reverted}");
    let (status, again) = app.call("DELETE", "/core/install", None).await;
    assert_eq!(status, 409, "{again}");
    app.close().await;
}
