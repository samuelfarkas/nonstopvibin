//! tests/account-status.test.ts and the stale-sync case of tests/core-lifecycle.test.ts: an
//! account sync reads the core's list, the seat identity of changed credential files, and
//! stores only what its own core listed.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nsv_domain::id::ProfileId;
use nsv_domain::wire::{Account, AccountStatus};
use nsv_files::DataDir;
use serde_json::{Value, json};

use super::common::{World, short};

async fn metadata(world: &World, profile: &ProfileId) -> Vec<Account> {
    let profile = profile.clone();
    world
        .store
        .call(move |db| db.account_metadata(&profile))
        .await
        .unwrap()
}

async fn started(listing: Value) -> (World, ProfileId) {
    let world = World::new(short(Duration::from_secs(600))).await;
    let profile = world.profile("Fixture", false, false, false).await;
    world
        .fake
        .knobs(|knobs| knobs.listings.insert(profile.clone(), listing));
    world
        .run(world.pool().start(&profile, false))
        .await
        .unwrap();
    (world, profile)
}

#[tokio::test(start_paused = true)]
async fn dead_refresh_tokens_show_as_reconnect_needed_instead_of_a_cooldown() {
    let file = |name: &str, fields: Value| {
        let mut file = json!({ "name": name, "provider": "claude", "account_type": "oauth", "auth_index": name });
        file.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        file
    };
    let (world, profile) = started(json!({ "files": [
        file("invalid-grant.json", json!({ "status": "error", "status_message": "invalid grant (retrying)", "unavailable": true })),
        file("unauthorized.json", json!({ "status": "error", "status_message": "unauthorized", "unavailable": true })),
        file("expired.json", json!({ "status": "error", "status_message": "token expired", "unavailable": true })),
        file("cooldown.json", json!({ "unavailable": true })),
        file("ready.json", json!({})),
    ] }))
    .await;
    let synced = world
        .run(world.accounts.sync(world.pool(), &profile))
        .await
        .unwrap();
    assert_eq!(synced, Some(5));
    let statuses: Vec<(String, AccountStatus)> = metadata(&world, &profile)
        .await
        .into_iter()
        .map(|account| (account.id.as_str().to_owned(), account.status))
        .collect();
    assert_eq!(
        statuses,
        [
            (
                "invalid-grant.json".to_owned(),
                AccountStatus::ReconnectNeeded
            ),
            (
                "unauthorized.json".to_owned(),
                AccountStatus::ReconnectNeeded
            ),
            ("expired.json".to_owned(), AccountStatus::NeedsAttention),
            ("cooldown.json".to_owned(), AccountStatus::CoolingDown),
            ("ready.json".to_owned(), AccountStatus::Ready),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_sync_notes_the_credentials_the_core_keeps_from_requests_for_quota() {
    let file = |name: &str, cooldown: Value| json!({ "name": name, "provider": "claude", "auth_index": name, "cooldowns": [cooldown] });
    let (world, profile) = started(json!({ "files": [
        file("weekly.json", json!({ "scope": "credential", "reason": "credential_quota", "retry_at": "2026-12-01T00:00:00Z", "remaining_seconds": 3600 })),
        file("model.json", json!({ "scope": "model", "model_key": "claude-fixture", "reason": "quota", "retry_at": "2026-12-01T00:00:00Z", "remaining_seconds": 60 })),
        file("failing.json", json!({ "scope": "credential", "reason": "unknown", "http_status": 503, "retry_at": "2026-12-01T00:00:00Z", "remaining_seconds": 60 })),
        { "name": "ready.json", "provider": "claude", "auth_index": "ready", "cooldowns": [] },
    ] }))
    .await;
    world
        .run(world.accounts.sync(world.pool(), &profile))
        .await
        .unwrap();
    let cooling: Vec<&str> = ["weekly.json", "model.json", "failing.json", "ready.json"]
        .into_iter()
        .filter(|account| world.accounts.quota_cooldown(&profile, account))
        .collect();
    assert_eq!(cooling, ["weekly.json", "model.json"]);
}

#[tokio::test(start_paused = true)]
async fn account_sync_makes_credential_files_owner_only() {
    let (world, profile) = started(json!({ "files": [
        { "name": "codex.json", "provider": "codex", "auth_index": "codex" },
    ] }))
    .await;
    let path = DataDir::new(&world.data)
        .credentials(&profile)
        .join("codex.json");
    fs::write(&path, json!({ "type": "codex" }).to_string()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    world
        .run(world.accounts.sync(world.pool(), &profile))
        .await
        .unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test(start_paused = true)]
async fn an_account_sync_that_outlives_its_core_does_not_overwrite_the_restarted_cores_accounts() {
    let (world, profile) = started(json!({ "files": [] })).await;
    // The held listing is the old core's: one subscription the new core no longer has.
    world.fake.knobs(|knobs| {
        knobs.listings.insert(
            profile.clone(),
            json!({ "files": [{ "name": "old.json", "provider": "codex" }] }),
        );
    });
    let first = Arc::new(AtomicBool::new(true));
    let (release, holding) = world
        .fake
        .hold("/auth-files", move |_| first.swap(false, Ordering::SeqCst));
    let stale = world.accounts.sync(world.pool(), &profile);
    let restart = async {
        world
            .until("the held listing", Duration::from_secs(1), |_| {
                holding.load(Ordering::SeqCst) > 0
            })
            .await;
        world.fake.knobs(|knobs| {
            knobs
                .listings
                .insert(profile.clone(), json!({ "files": [] }));
        });
        world.pool().stop(&profile).await.unwrap();
        world.pool().start(&profile, false).await.unwrap();
        release.send_replace(true);
    };
    let (stale, ()) = world.run(async { tokio::join!(stale, restart) }).await;
    assert_eq!(
        stale.unwrap(),
        None,
        "the listing of a replaced core is dropped"
    );
    assert_eq!(metadata(&world, &profile).await, []);
    assert_eq!(world.store.facts().borrow()[0].oauth, 0);
}
