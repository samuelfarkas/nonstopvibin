//! Credential changes and Codex resets through fake cores (`tests/integration.test.ts`,
//! `tests/reset-credits.test.ts`), and the refresh route's order (`tests/core-restart.test.ts`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::registry::ResetResultCode;
use nsv_domain::wire::{AccountPatch, AccountStatus, ApiAccountInput};
use nsv_net::Method;
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, watch};

use crate::common::{case, cases, response};
use crate::world::World;

fn api_input(key: &str) -> ApiAccountInput {
    serde_json::from_value(json!({
        "name": "Company API",
        "provider": "custom",
        "baseUrl": "https://api.example.invalid/v1",
        "apiKey": key,
        "prefix": "",
        "models": [{ "id": "fixture-model", "protocol": "openai" }],
    }))
    .unwrap()
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

#[tokio::test]
async fn an_api_subscription_can_join_several_profiles_but_only_once_each() {
    let world = World::new("changes-api").await;
    let (company, personal) = (
        world.profile("Company").await,
        world.profile("Personal").await,
    );
    let credentials = &world.cores.credentials;
    credentials
        .add_api_account(&company, api_input("shared-key"))
        .await
        .unwrap();
    assert!(
        world.cores.pool().running(&company),
        "adding one starts its profile"
    );
    let duplicate = credentials
        .add_api_account(&company, api_input("shared-key"))
        .await
        .unwrap_err();
    assert_eq!(
        (duplicate.message.as_str(), duplicate.status),
        ("This API key is already connected to this profile.", 409)
    );
    credentials
        .add_api_account(&company, api_input("other-key"))
        .await
        .unwrap();
    credentials
        .add_api_account(&personal, api_input("shared-key"))
        .await
        .unwrap();
    let listed = world.cores.accounts.list(&company).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .all(|account| account.status == AccountStatus::Configured)
    );
    assert_eq!(world.cores.accounts.list(&personal).await.unwrap().len(), 1);
    let refused = credentials
        .add_api_account(
            &company,
            serde_json::from_value(json!({
                "name": "x", "provider": "custom", "baseUrl": "http://example.invalid/v1",
                "apiKey": "k", "prefix": "", "models": [{ "id": "m", "protocol": "openai" }],
            }))
            .unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.status, 400);
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one account's edits, in order")]
async fn account_pause_resume_rename_and_removal_reach_the_core_and_the_store() {
    let world = World::new("changes-edits").await;
    let profile = world.profile("Edits").await;
    let credentials = &world.cores.credentials;
    let raw = json!({ "type": "codex", "access_token": "synthetic-codex", "refresh_token": "synthetic-refresh" });
    let account = credentials.import(&profile, object(&raw)).await.unwrap();
    let name = account.id.as_str();
    assert!(name.starts_with("codex-"), "{name}");
    assert_eq!(
        std::path::Path::new(name).extension(),
        Some("json".as_ref())
    );
    credentials
        .set_account(
            &profile,
            &account.id,
            AccountPatch {
                disabled: Some(true),
                priority: Some(3),
                name: Some("Work".to_owned()),
            },
        )
        .await
        .unwrap();
    let file: Value = serde_json::from_str(
        &std::fs::read_to_string(world.credential_file(&profile, account.id.as_str())).unwrap(),
    )
    .unwrap();
    assert_eq!(
        (
            file["disabled"].clone(),
            file["priority"].clone(),
            file["note"].clone()
        ),
        (json!(true), json!(3), json!("Work"))
    );
    let listed = &world.cores.accounts.list(&profile).await.unwrap()[0];
    assert_eq!(
        (listed.status, listed.name.as_str(), listed.priority),
        (AccountStatus::Paused, "Work", 3)
    );
    assert_eq!(
        world.fake.calls(&profile, "/auth-files/"),
        [
            (Method::PATCH, "/auth-files/status".to_owned()),
            (Method::PATCH, "/auth-files/fields".to_owned())
        ]
    );
    credentials
        .set_account(
            &profile,
            &account.id,
            AccountPatch {
                disabled: Some(false),
                priority: None,
                name: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        world.cores.accounts.list(&profile).await.unwrap()[0].status,
        AccountStatus::Ready
    );

    credentials
        .add_api_account(&profile, api_input("edit-key"))
        .await
        .unwrap();
    let api = world.cores.accounts.list(&profile).await.unwrap()[1]
        .id
        .clone();
    credentials
        .set_account(
            &profile,
            &api,
            AccountPatch {
                disabled: Some(true),
                priority: None,
                name: Some(String::new()),
            },
        )
        .await
        .unwrap();
    let listed = &world.cores.accounts.list(&profile).await.unwrap()[1];
    assert_eq!(
        (listed.status, listed.name.as_str()),
        (AccountStatus::Paused, "Company API")
    );

    let unknown = AccountId::parse("nope.json").unwrap();
    for refused in [
        credentials
            .set_account(
                &profile,
                &unknown,
                AccountPatch {
                    disabled: Some(true),
                    priority: None,
                    name: None,
                },
            )
            .await,
        credentials.remove_account(&profile, &unknown).await,
    ] {
        let refused = refused.unwrap_err();
        assert_eq!(
            (refused.message.as_str(), refused.status),
            ("Account not found in this profile.", 404)
        );
    }
    world.cores.pool().stop(&profile).await.unwrap();
    let stopped = credentials
        .set_account(
            &profile,
            &account.id,
            AccountPatch {
                disabled: Some(true),
                priority: None,
                name: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        (stopped.message.as_str(), stopped.status),
        ("Start this profile first.", 409)
    );

    // Removing a signed-in account starts its core first.
    credentials
        .remove_account(&profile, &account.id)
        .await
        .unwrap();
    credentials.remove_account(&profile, &api).await.unwrap();
    assert_eq!(world.cores.accounts.list(&profile).await.unwrap().len(), 0);
    assert!(
        !world
            .credential_file(&profile, account.id.as_str())
            .exists()
    );
    assert!(world.cores.pool().running(&profile));
}

#[tokio::test]
async fn imports_are_validated_and_deduplicated_within_a_profile_only() {
    let world = World::new("changes-imports").await;
    let profiles = [
        world.profile("Codex A").await,
        world.profile("Codex B").await,
    ];
    let credentials = &world.cores.credentials;
    for invalid in [
        json!({ "type": "nope", "access_token": "x" }),
        json!({ "type": "codex" }),
        json!({}),
    ] {
        let refused = credentials
            .import(&profiles[0], object(&invalid))
            .await
            .unwrap_err();
        assert_eq!(
            (refused.message.as_str(), refused.status),
            (
                "Choose a CLIProxyAPI OAuth JSON file. It must contain a supported type and token.",
                400
            )
        );
    }
    let raw = json!({ "type": "codex", "access_token": "synthetic-shared", "refresh_token": "synthetic-refresh-shared" });
    let mut ids = Vec::new();
    for profile in &profiles {
        ids.push(credentials.import(profile, object(&raw)).await.unwrap().id);
        let duplicate = credentials.import(profile, object(&raw)).await.unwrap_err();
        assert_eq!(duplicate.status, 409, "{duplicate:?}");
        let refreshed = json!({ "type": "codex", "access_token": "synthetic-new", "refresh_token": "synthetic-refresh-shared" });
        assert_eq!(
            credentials
                .import(profile, object(&refreshed))
                .await
                .unwrap_err()
                .status,
            409
        );
        assert_eq!(world.cores.accounts.list(profile).await.unwrap().len(), 1);
    }
    assert_ne!(ids[0], ids[1]);
    let gone = credentials
        .import_credential(
            &profiles[0],
            object(&raw),
            Some(AccountId::parse("gone.json").unwrap()),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (gone.message.as_str(), gone.status),
        ("The subscription to reconnect no longer exists.", 409)
    );
}

#[tokio::test]
async fn refresh_starts_a_kept_profile_first_and_checks_api_quota_even_when_it_cannot_start() {
    let world = World::new("changes-refresh").await;
    let profile = world.profile("Refresh").await;
    world
        .cores
        .credentials
        .add_api_account(&profile, api_input("refresh-key"))
        .await
        .unwrap();
    world.cores.pool().stop(&profile).await.unwrap();
    world.cores.refresh(&profile).await.unwrap();
    assert!(
        world.cores.pool().running(&profile),
        "a profile the app keeps running starts first"
    );
    world.cores.pool().stop(&profile).await.unwrap();
    // A config the core cannot be written for: the start fails, the API quota is still checked.
    let blocker = nsv_store::layout::core_config_path(world.data(), &profile);
    std::fs::remove_file(&blocker).unwrap();
    std::fs::create_dir(&blocker).unwrap();
    let failed = world.cores.refresh(&profile).await.unwrap_err();
    assert_eq!(failed.status, 500, "{failed:?}");
    let quota = world.cores.accounts.list(&profile).await.unwrap()[0]
        .quota
        .clone()
        .unwrap();
    assert_eq!(quota.status, nsv_domain::wire::QuotaStatus::Unavailable);
    std::fs::remove_dir(&blocker).unwrap();
}

fn quota_of(account: &nsv_domain::wire::Account) -> Value {
    serde_json::to_value(&account.quota).unwrap()
}

#[tokio::test]
async fn refresh_answers_a_failed_start_and_keeps_the_last_good_check() {
    let world = World::new("changes-refresh-exit").await;
    let profile = world.profile("Refresh fixture").await;
    world.fake.knobs(|knobs| {
        knobs.upstream = Arc::new(|_| {
            json!({ "status_code": 200, "body": { "rate_limit": { "primary_window": {
                "used_percent": 40, "limit_window_seconds": 18000,
            } } } })
        });
    });
    let raw = json!({ "type": "codex", "access_token": "synthetic-codex", "refresh_token": "synthetic-refresh" });
    world
        .cores
        .credentials
        .import(&profile, object(&raw))
        .await
        .unwrap();
    world.cores.refresh(&profile).await.unwrap();
    let quota = async || quota_of(&world.cores.accounts.list(&profile).await.unwrap()[0]);
    let checked = quota().await;
    assert_eq!(checked["status"], "available", "{checked}");

    world.cores.pool().stop(&profile).await.unwrap();
    world.fake.knobs(|knobs| knobs.exits_on_launch = true);
    let failed = world.cores.refresh(&profile).await.unwrap_err();
    assert_eq!(failed.status, 503, "{failed:?}");
    assert!(
        failed.message.starts_with("Proxy exited (1)."),
        "{}",
        failed.message
    );
    assert_eq!(
        quota().await,
        checked,
        "a failed start stores no failed check"
    );

    // Without a subscription the profile is not started.
    world.fake.knobs(|knobs| knobs.exits_on_launch = false);
    let empty = world.profile("Empty").await;
    let launches = world.fake.launches().len();
    world.cores.refresh(&empty).await.unwrap();
    assert_eq!(world.fake.launches().len(), launches);
    assert!(!world.cores.pool().alive(&empty));
}

/// Gives the profile an `OpenCode` Go key, whose quota is asked of the provider with the key and
/// no core; answers how many times the provider was asked.
async fn go_account(world: &World, profile: &ProfileId) -> impl Fn() -> usize {
    world.fetch.answer(|url| {
        Ok(if url.ends_with("/usage") {
            response(200, r#"{"usage":{"rolling":{"usagePercent":40}}}"#)
        } else {
            response(404, "{}")
        })
    });
    let go: ApiAccountInput = serde_json::from_value(json!({
        "name": "Go", "provider": "opencode-go", "baseUrl": "https://opencode.ai/zen/go/v1",
        "apiKey": "synthetic-go-key", "prefix": "",
        "models": [{ "id": "glm-5.3", "protocol": "openai" }],
    }))
    .unwrap();
    world
        .cores
        .credentials
        .add_api_account(profile, go)
        .await
        .unwrap();
    let fetch = Arc::clone(&world.fetch);
    move || {
        fetch
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.request.url.ends_with("/usage"))
            .count()
    }
}

#[tokio::test]
async fn a_refresh_that_shutdown_interrupts_asks_no_provider() {
    let world = World::new("changes-refresh-shutdown").await;
    let profile = world.profile("Interrupted").await;
    let go_checks = go_account(&world, &profile).await;
    let added = go_checks();
    world.cores.pool().stop(&profile).await.unwrap();
    // The core never answers, so the Refresh's start waits for it.
    world.fake.knobs(|knobs| knobs.silent = true);
    let launches = world.fake.launches().len();
    let refresh = tokio::spawn({
        let (cores, profile) = (Arc::clone(&world.cores), profile.clone());
        async move { cores.refresh(&profile).await }
    });
    while world.fake.launches().len() == launches {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    world.cores.shutdown().await.unwrap();
    let failed = refresh.await.unwrap().unwrap_err();
    assert_eq!(failed.status, 503, "{failed:?}");
    assert!(
        failed.message.contains("shutting down"),
        "{}",
        failed.message
    );
    assert_eq!(go_checks(), added, "no provider is asked");
    assert_eq!(*world.reports.lock().unwrap(), Vec::<String>::new());
}

#[tokio::test]
async fn refresh_checks_api_key_quota_while_the_core_is_held_or_cannot_start() {
    let world = World::new("changes-refresh-go").await;
    let profile = world.profile("Go fixture").await;
    let go_checks = go_account(&world, &profile).await;
    assert!(world.cores.pool().running(&profile));
    // Adding the account checked it once already.
    let added = go_checks();
    let held = world
        .cores
        .pool()
        .while_stopped(&profile, {
            let (cores, profile) = (Arc::clone(&world.cores), profile.clone());
            async move {
                let held = cores.refresh(&profile).await.unwrap_err();
                assert_eq!(held.status, 409);
                assert_eq!(
                    held.message,
                    "This profile is reconnecting a subscription. Try again in a moment."
                );
                Ok(())
            }
        })
        .await;
    held.unwrap();
    assert_eq!(go_checks(), added + 1, "checked while the core is held");

    world.cores.pool().stop(&profile).await.unwrap();
    world.fake.knobs(|knobs| knobs.exits_on_launch = true);
    let failed = world.cores.refresh(&profile).await.unwrap_err();
    assert_eq!(failed.status, 503, "{failed:?}");
    assert!(
        failed.message.starts_with("Proxy exited (1)."),
        "{}",
        failed.message
    );
    assert_eq!(
        go_checks(),
        added + 2,
        "checked although the core cannot start"
    );
    let accounts = world.cores.accounts.list(&profile).await.unwrap();
    assert_eq!(quota_of(&accounts[0])["status"], "available");
    assert_eq!(*world.reports.lock().unwrap(), Vec::<String>::new());
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the TypeScript test's redemption sequence, in order"
)]
async fn codex_resets_need_a_running_profile_and_retry_an_uncertain_redemption_with_the_same_id() {
    let world = World::new("changes-resets").await;
    let profile = world.profile("Reset fixture").await;
    let (resets, source, quotas) = (
        &world.cores.resets,
        world.cores.quotas.source(),
        world.cores.quotas.as_ref(),
    );
    let account = AccountId::parse("fixture.json").unwrap();
    let (message, status) = {
        let cases = cases("api");
        let error =
            case(&cases, "GET reset-credits unknown account")["output"]["body"]["error"].clone();
        (
            error["message"].as_str().unwrap().to_owned(),
            u16::try_from(error["code"].as_u64().unwrap()).unwrap(),
        )
    };
    let stopped = resets
        .credits(source, &profile, &account)
        .await
        .unwrap_err();
    assert_eq!((stopped.to_string(), stopped.status()), (message, status));

    let raw = json!({
        "type": "codex", "access_token": "synthetic-codex", "id_token": { "chatgpt_account_id": "fixture-identity" },
    });
    std::fs::create_dir_all(world.credential_file(&profile, "x").parent().unwrap()).unwrap();
    std::fs::write(
        world.credential_file(&profile, "fixture.json"),
        raw.to_string(),
    )
    .unwrap();
    world.cores.pool().start(&profile, false).await.unwrap();
    world
        .cores
        .accounts
        .sync(world.cores.pool(), &profile)
        .await
        .unwrap();

    let redemptions = Arc::new(Mutex::new(Vec::<String>::new()));
    let fail = Arc::new(AtomicBool::new(true));
    {
        let (redemptions, fail) = (Arc::clone(&redemptions), Arc::clone(&fail));
        world.fake.knobs(move |knobs| {
            knobs.upstream = Arc::new(move |call: &Value| {
                assert_eq!(call["auth_index"], "index-fixture.json");
                assert_eq!(call["header"]["Chatgpt-Account-Id"], "fixture-identity");
                assert_eq!(call["header"]["Authorization"], "Bearer $TOKEN$");
                let url = call["url"].as_str().unwrap();
                if url == "https://chatgpt.com/backend-api/wham/usage" {
                    return json!({ "status_code": 200, "body": { "rate_limit": { "primary_window": { "used_percent": 0, "limit_window_seconds": 18000 } } } });
                }
                if call["method"] == "GET" {
                    assert_eq!(url, "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits");
                    return json!({ "status_code": 200, "body": { "available_count": 1, "credits": [{
                        "id": "credit-one", "reset_type": "rate_limit", "status": "available",
                        "expires_at": "2026-12-01T00:00:00+00:00", "title": null,
                    }] } });
                }
                assert_eq!(url, "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume");
                let data: Value = serde_json::from_str(call["data"].as_str().unwrap()).unwrap();
                assert_eq!(data["credit_id"], "credit-one");
                redemptions.lock().unwrap().push(data["redeem_request_id"].as_str().unwrap().to_owned());
                if fail.load(Ordering::SeqCst) {
                    json!({ "status_code": 503, "body": "sensitive upstream error" })
                } else {
                    json!({ "status_code": 200, "body": { "code": "reset" } })
                }
            });
        });
    }
    let credits = resets.credits(source, &profile, &account).await.unwrap();
    assert_eq!(credits.available_count, 1);
    let uncertain = resets
        .consume(source, quotas, &profile, &account, "credit-one")
        .await
        .unwrap_err();
    assert_eq!(uncertain.status(), 502);
    assert!(!uncertain.to_string().contains("sensitive"));
    let kept = world
        .store
        .call(|db| db.reset_request_id("fixture-identity", "credit-one"))
        .await
        .unwrap();
    assert_eq!(
        redemptions.lock().unwrap().first(),
        Some(&kept),
        "the request id is stored durably"
    );
    fail.store(false, Ordering::SeqCst);
    let (entered, (release, released)) = (Arc::new(Notify::new()), watch::channel(false));
    world
        .fake
        .knobs(|knobs| knobs.upstream_gate = Some((Arc::clone(&entered), released)));
    let pending = tokio::spawn({
        let (cores, profile, account) =
            (Arc::clone(&world.cores), profile.clone(), account.clone());
        async move {
            cores
                .resets
                .consume(
                    cores.quotas.source(),
                    cores.quotas.as_ref(),
                    &profile,
                    &account,
                    "credit-one",
                )
                .await
        }
    });
    entered.notified().await;
    let busy = resets
        .consume(source, quotas, &profile, &account, "credit-one")
        .await
        .unwrap_err();
    assert_eq!(
        (busy.to_string().as_str(), busy.status()),
        ("A reset is already in progress for this subscription.", 409)
    );
    release.send_replace(true);
    let done = pending.await.unwrap().unwrap();
    assert_eq!(
        (done.code, done.quota_refreshed),
        (ResetResultCode::Reset, true)
    );
    let redemptions = redemptions.lock().unwrap();
    assert_eq!(
        redemptions.get(1),
        redemptions.first(),
        "the retry redeemed the same request"
    );
}
