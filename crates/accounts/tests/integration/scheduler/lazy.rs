//! tests/lazy-cores.test.ts: a profile without an OAuth account runs its core only while
//! something uses it. The gateway's part (key, routing, WebSocket upgrade) is the gateway
//! crate's; here a request is what the gateway does with the scheduler: count it in, wake the
//! profile, send it to the core's port, count it out.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::json;

use super::common::{World, short};

const IDLE: Duration = Duration::from_secs(3);

async fn lazy_world(oauth: bool) -> (World, nsv_domain::id::ProfileId) {
    let world = World::new(short(IDLE)).await;
    let profile = world.profile("Lazy fixture", true, true, oauth).await;
    (world, profile)
}

#[tokio::test(start_paused = true)]
async fn an_idle_daemons_only_pending_deadline_is_the_daily_history_cleanup() {
    let (world, profile) = lazy_world(false).await;
    world.scheduler.launch().await.unwrap();
    world.settle().await;
    assert!(world.scheduler.is_dormant(&profile));
    world.assert_only_cleanup_due("nothing runs");
    // Each day's cleanup schedules the next one, and nothing else.
    world
        .pass_by(Duration::from_hours(72), Duration::from_hours(1))
        .await;
    world.assert_only_cleanup_due("nothing runs");
    assert_eq!(world.launches(&profile), 0);
    assert!(world.reports().is_empty(), "{:?}", world.reports());
}

#[tokio::test(start_paused = true)]
async fn a_profile_without_an_oauth_account_starts_its_core_on_its_first_request() {
    let (world, profile) = lazy_world(false).await;
    world.scheduler.launch().await.unwrap();
    world.settle().await;
    assert!(!world.pool().state().alive(&profile), "not at launch");
    assert_eq!(world.chat(&profile, "fixture-model").await, 200);
    assert!(world.pool().state().running(&profile));
    assert_eq!(world.launches(&profile), 1);
}

#[tokio::test(start_paused = true)]
async fn requests_that_arrive_together_at_a_dormant_profile_all_wait_for_its_one_core() {
    let (world, profile) = lazy_world(false).await;
    world.scheduler.launch().await.unwrap();
    let wakes = world
        .run(async {
            tokio::join!(
                world.scheduler.wake(&profile),
                world.scheduler.wake(&profile),
                world.scheduler.wake(&profile),
                world.scheduler.wake(&profile),
            )
        })
        .await;
    assert!(
        wakes.0.is_ok() && wakes.1.is_ok() && wakes.2.is_ok() && wakes.3.is_ok(),
        "{wakes:?}"
    );
    assert!(world.pool().state().usable(&profile));
    assert_eq!(world.launches(&profile), 1, "one core for all of them");
}

#[tokio::test(start_paused = true)]
async fn only_a_uis_state_polls_start_dormant_cores() {
    let (world, profile) = lazy_world(false).await;
    world.scheduler.launch().await.unwrap();
    // The CLI's state read asks nothing of the scheduler.
    world.pass(Duration::from_secs(1)).await;
    assert!(!world.pool().state().alive(&profile), "the CLI's read");
    world.run(world.scheduler.watch()).await;
    world.settle().await;
    assert!(world.pool().state().running(&profile), "a UI's poll");
}

#[tokio::test(start_paused = true)]
async fn the_model_catalog_starts_a_dormant_core() {
    let (world, profile) = lazy_world(false).await;
    world.scheduler.launch().await.unwrap();
    world.run(world.scheduler.wake(&profile)).await.unwrap();
    let models = world.run(world.pool().models(&profile)).await.unwrap();
    assert_eq!(models.len(), 1, "an entry without a string id is skipped");
    assert_eq!(models[0].id, "fixture-model");
    assert_eq!(models[0].owned_by.as_deref(), Some("fixture"));
}

#[tokio::test(start_paused = true)]
async fn an_idle_core_stops_after_its_last_request_without_losing_its_usage_and_the_next_request_starts_it_again()
 {
    let (world, profile) = lazy_world(false).await;
    world.scheduler.launch().await.unwrap();
    // Reads of the queue find it empty until shortly before the idle stop, so only the stop's
    // own drains can collect what the core still holds.
    world
        .fake
        .knobs(|knobs| knobs.hidden_queue.insert(profile.clone()));
    for _ in 0..3 {
        assert_eq!(world.chat(&profile, "fixture-model").await, 200);
    }
    world.pass(Duration::from_secs(2)).await;
    assert_eq!(world.usage(&profile).await.len(), 0);
    assert!(world.pool().state().running(&profile), "not idle yet");
    world
        .fake
        .knobs(|knobs| knobs.hidden_queue.remove(&profile));
    world
        .until("the idle stop", IDLE * 2, |world| {
            !world.pool().state().alive(&profile)
        })
        .await;
    assert_eq!(
        world.usage(&profile).await.len(),
        3,
        "every request was collected before the core stopped"
    );
    assert!(world.scheduler.is_dormant(&profile));
    world.pass(IDLE * 3).await;
    assert!(
        !world.pool().state().alive(&profile),
        "the restart policy does not start it again"
    );
    assert_eq!(world.chat(&profile, "fixture-model").await, 200);
    assert_eq!(world.launches(&profile), 2);
    world.until_usage(&profile, 4, IDLE).await;
}

#[tokio::test(start_paused = true)]
async fn a_request_that_arrives_while_an_idle_stop_drains_its_core_is_served_by_the_next_core() {
    let (world, profile) = lazy_world(false).await;
    world.scheduler.launch().await.unwrap();
    // The stop's own final drain, made once its core is marked stopping, waits for the
    // request below.
    let pool = world.pool().clone();
    let (release, holding) = world
        .fake
        .hold("/usage-queue", move |profile| !pool.state().usable(profile));
    assert_eq!(world.chat(&profile, "fixture-model").await, 200);
    let first = world.fake.latest(&profile).unwrap();
    world
        .until("the stop's final drain", IDLE * 2, |_| {
            holding.load(Ordering::SeqCst) > 0
        })
        .await;
    let late = world.chat(&profile, "fixture-model");
    let release_later = async {
        world.pass(Duration::from_millis(100)).await;
        release.send_replace(true);
    };
    let (status, ()) = tokio::join!(late, release_later);
    assert_eq!(status, 200);
    let next = world.fake.latest(&profile).unwrap();
    assert_ne!(next.index, first.index, "a new core served it");
    assert!(!first.alive());
    assert!(world.pool().state().running(&profile));
    world.until_usage(&profile, 2, IDLE).await;
}

#[tokio::test(start_paused = true)]
async fn an_idle_stop_that_cannot_read_the_usage_queue_keeps_the_core_running() {
    let (world, profile) = lazy_world(false).await;
    world.scheduler.launch().await.unwrap();
    assert_eq!(world.chat(&profile, "fixture-model").await, 200);
    world.pass(Duration::from_secs(1)).await;
    assert_eq!(world.usage(&profile).await.len(), 1);
    world
        .fake
        .knobs(|knobs| knobs.failing_queue.insert(profile.clone()));
    world
        .until("a reported failed read", IDLE * 2, |world| {
            world
                .reports()
                .iter()
                .any(|report| report.starts_with("Lazy fixture: Usage before an idle stop: "))
        })
        .await;
    assert!(world.pool().state().running(&profile));
    assert!(!world.scheduler.is_dormant(&profile));
}

#[tokio::test(start_paused = true)]
async fn an_idle_core_whose_key_was_rate_limited_keeps_running_until_a_request_on_it_succeeds() {
    let (world, profile) = lazy_world(false).await;
    world.scheduler.launch().await.unwrap();
    world
        .fake
        .knobs(|knobs| knobs.limited = Some("fixture-model".to_owned()));
    assert_eq!(world.chat(&profile, "fixture-model").await, 429);
    world.pass(IDLE * 4).await;
    let failed = world.usage(&profile).await;
    assert_eq!(failed.len(), 1);
    assert!(failed[0].failed && failed[0].status_code == 429);
    assert!(
        world.pool().state().running(&profile),
        "a new core would forget the cooldown and send the next request to the exhausted key"
    );
    world.fake.knobs(|knobs| knobs.limited = None);
    assert_eq!(world.chat(&profile, "fixture-model").await, 200);
    world
        .until("the idle stop", IDLE * 3, |world| {
            !world.pool().state().alive(&profile)
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_success_on_another_model_of_the_same_key_does_not_end_a_models_cooldown() {
    let (world, profile) = lazy_world(false).await;
    world.scheduler.launch().await.unwrap();
    world
        .fake
        .knobs(|knobs| knobs.limited = Some("fixture-model".to_owned()));
    assert_eq!(world.chat(&profile, "fixture-model").await, 429);
    assert_eq!(world.chat(&profile, "other-model").await, 200);
    world.pass(IDLE * 4).await;
    assert_eq!(world.usage(&profile).await.len(), 2);
    assert!(
        world.pool().state().running(&profile),
        "fixture-model is still cooling down in this core"
    );
}

#[tokio::test(start_paused = true)]
async fn a_cooldown_ends_with_the_cores_own_period() {
    let (world, profile) = lazy_world(false).await;
    world.scheduler.launch().await.unwrap();
    world
        .fake
        .knobs(|knobs| knobs.limited = Some("fixture-model".to_owned()));
    assert_eq!(world.chat(&profile, "fixture-model").await, 429);
    // The pinned core cools a 429 down for at most 30 minutes.
    world
        .pass_by(Duration::from_mins(29), Duration::from_secs(1))
        .await;
    assert!(world.pool().state().running(&profile));
    world
        .pass_by(Duration::from_mins(2), Duration::from_secs(1))
        .await;
    assert!(
        !world.pool().state().alive(&profile),
        "the core no longer holds it"
    );
}

#[tokio::test(start_paused = true)]
async fn a_profile_with_an_oauth_account_starts_at_launch_and_never_stops_for_being_idle() {
    let (world, profile) = lazy_world(true).await;
    // Its core lists the Claude subscription the earlier run synced.
    world.fake.knobs(|knobs| {
        knobs.listings.insert(
            profile.clone(),
            json!({ "files": [{ "name": "claude.json", "provider": "claude", "auth_index": "1" }] }),
        );
    });
    world.run(world.scheduler.launch()).await.unwrap();
    assert!(world.pool().state().running(&profile), "at launch");
    assert_eq!(world.chat(&profile, "fixture-model").await, 200);
    world.pass(IDLE * 4).await;
    assert!(
        world.pool().state().running(&profile),
        "still running after four idle periods"
    );
    assert_eq!(world.launches(&profile), 1);
}

#[tokio::test(start_paused = true)]
async fn an_account_sync_that_removes_the_last_subscription_lets_the_idle_core_stop() {
    let world = World::new(short(IDLE)).await;
    let profile = world.profile("Lazy fixture", true, false, true).await;
    world.fake.knobs(|knobs| {
        knobs.listings.insert(
            profile.clone(),
            json!({ "files": [{ "name": "claude.json", "provider": "claude", "auth_index": "1" }] }),
        );
    });
    world.run(world.scheduler.launch()).await.unwrap();
    assert_eq!(world.chat(&profile, "fixture-model").await, 200);
    world.pass(IDLE).await;
    world.assert_only_cleanup_due("a subscription keeps the core running");
    // The subscription is gone from the core; a sync no route asked for stores that.
    world.fake.knobs(|knobs| {
        knobs
            .listings
            .insert(profile.clone(), json!({ "files": [] }))
    });
    let synced = world.run(world.accounts.sync(world.pool(), &profile)).await;
    assert_eq!(synced.unwrap(), Some(0));
    world
        .until("the idle stop", IDLE * 2, |world| {
            !world.pool().state().alive(&profile)
        })
        .await;
    assert!(world.scheduler.is_dormant(&profile));
    assert_eq!(world.launches(&profile), 1);
}

#[tokio::test(start_paused = true)]
async fn an_open_ui_a_pending_sign_in_or_an_open_request_keeps_an_idle_core() {
    let (world, profile) = lazy_world(false).await;
    world.scheduler.launch().await.unwrap();
    assert_eq!(world.chat(&profile, "fixture-model").await, 200);
    *world.host.signing_in.lock().unwrap() = Some(profile.clone());
    world.pass(IDLE * 2).await;
    assert!(world.pool().state().running(&profile), "a pending sign-in");
    *world.host.signing_in.lock().unwrap() = None;
    world.scheduler.activity(&profile, true);
    world.pass(IDLE * 2).await;
    assert!(world.pool().state().running(&profile), "an open request");
    world.scheduler.activity(&profile, false);
    for _ in 0..8 {
        world.run(world.scheduler.watch()).await;
        world.pass(IDLE / 2).await;
    }
    assert!(world.pool().state().running(&profile), "an open UI");
    // A UI's read keeps polling on for 12 s, and the idle period counts from its end.
    world.pass(Duration::from_secs(10) + IDLE).await;
    assert!(
        world.pool().state().running(&profile),
        "the UI's last read still counts"
    );
    world
        .until("the idle stop", Duration::from_secs(5), |world| {
            !world.pool().state().alive(&profile)
        })
        .await;
}
