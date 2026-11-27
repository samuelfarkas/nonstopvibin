//! Claude Agent through the daemon: the management API reports and connects a profile's
//! Claude Agent subscription with a scripted `claude` that only answers `initialize`, a
//! profile holds one such subscription and never beside Claude sign-ins, and the engine's
//! routes answer local agent clients only. No provider is contacted and no real core runs.

use std::path::{Path, PathBuf};

use nsv_server::AllowedHost;
use nsv_testkit::fake::response;
use nsv_testkit::world::{ACCOUNT, ORGANIZATION_A, claude};
use nsv_testkit::{TempDir, write_executable};
use serde_json::{Value, json};

use crate::common::send;
use crate::features::{Running, start_with};

/// A `claude` that answers `initialize` with `account`, then waits for its stdin to close.
fn script(directory: &Path, account: &Value) -> PathBuf {
    let answer = json!({ "type": "control_response", "response": {
        "subtype": "success", "request_id": "initialize",
        "response": {
            "models": [
                { "value": "default", "resolvedModel": "claude-opus-5-5" },
                { "value": "opus", "resolvedModel": "claude-opus-5-5" },
                { "value": "haiku", "resolvedModel": "claude-haiku-5-5" },
            ],
            "account": account,
        },
    } });
    let path = directory.join("claude");
    let body = format!("#!/bin/sh\nread line\nprintf '%s\\n' '{answer}'\ncat >/dev/null\n");
    write_executable(&path, body.as_bytes());
    path
}

async fn running(name: &str, account: &Value, allow_hosts: Vec<AllowedHost>) -> Running {
    let root = TempDir::new(name);
    let claude = script(root.path(), account);
    start_with(root, None, allow_hosts, Some(claude)).await
}

fn signed_in() -> Value {
    json!({ "tokenSource": "claude.ai", "email": "me@example.invalid", "apiProvider": "firstParty" })
}

async fn accounts(app: &Running, profile: &str) -> Vec<Value> {
    let state = app.ok("GET", "/state", None).await;
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

#[tokio::test]
async fn a_signed_in_claude_code_connects_one_claude_agent_subscription() {
    let app = running("claude-agent-connect", &signed_in(), Vec::new()).await;
    let profile = app.profile("Agent").await;
    let route = format!("/profiles/{profile}/claude-agent");
    let status = app.ok("GET", &route, None).await;
    let config = app.root.path().join("data/claude-agent").join(&profile);
    assert_eq!(
        status,
        json!({
            "claude": app.root.path().join("claude").to_str().unwrap(),
            "configDir": config.to_str().unwrap(),
            "signIn": format!(
                "CLAUDE_CONFIG_DIR={} {} auth login",
                config.display(),
                app.root.path().join("claude").display()
            ),
            "signedIn": true,
            "tokenSource": "claude.ai",
        })
    );
    let (status, _) = app.call("POST", &route, None).await;
    assert_eq!(status, 201);
    let listed = accounts(&app, &profile).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["provider"], "claude-agent");
    assert_eq!(listed[0]["kind"], "api-key");
    assert_eq!(listed[0]["name"], "me@example.invalid");
    assert_eq!(listed[0]["modelCount"], 2);
    // One per profile.
    let (status, refused) = app.call("POST", &route, None).await;
    assert_eq!(
        (status, &refused["error"]["message"]),
        (
            409,
            &json!("This profile already has a Claude Agent subscription.")
        )
    );
    // And never beside Claude sign-ins.
    let (status, refused) = app
        .call(
            "POST",
            &format!("/profiles/{profile}/oauth"),
            Some(json!({ "provider": "claude" })),
        )
        .await;
    assert_eq!(status, 409, "{refused}");
    let (status, refused) = app
        .call(
            "POST",
            &format!("/profiles/{profile}/import"),
            Some(json!({ "contents": claude(ORGANIZATION_A, "first") })),
        )
        .await;
    assert_eq!(status, 409, "{refused}");
    app.close().await;
}

#[tokio::test]
async fn claude_agent_is_refused_beside_claude_sign_ins_and_before_claude_code_signs_in() {
    let signed_out = json!({ "tokenSource": "none", "apiProvider": "firstParty" });
    let app = running("claude-agent-refused", &signed_out, Vec::new()).await;
    let profile = app.profile("Agent").await;
    let route = format!("/profiles/{profile}/claude-agent");
    let status = app.ok("GET", &route, None).await;
    assert_eq!(status["signedIn"], false);
    let (code, refused) = app.call("POST", &route, None).await;
    assert_eq!(code, 409);
    assert_eq!(
        refused["error"]["message"],
        format!(
            "Sign Claude Code in for this profile first: {}",
            status["signIn"].as_str().unwrap()
        )
    );
    assert_eq!(accounts(&app, &profile).await, Vec::<Value>::new());
    app.close().await;

    let app = running("claude-agent-seats", &signed_in(), Vec::new()).await;
    let seats = app.profile("Seats").await;
    app.outside.answer(|_| {
        Ok(response(
            200,
            json!({
                "account": { "uuid": ACCOUNT, "email": "same@example.invalid" },
                "organization": { "uuid": ORGANIZATION_A, "name": "Example A" },
            })
            .to_string(),
        ))
    });
    app.ok(
        "POST",
        &format!("/profiles/{seats}/import"),
        Some(json!({ "contents": claude(ORGANIZATION_A, "first") })),
    )
    .await;
    let (code, refused) = app
        .call("POST", &format!("/profiles/{seats}/claude-agent"), None)
        .await;
    assert_eq!(code, 409, "{refused}");
    assert_eq!(accounts(&app, &seats).await.len(), 1);

    // A Claude credential the core holds but the account list has not caught up with, as after
    // an import whose sync failed, refuses it too.
    let unsynced = app.profile("Unsynced").await;
    let auth = app
        .root
        .path()
        .join("data/profiles")
        .join(&unsynced)
        .join("auth");
    std::fs::create_dir_all(&auth).unwrap();
    std::fs::write(
        auth.join("claude-unsynced.json"),
        claude(ORGANIZATION_A, "second").to_string(),
    )
    .unwrap();
    let (code, refused) = app
        .call("POST", &format!("/profiles/{unsynced}/claude-agent"), None)
        .await;
    assert_eq!(code, 409, "{refused}");
    app.close().await;
}

#[tokio::test]
async fn the_engine_answers_local_agent_clients_only() {
    let proxy = "nonstopvibin.tailnet.example";
    let app = running(
        "claude-agent-routes",
        &signed_in(),
        vec![AllowedHost::parse(proxy).unwrap()],
    )
    .await;
    let profile = app.profile("Agent").await;
    app.ok("POST", &format!("/profiles/{profile}/claude-agent"), None)
        .await;
    let path = format!("/claude-agent/{profile}/v1/models");
    let request = async |headers: &[(&str, &str)]| {
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        let answer = send(app.daemon.port(), "GET", &path, &headers, b"").await;
        let body: Value = serde_json::from_slice(&answer.body).unwrap();
        (answer.status, body)
    };
    // The engine itself refuses a request without the account's key.
    let (status, body) = request(&[]).await;
    assert_eq!(
        (status, &body),
        (
            401,
            &json!({ "type": "error", "error": { "type": "authentication_error", "message": "Invalid API key." } })
        )
    );
    let (status, body) = request(&[("Host", proxy)]).await;
    assert_eq!(
        (status, &body["error"]["message"]),
        (
            403,
            &json!("The agent API is only available on the loopback address.")
        )
    );
    let origin = format!("http://127.0.0.1:{}", app.daemon.port());
    let (status, body) = request(&[("Origin", &origin)]).await;
    assert_eq!(
        (status, &body["error"]["message"]),
        (
            403,
            &json!("The agent API accepts local agent clients, not browser origins.")
        )
    );
    // The management API's session token is no key here either.
    let token = format!("Bearer {}", app.daemon.session_token().expose_secret());
    assert_eq!(request(&[("Authorization", &token)]).await.0, 401);
    app.close().await;
}
