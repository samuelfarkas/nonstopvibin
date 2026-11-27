//! A profile's usage hub, as T3 Code v0.1.13 reads it: its URL and key from the management
//! API, then `/p/<slug>/v0/management/*` with that key, over the fake cores. No provider is
//! contacted and no real core runs.

use std::sync::{Arc, Mutex};

use nsv_domain::id::ProfileId;
use nsv_domain::wire::Account;
use nsv_files::{DataDir, owner_only_directory};
use nsv_net::USER_AGENT;
use nsv_server::AllowedHost;
use nsv_store::{Store, Vault};
use nsv_testkit::TempDir;
use serde_json::{Value, json};

use crate::common::send;
use crate::features::{Running, start};

const CODEX_USAGE: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_CREDITS: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const CODEX_CONSUME: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";
const CLAUDE_USAGE: &str = "https://api.anthropic.com/api/oauth/usage";
const XAI_CREDITS: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
/// A redemption as T3 Code encodes it.
const REDEMPTION: &str =
    r#"{"redeem_request_id":"6f1c2a9e-2d4b-5c1e-9a7f-3b8d5e0c1a42","credit_id":"credit-1"}"#;

async fn running(name: &str) -> Running {
    start(TempDir::new(name), None, Vec::new()).await
}

/// The management API's view of a profile's hub: its URL and key.
async fn usage_hub(app: &Running, method: &str, profile: &str) -> (String, String) {
    let hub = app
        .ok(method, &format!("/profiles/{profile}/usage-hub"), None)
        .await;
    (
        hub["url"].as_str().unwrap().to_owned(),
        hub["key"].as_str().unwrap().to_owned(),
    )
}

/// A hub request with `headers` and a raw body: the status and the JSON answer.
async fn raw(
    app: &Running,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, Value) {
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    let answer = send(app.daemon.port(), method, path, &headers, body).await;
    (answer.status, serde_json::from_slice(&answer.body).unwrap())
}

/// A hub request as T3 Code sends it: `Authorization: Bearer <key>`, and JSON for a POST.
async fn hub(
    app: &Running,
    method: &str,
    path: &str,
    key: &str,
    body: Option<&Value>,
) -> (u16, Value) {
    let authorization = format!("Bearer {key}");
    let body = body.map_or_else(Vec::new, |body| body.to_string().into_bytes());
    raw(
        app,
        method,
        path,
        &[("Authorization", &authorization)],
        &body,
    )
    .await
}

/// The error message of a refusal.
fn refusal(answer: &(u16, Value)) -> (u16, &Value) {
    (answer.0, &answer.1["error"]["message"])
}

/// Alpha's core lists a Codex, a Claude and a Grok subscription, a paused Claude one, a Gemini
/// one and an API key; Beta's a Codex subscription.
fn alpha_listing() -> Value {
    json!({ "files": [
        {
            "name": "codex-user@example.invalid.json", "type": "codex", "provider": "codex",
            "auth_index": "alpha-codex", "email": "codex@example.invalid", "disabled": false,
            "status": "active",
            "id_token": { "chatgpt_account_id": "workspace-alpha", "plan_type": "pro" },
        },
        {
            "name": "claude.json", "provider": "claude", "auth_index": "alpha-claude",
            "email": "claude@example.invalid", "disabled": false,
        },
        { "name": "xai.json", "provider": "xai", "auth_index": "alpha-xai", "disabled": false },
        {
            "name": "claude-paused.json", "provider": "claude", "auth_index": "alpha-paused",
            "email": "paused@example.invalid", "disabled": true,
        },
        { "name": "gemini.json", "provider": "gemini", "auth_index": "alpha-gemini", "disabled": false },
        {
            "name": "codex-key", "provider": "codex", "account_type": "api_key",
            "auth_index": "alpha-api-key", "disabled": false,
        },
    ] })
}

struct Hubs {
    app: Running,
    alpha: ProfileId,
    beta: ProfileId,
    /// Alpha's hub key.
    key: String,
    beta_key: String,
}

impl Hubs {
    async fn call(&self, path: &str, body: &Value) -> (u16, Value) {
        let path = format!("/p/alpha/v0/management/{path}");
        hub(&self.app, "POST", &path, &self.key, Some(body)).await
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let path = format!("/p/alpha/v0/management/{path}");
        hub(&self.app, "GET", &path, &self.key, None).await
    }

    fn calls(&self, profile: &ProfileId, path: &str) -> usize {
        self.app.fake.calls(profile, path).len()
    }
}

/// Profiles Alpha and Beta, running, with their accounts synced.
async fn hubs(name: &str) -> Hubs {
    let app = running(name).await;
    let alpha = ProfileId::parse(&app.profile("Alpha").await).unwrap();
    let beta = ProfileId::parse(&app.profile("Beta").await).unwrap();
    let beta_listing = json!({ "files": [{
        "name": "codex-beta.json", "provider": "codex", "auth_index": "beta-codex",
        "disabled": false, "id_token": { "chatgpt_account_id": "workspace-beta" },
    }] });
    app.fake.knobs(|knobs| {
        knobs.listings.insert(alpha.clone(), alpha_listing());
        knobs.listings.insert(beta.clone(), beta_listing);
    });
    for profile in [&alpha, &beta] {
        app.ok("POST", &format!("/profiles/{profile}/start"), None)
            .await;
    }
    let key = usage_hub(&app, "GET", alpha.as_str()).await.1;
    let beta_key = usage_hub(&app, "GET", beta.as_str()).await.1;
    Hubs {
        app,
        alpha,
        beta,
        key,
        beta_key,
    }
}

fn api_call(auth_index: &str, method: &str, url: &str) -> Value {
    // T3 Code's own headers, which the hub ignores.
    json!({
        "auth_index": auth_index, "method": method, "url": url,
        "header": {
            "Authorization": "Bearer $TOKEN$", "Originator": "Codex Desktop",
            "X-Injected": "never forwarded",
        },
    })
}

fn with_data(mut call: Value, data: &str) -> Value {
    call.as_object_mut()
        .unwrap()
        .insert("data".to_owned(), json!(data));
    call
}

#[tokio::test]
async fn the_usage_hub_is_the_profile_route_and_keeps_its_key_until_replaced() {
    let app = running("hub-route").await;
    let profile = app.profile("Work Hub").await;
    let (url, key) = usage_hub(&app, "GET", &profile).await;
    assert_eq!(
        url,
        format!("http://127.0.0.1:{}/p/work-hub", app.daemon.port())
    );
    let shape = key.strip_prefix("nvh_").unwrap();
    assert_eq!(shape.len(), 43, "{key}");
    assert_eq!(
        usage_hub(&app, "GET", &profile).await,
        (url.clone(), key.clone())
    );

    let (replaced_url, replaced) = usage_hub(&app, "POST", &profile).await;
    assert_eq!(replaced_url, url);
    assert_ne!(replaced, key);
    assert_eq!(usage_hub(&app, "GET", &profile).await.1, replaced);
    app.close().await;
}

#[tokio::test]
async fn only_the_profile_s_own_hub_key_opens_its_hub() {
    let hubs = hubs("hub-auth").await;
    let app = &hubs.app;
    let files = "/p/alpha/v0/management/auth-files";
    let client_key = app
        .ok("GET", &format!("/profiles/{}/key", hubs.alpha), None)
        .await["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let session = app.daemon.session_token().expose_secret().to_owned();
    let invalid = json!("Invalid usage hub key.");
    for key in [
        client_key.as_str(),
        session.as_str(),
        hubs.beta_key.as_str(),
        "nvh_guess",
    ] {
        let answer = hub(app, "GET", files, key, None).await;
        assert_eq!(refusal(&answer), (401, &invalid), "{key}");
    }
    let lowercase = format!("bearer {}", hubs.key);
    for headers in [
        vec![],
        vec![("Authorization", lowercase.as_str())],
        vec![("x-api-key", hubs.key.as_str())],
    ] {
        let answer = raw(app, "GET", files, &headers, b"").await;
        assert_eq!(refusal(&answer), (401, &invalid), "{headers:?}");
    }
    // An unknown profile answers as a wrong key does.
    let unknown = hub(
        app,
        "GET",
        "/p/nobody/v0/management/auth-files",
        &hubs.key,
        None,
    )
    .await;
    assert_eq!(refusal(&unknown), (401, &invalid));
    assert_eq!(hubs.get("auth-files").await.0, 200);

    let (_, replaced) = usage_hub(app, "POST", hubs.alpha.as_str()).await;
    let old = hub(app, "GET", files, &hubs.key, None).await;
    assert_eq!(refusal(&old), (401, &invalid));
    assert_eq!(hub(app, "GET", files, &replaced, None).await.0, 200);
    hubs.app.close().await;
}

#[tokio::test]
async fn the_hub_answers_its_four_endpoints_and_only_on_loopback() {
    let proxy = "hub.example.ts.net";
    let app = start(
        TempDir::new("hub-paths"),
        None,
        vec![AllowedHost::parse(proxy).unwrap()],
    )
    .await;
    let profile = app.profile("Alpha").await;
    let (_, key) = usage_hub(&app, "GET", &profile).await;
    let not_found = json!("Not found.");
    for (method, path) in [
        ("GET", "/p/alpha/v0/management/config"),
        ("GET", "/p/alpha/v0/management/auth-files/download"),
        ("POST", "/p/alpha/v0/management/auth-files"),
        ("DELETE", "/p/alpha/v0/management/auth-files"),
        ("GET", "/p/alpha/v0/management/api-call"),
        ("GET", "/p/alpha/v0/management"),
    ] {
        let answer = hub(&app, method, path, &key, None).await;
        assert_eq!(refusal(&answer), (404, &not_found), "{method} {path}");
    }
    let files = "/p/alpha/v0/management/auth-files";
    let authorization = format!("Bearer {key}");
    let proxied = raw(
        &app,
        "GET",
        files,
        &[("Host", proxy), ("Authorization", &authorization)],
        b"",
    )
    .await;
    assert_eq!(
        refusal(&proxied),
        (
            403,
            &json!("The agent API is only available on the loopback address.")
        )
    );
    let origin = format!("http://127.0.0.1:{}", app.daemon.port());
    let browser = raw(
        &app,
        "GET",
        files,
        &[("Origin", &origin), ("Authorization", &authorization)],
        b"",
    )
    .await;
    assert_eq!(
        refusal(&browser),
        (
            403,
            &json!("The agent API accepts local agent clients, not browser origins.")
        )
    );
    assert_eq!(
        hub(&app, "GET", files, &key, None).await,
        (200, json!({ "files": [] }))
    );
    app.close().await;
}

#[tokio::test]
async fn auth_files_list_the_hub_accounts_and_nothing_else() {
    let hubs = hubs("hub-files").await;
    assert_eq!(
        hubs.get("auth-files").await,
        (
            200,
            json!({ "files": [
                {
                    "id": "codex-user@example.invalid.json", "auth_index": "alpha-codex",
                    "provider": "codex", "email": "codex@example.invalid", "disabled": false,
                    "id_token": { "chatgpt_account_id": "workspace-alpha" },
                },
                {
                    "id": "claude.json", "auth_index": "alpha-claude", "provider": "claude",
                    "email": "claude@example.invalid", "disabled": false,
                },
                { "id": "xai.json", "auth_index": "alpha-xai", "provider": "xai", "disabled": false },
                {
                    "id": "claude-paused.json", "auth_index": "alpha-paused", "provider": "claude",
                    "email": "paused@example.invalid", "disabled": true,
                },
            ] })
        )
    );
    hubs.app.close().await;
}

#[tokio::test]
async fn t3_code_s_calls_reach_only_the_profile_s_core_with_this_app_s_headers() {
    let hubs = hubs("hub-calls").await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    hubs.app.fake.knobs(|knobs| {
        knobs.upstream = Arc::new(move |call: &Value| {
            recorded.lock().unwrap().push(call.clone());
            match call["url"].as_str().unwrap() {
                XAI_CREDITS => json!({ "status_code": 429, "body": "synthetic provider detail" }),
                CLAUDE_USAGE => {
                    json!({ "status_code": 200, "body": { "five_hour": { "utilization": 25 } } })
                }
                _ => json!({ "status_code": 200, "body": r#"{"answered":true}"# }),
            }
        });
    });
    let calls = [
        api_call("alpha-codex", "GET", CODEX_USAGE),
        api_call("alpha-codex", "GET", CODEX_CREDITS),
        with_data(api_call("alpha-codex", "POST", CODEX_CONSUME), REDEMPTION),
        api_call("alpha-claude", "GET", CLAUDE_USAGE),
        api_call("alpha-xai", "GET", XAI_CREDITS),
    ];
    let mut answers = Vec::new();
    for call in &calls {
        answers.push(hubs.call("api-call", call).await);
    }
    let answered = json!({ "status_code": 200, "body": r#"{"answered":true}"# });
    assert_eq!(
        answers,
        [
            (200, answered.clone()),
            (200, answered.clone()),
            (200, answered),
            (
                200,
                json!({ "status_code": 200, "body": r#"{"five_hour":{"utilization":25}}"# })
            ),
            (200, json!({ "status_code": 429, "body": "" })),
        ]
    );
    let codex = json!({
        "Authorization": "Bearer $TOKEN$", "Content-Type": "application/json",
        "Chatgpt-Account-Id": "workspace-alpha", "User-Agent": USER_AGENT,
    });
    let bearer = json!({ "Authorization": "Bearer $TOKEN$", "Content-Type": "application/json" });
    let claude = json!({
        "Authorization": "Bearer $TOKEN$", "Content-Type": "application/json",
        "anthropic-beta": "oauth-2025-04-20",
    });
    assert_eq!(
        *seen.lock().unwrap(),
        [
            json!({ "auth_index": "alpha-codex", "method": "GET", "url": CODEX_USAGE, "header": codex }),
            json!({ "auth_index": "alpha-codex", "method": "GET", "url": CODEX_CREDITS, "header": codex }),
            json!({
                "auth_index": "alpha-codex", "method": "POST", "url": CODEX_CONSUME,
                "header": codex, "data": REDEMPTION,
            }),
            json!({ "auth_index": "alpha-claude", "method": "GET", "url": CLAUDE_USAGE, "header": claude }),
            json!({ "auth_index": "alpha-xai", "method": "GET", "url": XAI_CREDITS, "header": bearer }),
        ]
    );
    assert_eq!(hubs.calls(&hubs.alpha, "/api-call"), 5);
    assert_eq!(hubs.calls(&hubs.beta, "/api-call"), 0);
    hubs.app.close().await;
}

#[tokio::test]
async fn refused_calls_never_reach_a_core() {
    let hubs = hubs("hub-refused").await;
    let refused = [
        (
            api_call(
                "alpha-codex",
                "GET",
                "https://chatgpt.com/backend-api/wham/tasks",
            ),
            403,
        ),
        (
            api_call("alpha-codex", "GET", &format!("{CODEX_USAGE}?x=1")),
            403,
        ),
        (
            with_data(api_call("alpha-codex", "POST", CODEX_USAGE), "{}"),
            403,
        ),
        (api_call("alpha-codex", "get", CODEX_USAGE), 403),
        // A Claude token never goes to chatgpt.com.
        (api_call("alpha-claude", "GET", CODEX_USAGE), 403),
        (api_call("alpha-gemini", "GET", CODEX_USAGE), 404),
        (api_call("alpha-api-key", "GET", CODEX_USAGE), 404),
        (api_call("unknown", "GET", CODEX_USAGE), 404),
        (api_call("beta-codex", "GET", CODEX_USAGE), 404),
        (api_call("alpha-paused", "GET", CLAUDE_USAGE), 409),
        (
            with_data(api_call("alpha-codex", "GET", CODEX_USAGE), "{}"),
            400,
        ),
        (api_call("alpha-codex", "POST", CODEX_CONSUME), 400),
        (
            with_data(
                api_call("alpha-codex", "POST", CODEX_CONSUME),
                r#"{"redeem_request_id":"r","credit_id":"c","extra":"x"}"#,
            ),
            400,
        ),
        (
            with_data(
                api_call("alpha-codex", "POST", CODEX_CONSUME),
                r#"{"redeem_request_id":"r"}"#,
            ),
            400,
        ),
        (
            with_data(
                api_call("alpha-codex", "POST", CODEX_CONSUME),
                r#"{"redeem_request_id":1,"credit_id":"c"}"#,
            ),
            400,
        ),
        (json!({ "method": "GET", "url": CODEX_USAGE }), 400),
    ];
    for (call, status) in &refused {
        let answer = hubs.call("api-call", call).await;
        assert_eq!(answer.0, *status, "{call} {}", answer.1);
    }
    let authorization = format!("Bearer {}", hubs.key);
    let path = "/p/alpha/v0/management/api-call";
    let headers = [("Authorization", authorization.as_str())];
    let invalid = raw(&hubs.app, "POST", path, &headers, b"not json").await;
    assert_eq!(refusal(&invalid), (400, &json!("Invalid JSON.")));
    let large = format!(
        r#"{{"auth_index":"alpha-codex","method":"GET","url":"{CODEX_USAGE}","pad":"{}"}}"#,
        "x".repeat(64 * 1024)
    );
    let oversized = raw(&hubs.app, "POST", path, &headers, large.as_bytes()).await;
    assert_eq!(refusal(&oversized), (413, &json!("Payload exceeds 64 KB.")));
    assert_eq!(hubs.calls(&hubs.alpha, "/api-call"), 0);
    assert_eq!(hubs.calls(&hubs.beta, "/api-call"), 0);
    hubs.app.close().await;
}

#[tokio::test]
async fn reset_quota_clears_only_the_profile_s_own_account() {
    let hubs = hubs("hub-reset-quota").await;
    assert_eq!(
        hubs.call("reset-quota", &json!({ "auth_index": "alpha-codex" }))
            .await,
        (200, json!({ "status": "ok" }))
    );
    let other = hubs
        .call("reset-quota", &json!({ "auth_index": "beta-codex" }))
        .await;
    assert_eq!(
        refusal(&other),
        (404, &json!("Account not found in this profile."))
    );
    assert_eq!(hubs.call("reset-quota", &json!({})).await.0, 400);
    assert_eq!(hubs.app.fake.quota_resets(&hubs.alpha), ["alpha-codex"]);
    assert_eq!(hubs.app.fake.quota_resets(&hubs.beta), Vec::<String>::new());
    hubs.app.close().await;
}

#[tokio::test]
async fn models_lists_only_the_ids_the_core_routes_to_a_hub_account() {
    let hubs = hubs("hub-models").await;
    // T3 Code's `encodeURIComponent` of the account id.
    let models = hubs
        .get("auth-files/models?name=codex-user%40example.invalid.json")
        .await;
    assert_eq!(
        models,
        (
            200,
            json!({ "models": [{ "id": "model-for-codex-user@example.invalid.json" }] })
        )
    );
    assert_eq!(
        hubs.app.fake.calls(&hubs.alpha, "/auth-files/models"),
        [(
            nsv_net::Method::GET,
            "/auth-files/models?name=codex-user%40example.invalid.json".to_owned()
        )]
    );
    for name in ["gemini.json", "codex-beta.json", "unknown.json"] {
        let answer = hubs.get(&format!("auth-files/models?name={name}")).await;
        assert_eq!(answer.0, 404, "{name}");
    }
    assert_eq!(hubs.get("auth-files/models").await.0, 400);
    assert_eq!(hubs.calls(&hubs.alpha, "/auth-files/models"), 1);
    assert_eq!(hubs.calls(&hubs.beta, "/auth-files/models"), 0);
    hubs.app.close().await;
}

#[tokio::test]
async fn a_stopped_profile_lists_its_accounts_and_starts_no_core_for_a_call() {
    let root = TempDir::new("hub-stopped");
    let data = root.path().join("data");
    owner_only_directory(&data, || Ok::<(), std::io::Error>(())).unwrap();
    let store = Store::open(
        &DataDir::new(&data),
        Vault::open(&DataDir::new(&data)).unwrap(),
    )
    .await
    .unwrap();
    // A profile whose accounts were synced in an earlier run and that was never started since.
    let account: Account = serde_json::from_value(json!({
        "id": "codex.json", "name": "Codex", "provider": "codex", "kind": "oauth",
        "disabled": false, "status": "Ready", "priority": 0, "authIndex": "stopped-codex",
    }))
    .unwrap();
    let profile = store
        .call(move |db| {
            let profile = db.create_profile("Alpha")?;
            db.save_account_metadata(&profile.id, &[account])?;
            Ok(profile.id)
        })
        .await
        .unwrap();
    store.close().await;
    let app = start(root, None, Vec::new()).await;
    let (_, key) = usage_hub(&app, "GET", profile.as_str()).await;
    assert!(app.fake.launches().is_empty());

    let base = "/p/alpha/v0/management";
    assert_eq!(
        hub(&app, "GET", &format!("{base}/auth-files"), &key, None).await,
        (
            200,
            json!({ "files": [{
                "id": "codex.json", "auth_index": "stopped-codex", "provider": "codex",
                "disabled": false,
            }] })
        )
    );
    let not_started = json!("Start this profile first.");
    let call = api_call("stopped-codex", "GET", CODEX_USAGE);
    let reset = json!({ "auth_index": "stopped-codex" });
    for answer in [
        hub(&app, "POST", &format!("{base}/api-call"), &key, Some(&call)).await,
        hub(
            &app,
            "POST",
            &format!("{base}/reset-quota"),
            &key,
            Some(&reset),
        )
        .await,
        hub(
            &app,
            "GET",
            &format!("{base}/auth-files/models?name=codex.json"),
            &key,
            None,
        )
        .await,
    ] {
        assert_eq!(refusal(&answer), (409, &not_started));
    }
    assert!(app.fake.launches().is_empty(), "the hub started a core");
    app.close().await;
}
