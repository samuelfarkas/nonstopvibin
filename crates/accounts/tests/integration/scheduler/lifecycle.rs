//! tests/core-lifecycle.test.ts: shutdown with the scheduler, its usage drains and account
//! polling. The pool's own starts, stops, swaps and settings are tested in nsv-core. Sign-in and
//! credential-change ordering belong to those modules and stay out.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use nsv_accounts::restart::RestartTiming;
use nsv_accounts::scheduler::{TIMING, Timing};
use nsv_core::processes::KILL_GRACE;
use nsv_domain::id::ProfileId;
use nsv_testkit::fake::Script;
use tokio::time::Instant;

use super::common::{World, short};

/// A profile whose core has started, with the scheduler running.
async fn running(world: &World, name: &str) -> ProfileId {
    let profile = world.profile(name, false, true, false).await;
    world
        .run(world.pool().start(&profile, false))
        .await
        .unwrap();
    profile
}

async fn launched(timing: Timing, names: &[&str]) -> (World, Vec<ProfileId>) {
    let world = World::new(timing).await;
    let mut profiles = Vec::new();
    for name in names {
        profiles.push(running(&world, name).await);
    }
    world.scheduler.launch().await.unwrap();
    world.settle().await;
    (world, profiles)
}

fn timing() -> Timing {
    short(TIMING.idle)
}

fn gaps(times: &[Instant]) -> Vec<u128> {
    times
        .windows(2)
        .map(|pair| pair[1].duration_since(pair[0]).as_millis())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_failing_stop_does_not_keep_shutdown_from_draining_and_stopping_the_other_profiles() {
    let world = World::new(timing()).await;
    let mut profiles = Vec::new();
    for (name, unkillable) in [("Broken one", true), ("Broken two", true), ("Fine", false)] {
        let profile = world.profile(name, false, true, false).await;
        world.fake.script(
            &profile,
            [Script {
                unkillable,
                ..Script::default()
            }],
        );
        world
            .run(world.pool().start(&profile, false))
            .await
            .unwrap();
        profiles.push(profile);
    }
    world.scheduler.launch().await.unwrap();
    let began = Instant::now();
    let error = world.run(world.scheduler.shutdown()).await.unwrap_err();
    assert!(
        error
            .message
            .starts_with("Profiles did not stop cleanly: Could not stop the proxy: "),
        "{error:?}"
    );
    for profile in &profiles {
        assert!(
            world.fake.called_at(profile, "/usage-queue").last() >= Some(&began),
            "every profile's usage was drained"
        );
    }
    assert!(!world.pool().state().alive(&profiles[2]));
}

#[tokio::test(start_paused = true)]
async fn queued_starts_reject_shutdown_after_a_pending_accounting_drain() {
    let (world, profiles) = launched(timing(), &["Queued"]).await;
    let profile = &profiles[0];
    let pool = world.pool().clone();
    let (release, holding) = world
        .fake
        .hold("/usage-queue", move |profile| !pool.state().usable(profile));
    let test = async {
        let stopping = world.pool().stop(profile);
        let starting = async {
            world
                .until("the stop's drain", Duration::from_secs(1), |_| {
                    holding.load(Ordering::SeqCst) > 0
                })
                .await;
            world.pool().start(profile, false).await
        };
        let shutting = async {
            world
                .until("the queued start", Duration::from_secs(1), |_| {
                    holding.load(Ordering::SeqCst) > 0
                })
                .await;
            world.pass(Duration::from_millis(100)).await;
            world.scheduler.begin_shutdown().await;
            release.send_replace(true);
        };
        tokio::join!(stopping, starting, shutting)
    };
    let (stopped, started, ()) = world.run(test).await;
    stopped.unwrap();
    let refused = started.unwrap_err();
    assert_eq!(
        (refused.status, refused.message.as_str()),
        (503, "nonstopvibin is shutting down.")
    );
    assert_eq!(world.launches(profile), 1);
    assert!(!world.pool().state().alive(profile));
    assert!(!world.pool().state().starting(profile));
}

#[tokio::test(start_paused = true)]
async fn shutdown_lets_the_schedulers_running_drain_pass_finish_before_it_stops_the_cores() {
    let (world, profiles) = launched(timing(), &["Pass"]).await;
    let profile = profiles[0].clone();
    let pool = world.pool().clone();
    let (release, holding) = world
        .fake
        .hold("/usage-queue", move |profile| pool.state().usable(profile));
    world.scheduler.activity(&profile, false);
    world
        .until("the drain pass", Duration::from_secs(1), |_| {
            holding.load(Ordering::SeqCst) > 0
        })
        .await;
    let shutdown = world.scheduler.shutdown();
    let test = async {
        world.pass(Duration::from_secs(1)).await;
        assert!(
            world.pool().state().alive(&profile),
            "the pass is still under way"
        );
        release.send_replace(true);
    };
    let (shut, ()) = world.run(async { tokio::join!(shutdown, test) }).await;
    shut.unwrap();
    assert!(!world.pool().state().alive(&profile));
}

#[tokio::test(start_paused = true)]
async fn a_finished_request_drains_usage_once_and_once_more_after_the_activity_window() {
    let (world, profiles) = launched(timing(), &["Drain"]).await;
    let profile = &profiles[0];
    world.pass(Duration::from_secs(1)).await;
    let before = world.fake.called_at(profile, "/usage-queue").len();
    world.scheduler.activity(profile, false);
    world.pass(Duration::from_secs(3)).await;
    let drains = world.fake.called_at(profile, "/usage-queue");
    assert_eq!(
        drains.len() - before,
        2,
        "not a read of the empty queue every 200 ms through the window"
    );
}

#[tokio::test(start_paused = true)]
async fn idle_profiles_stop_draining_usage_after_one_trailing_drain_unless_a_request_is_in_flight()
{
    let (world, profiles) = launched(timing(), &["Idle"]).await;
    let profile = &profiles[0];
    world.scheduler.activity(profile, false);
    world.pass(Duration::from_secs(2)).await;
    let settled = world.fake.called_at(profile, "/usage-queue").len();
    world.pass(Duration::from_secs(60)).await;
    assert_eq!(
        world.fake.called_at(profile, "/usage-queue").len(),
        settled,
        "no usage wakeups while idle"
    );
    world.scheduler.activity(profile, true);
    world
        .pass_by(Duration::from_secs(65), Duration::from_millis(500))
        .await;
    let open = world.fake.called_at(profile, "/usage-queue");
    assert!(
        open.len() >= settled + 3,
        "an open stream or WebSocket keeps its periodic drain: {}",
        open.len() - settled
    );
    let last = gaps(&open[settled..]);
    assert!(
        last.iter()
            .skip(2)
            .all(|gap| (30_000..30_600).contains(gap)),
        "{last:?}"
    );
    world.scheduler.activity(profile, false);
    world.pass(Duration::from_secs(2)).await;
    let before_stop = world.fake.called_at(profile, "/usage-queue").len();
    world.run(world.pool().stop(profile)).await.unwrap();
    assert_eq!(
        world.fake.called_at(profile, "/usage-queue").len(),
        before_stop + 1,
        "stop still drains the queue"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failing_usage_drain_retries_once_promptly_then_backs_off_to_a_bound_and_resets_on_success()
 {
    let timing = Timing {
        usage_backoff: Duration::from_millis(1_600),
        ..timing()
    };
    let (world, profiles) = launched(timing, &["Backoff"]).await;
    let profile = &profiles[0];
    world.pass(Duration::from_secs(1)).await;
    world
        .fake
        .knobs(|knobs| knobs.failing_queue.insert(profile.clone()));
    let before = world.fake.called_at(profile, "/usage-queue").len();
    world.scheduler.activity(profile, false);
    world
        .until("seven failed reads", Duration::from_secs(20), |world| {
            world.fake.called_at(profile, "/usage-queue").len() >= before + 7
        })
        .await;
    let attempts = world.fake.called_at(profile, "/usage-queue");
    // Retry n waits min(coalesce × 2^(n − 1), backoff); the first retry waits for nothing but
    // the coalescing wait, and the clock moves in 50 ms steps.
    let waits: Vec<u128> = gaps(&attempts[before..]);
    for (gap, wait) in waits.iter().zip([200, 400, 800, 1_600, 1_600, 1_600]) {
        assert!((wait..wait + 100).contains(gap), "{waits:?}");
    }
    let reports = world.reports();
    assert!(
        reports.iter().all(|report| report.starts_with(
            "Backoff: Proxy management returned HTTP 500. Check the profile is running"
        )),
        "{reports:?}"
    );
    world
        .fake
        .knobs(|knobs| knobs.failing_queue.remove(profile));
    world.scheduler.activity(profile, false);
    world.pass(Duration::from_secs(3)).await;
    // A success resets the backoff: the next failure is retried promptly again.
    world
        .fake
        .knobs(|knobs| knobs.failing_queue.insert(profile.clone()));
    let before = world.fake.called_at(profile, "/usage-queue").len();
    world.scheduler.activity(profile, false);
    world
        .until("two failed reads", Duration::from_secs(5), |world| {
            world.fake.called_at(profile, "/usage-queue").len() >= before + 2
        })
        .await;
    let again = gaps(&world.fake.called_at(profile, "/usage-queue")[before..]);
    assert!((200..300).contains(&again[0]), "{again:?}");
}

#[tokio::test(start_paused = true)]
async fn account_status_is_polled_only_while_a_ui_reads_the_state() {
    let timing = Timing {
        accounts: Duration::from_millis(500),
        watch: Duration::from_secs(2),
        ..timing()
    };
    let (world, profiles) = launched(timing, &["Watch"]).await;
    let profile = &profiles[0];
    let syncs = || world.fake.called_at(profile, "/auth-files").len();
    let before = syncs();
    world.pass(Duration::from_secs(4)).await;
    assert_eq!(syncs(), before, "no UI reads: no account polling");
    world.scheduler.watch().await;
    assert_eq!(
        syncs(),
        before + 1,
        "the first read after a pause syncs before it answers"
    );
    world.pass(Duration::from_millis(1_500)).await;
    assert!(
        syncs() >= before + 3,
        "a UI that keeps reading keeps polling on ({} syncs)",
        syncs() - before
    );
    world.pass(Duration::from_secs(3)).await;
    let settled = syncs();
    world.pass(Duration::from_secs(3)).await;
    assert_eq!(syncs(), settled, "polling stops once the UI stops reading");
}

#[tokio::test(start_paused = true)]
async fn a_readiness_retry_of_one_core_keeps_its_activity_window() {
    let world = World::new(timing()).await;
    let profile = world.profile("Retry", false, true, false).await;
    world.scheduler.launch().await.unwrap();
    let failed = Arc::new(AtomicBool::new(false));
    {
        let (scheduler, failed, id) = (
            world.scheduler.clone(),
            Arc::clone(&failed),
            profile.clone(),
        );
        // The first account sync fails right after a request marked activity, so readiness retries.
        world.fake.knobs(|knobs| {
            knobs.intercept = Some(Arc::new(move |_, path| {
                if path != "/auth-files" || failed.swap(true, Ordering::SeqCst) {
                    return None;
                }
                scheduler.activity(&id, false);
                Some(Box::pin(async {
                    Some(serde_json::json!({ "files": "unreadable" }))
                }))
            }));
        });
    }
    let before = world.fake.called_at(&profile, "/usage-queue").len();
    world
        .run(world.pool().start(&profile, false))
        .await
        .unwrap();
    assert!(failed.load(Ordering::SeqCst));
    assert!(
        world.fake.called_at(&profile, "/auth-files").len() >= 2,
        "readiness retried"
    );
    world.pass(Duration::from_secs(3)).await;
    let drains = world.fake.called_at(&profile, "/usage-queue").len() - before;
    world.fake.knobs(|knobs| knobs.intercept = None);
    assert_eq!(
        drains, 2,
        "a prompt drain and one after the activity window"
    );
}

#[tokio::test(start_paused = true)]
async fn the_first_ui_read_after_a_pause_shows_the_cooldown_and_refreshes_stale_quotas() {
    let timing = Timing {
        restart: RestartTiming {
            coalesce: 20,
            ..timing().restart
        },
        usage_retry: Duration::from_millis(20),
        accounts: Duration::from_millis(50),
        watch: Duration::from_millis(200),
        quotas: Duration::from_millis(100),
        ..timing()
    };
    let world = World::new(timing).await;
    let profile = world.profile("Status", false, false, false).await;
    // The core's own listing for one signed-in Codex account.
    let list = |cooling: bool| {
        serde_json::json!({ "files": [{
            "name": "codex-watch.json", "type": "codex", "provider": "codex",
            "auth_index": "a1", "status": "active", "unavailable": cooling,
        }] })
    };
    let listing = |cooling| {
        world
            .fake
            .knobs(|knobs| knobs.listings.insert(profile.clone(), list(cooling)))
    };
    let status = async || {
        let accounts = world.accounts.list(&profile).await.unwrap();
        let account = accounts
            .iter()
            .find(|account| account.id.as_str() == "codex-watch.json")
            .unwrap();
        serde_json::to_value(account).unwrap()["status"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let checks = || world.host.quota_checks.lock().unwrap().len();
    listing(false);
    world
        .run(world.pool().start(&profile, false))
        .await
        .unwrap();
    world.scheduler.launch().await.unwrap();
    world.settle().await;
    assert_eq!(status().await, "Ready");
    listing(true);
    let before = checks();
    world.pass(Duration::from_millis(300)).await;
    assert_eq!(status().await, "Ready", "nobody watched: not polled");
    assert_eq!(checks(), before, "nobody watched: no quota check");

    world.scheduler.watch().await;
    assert_eq!(
        status().await,
        "Cooling down",
        "the read that resumes polling already shows the cooldown"
    );
    world.pass(Duration::from_secs(1)).await;
    assert!(
        checks() > before,
        "a stale quota refreshes once a UI watches"
    );

    listing(false);
    for _ in 0..10 {
        world.scheduler.watch().await;
        world.pass(Duration::from_millis(100)).await;
    }
    assert_eq!(
        status().await,
        "Ready",
        "polling continues while the UI reads"
    );
}

/// Queues `count` gateway requests on the profile's running core.
fn queue_requests(world: &World, profile: &ProfileId, count: usize) {
    let port = world.pool().port(profile).unwrap();
    for _ in 0..count {
        assert_eq!(world.fake.request(port, "fixture-model"), 200);
    }
}

#[tokio::test(start_paused = true)]
async fn concurrent_drains_of_one_profile_share_one_queue_read() {
    let (world, profiles) = launched(timing(), &["Shared drain"]).await;
    let profile = &profiles[0];
    world.pass(Duration::from_secs(3)).await;
    queue_requests(&world, profile, 3);
    let before = world.fake.called_at(profile, "/usage-queue").len();
    let (release, holding) = world.fake.hold("/usage-queue", |_| true);
    let test = async {
        // A finished request's drain, held at the core.
        world.scheduler.activity(profile, false);
        world
            .until("the held drain", Duration::from_secs(1), |_| {
                holding.load(Ordering::SeqCst) > 0
            })
            .await;
        // A stop's final drain while the first is still reading.
        let stop = world.pool().stop(profile);
        let rest = async {
            world.settle().await;
            release.send_replace(true);
        };
        tokio::join!(stop, rest).0
    };
    world.run(test).await.unwrap();
    assert_eq!(
        world.fake.called_at(profile, "/usage-queue").len() - before,
        1,
        "the overlapping drains read the queue once"
    );
    assert_eq!(
        world.usage(profile).await.len(),
        3,
        "every queued row is stored"
    );
}

#[tokio::test(start_paused = true)]
async fn slow_account_maintenance_does_not_hold_up_usage_collection() {
    let timing = Timing {
        accounts: Duration::from_millis(500),
        watch: Duration::from_secs(10),
        ..timing()
    };
    let (world, profiles) = launched(timing, &["Slow maintenance"]).await;
    let profile = &profiles[0];
    world.pass(Duration::from_secs(3)).await;
    // The read that resumes polling syncs at once; the maintenance pass after it is held.
    let syncs = Arc::new(AtomicUsize::new(0));
    let (release, holding) = world.fake.hold("/auth-files", move |_| {
        syncs.fetch_add(1, Ordering::SeqCst) > 0
    });
    world.scheduler.watch().await;
    world
        .until("the held maintenance", Duration::from_secs(5), |_| {
            holding.load(Ordering::SeqCst) > 0
        })
        .await;
    let before = world.fake.called_at(profile, "/usage-queue").len();
    queue_requests(&world, profile, 1);
    world.scheduler.activity(profile, false);
    world.pass(Duration::from_secs(1)).await;
    assert!(
        world.fake.called_at(profile, "/usage-queue").len() > before,
        "a finished request is drained while maintenance waits"
    );
    assert_eq!(world.usage(profile).await.len(), 1);
    release.send_replace(true);
}

#[tokio::test(start_paused = true)]
async fn a_stop_completes_when_the_core_exits_during_its_final_drain() {
    let (world, profiles) = launched(timing(), &["Exit in drain"]).await;
    let profile = &profiles[0];
    world.pass(Duration::from_secs(3)).await;
    {
        let (fake, id) = (world.fake.clone(), profile.clone());
        // The next queue read is the stop's: the core exits as it arrives.
        world.fake.knobs(|knobs| {
            knobs.intercept = Some(Arc::new(move |_, path| {
                if !path.starts_with("/usage-queue") {
                    return None;
                }
                fake.kill(&id);
                Some(Box::pin(async { None }))
            }));
        });
    }
    let started = Instant::now();
    world.run(world.pool().stop(profile)).await.unwrap();
    world.fake.knobs(|knobs| knobs.intercept = None);
    assert!(
        started.elapsed() <= KILL_GRACE,
        "stopped after {:?}",
        started.elapsed()
    );
    assert!(!world.pool().state().alive(profile));
}

#[tokio::test(start_paused = true)]
async fn concurrent_stops_drain_every_queued_page_and_refuse_the_gateway() {
    let (world, profiles) = launched(timing(), &["Drain pages"]).await;
    let profile = &profiles[0];
    world.pass(Duration::from_secs(3)).await;
    // Two full pages and a short one.
    queue_requests(&world, profile, 2 * 500 + 3);
    let before = world.fake.called_at(profile, "/usage-queue").len();
    let refused = Arc::new(AtomicUsize::new(0));
    {
        let (pool, id, refused) = (world.pool().clone(), profile.clone(), Arc::clone(&refused));
        world.fake.knobs(|knobs| {
            knobs.intercept = Some(Arc::new(move |_, path| {
                if path.starts_with("/usage-queue") && pool.port(&id).is_err() {
                    refused.fetch_add(1, Ordering::SeqCst);
                }
                None
            }));
        });
    }
    let (first, second) = world
        .run(async { tokio::join!(world.pool().stop(profile), world.pool().stop(profile)) })
        .await;
    world.fake.knobs(|knobs| knobs.intercept = None);
    first.unwrap();
    second.unwrap();
    let reads = world.fake.called_at(profile, "/usage-queue").len() - before;
    assert_eq!(reads, 3, "only the stop reads a stopping core's queue");
    assert_eq!(
        refused.load(Ordering::SeqCst),
        3,
        "the gateway is refused during the stop"
    );
    assert_eq!(world.usage(profile).await.len(), 2 * 500 + 3);
    assert!(!world.pool().state().alive(profile));
}
