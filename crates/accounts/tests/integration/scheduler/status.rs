//! tests/account-status.test.ts and the stale-sync case of tests/core-lifecycle.test.ts: an
//! account sync reads the core's list, the seat identity of changed credential files, and
//! stores only what its own core listed.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

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
async fn account_sync_rereads_a_claude_identity_only_when_its_file_changes_and_repairs_only_loose_modes()
 {
    let (world, profile) = started(json!({ "files": [
        { "name": "seat.json", "provider": "claude", "auth_index": "seat" },
    ] }))
    .await;
    let path = DataDir::new(&world.data)
        .credentials(&profile)
        .join("seat.json");
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let write = |organization: &str, modified: SystemTime| {
        fs::write(
            &path,
            json!({ "type": "claude", "organization_name": organization }).to_string(),
        )
        .unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    };
    let organization = async || {
        world
            .run(world.accounts.sync(world.pool(), &profile))
            .await
            .unwrap();
        metadata(&world, &profile).await[0]
            .organization_name
            .clone()
    };
    write("Org One", at);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(organization().await.as_deref(), Some("Org One"));
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Same size and time: the cached identity stands, so the file was not read again.
    write("Org Two", at);
    assert_eq!(organization().await.as_deref(), Some("Org One"));
    write("Org Two", at + Duration::from_secs(1));
    assert_eq!(organization().await.as_deref(), Some("Org Two"));
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
    assert_eq!(world.accounts.oauth_count(&profile), Some(0));
}
