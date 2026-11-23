//! tests/core-lifecycle.test.ts: the pool's starts, stops, swaps, settings and shutdown, and
//! the scheduler's usage drains and account polling. Sign-in and credential-change ordering
//! belong to those modules and stay out.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use nsv_accounts::scheduler::{TIMING, Timing};
use nsv_core::processes::KILL_GRACE;
use nsv_domain::id::ProfileId;
use nsv_domain::registry::Strategy;
use nsv_domain::wire::Runtime;
use nsv_net::Method;
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

use crate::common::{World, short};
use crate::fake::{Hold, Script};

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

/// Holds requests to `path` while `when` says so; answers the release and how many were held.
fn hold(
    world: &World,
    path: &'static str,
    when: impl Fn(&ProfileId) -> bool + Send + Sync + 'static,
) -> (watch::Sender<bool>, Arc<AtomicUsize>) {
    let (release, released) = watch::channel(false);
    let holding = Arc::new(AtomicUsize::new(0));
    world.fake.knobs(|knobs| {
        knobs.hold = Some(Hold {
            path,
            when: Box::new(when),
            released,
            holding: Arc::clone(&holding),
        });
    });
    (release, holding)
}

#[tokio::test(start_paused = true)]
async fn a_core_that_never_becomes_ready_and_ignores_sigterm_is_killed_before_start_fails() {
    let world = World::with_ready(timing(), Duration::from_secs(1)).await;
    let profile = world.profile("Stubborn", false, true, false).await;
    world.fake.script(
        &profile,
        [Script {
            answers: None,
            ignores_term: true,
            ..Script::default()
        }],
    );
    let began = Instant::now();
    let error = world
        .run(world.pool().start(&profile, false))
        .await
        .unwrap_err();
    assert_eq!(error.status, 503);
    assert!(
        error.message.starts_with("Proxy did not become ready. "),
        "{error:?}"
    );
    assert!(began.elapsed() >= Duration::from_secs(1) + KILL_GRACE);
    let core = world.fake.latest(&profile).unwrap();
    assert!(!core.alive());
    assert_eq!(core.terms.load(Ordering::SeqCst), 1);
    let state = world.pool().state();
    assert_eq!(
        state.runtime(&profile),
        (Runtime::Error, Some(error.message.as_str()))
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_completes_after_the_core_could_not_spawn() {
    let world = World::new(timing()).await;
    let profile = world.profile("Unlaunchable", false, true, false).await;
    world.fake.script(
        &profile,
        [Script {
            launch_error: true,
            ..Script::default()
        }],
    );
    let error = world
        .run(world.pool().start(&profile, false))
        .await
        .unwrap_err();
    assert_eq!(error.status, 503);
    assert!(
        error.message.starts_with("Could not launch proxy: "),
        "{error:?}"
    );
    assert!(!world.pool().alive(&profile));
    world.run(world.scheduler.shutdown()).await.unwrap();
    assert!(!world.pool().state().starting(&profile));
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_slow_exiting_profiles_together_in_about_the_time_of_the_slowest() {
    let world = World::new(timing()).await;
    let mut profiles = Vec::new();
    for name in ["One", "Two", "Three"] {
        let profile = world.profile(name, false, true, false).await;
        // As on a slow runner: each core exits a second after it is told to.
        world.fake.script(
            &profile,
            [Script {
                stop_takes: Duration::from_secs(1),
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
    world.settle().await;
    let began = Instant::now();
    world.run(world.scheduler.shutdown()).await.unwrap();
    let took = began.elapsed();
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(2),
        "shutdown took {took:?}; one at a time takes over 3 s"
    );
    for profile in &profiles {
        assert!(
            world.fake.calls(profile, "/usage-queue").last() >= Some(&began),
            "every profile's usage was drained"
        );
        assert!(!world.pool().alive(profile));
        assert_eq!(world.pool().state().runtime(profile).0, Runtime::Stopped);
    }
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
            world.fake.calls(profile, "/usage-queue").last() >= Some(&began),
            "every profile's usage was drained"
        );
    }
    assert!(!world.pool().alive(&profiles[2]));
}

#[tokio::test(start_paused = true)]
async fn queued_starts_reject_shutdown_after_a_pending_accounting_drain() {
    let (world, profiles) = launched(timing(), &["Queued"]).await;
    let profile = &profiles[0];
    let pool = world.pool().clone();
    let (release, holding) = hold(&world, "/usage-queue", move |profile| !pool.usable(profile));
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
    assert!(!world.pool().alive(profile));
    assert!(!world.pool().state().starting(profile));
}

#[tokio::test(start_paused = true)]
async fn shutdown_lets_the_schedulers_running_drain_pass_finish_before_it_stops_the_cores() {
    let (world, profiles) = launched(timing(), &["Pass"]).await;
    let profile = profiles[0].clone();
    let pool = world.pool().clone();
    let (release, holding) = hold(&world, "/usage-queue", move |profile| pool.usable(profile));
    world.scheduler.activity(&profile, false);
    world
        .until("the drain pass", Duration::from_secs(1), |_| {
            holding.load(Ordering::SeqCst) > 0
        })
        .await;
    let shutdown = world.scheduler.shutdown();
    let test = async {
        world.pass(Duration::from_secs(1)).await;
        assert!(world.pool().alive(&profile), "the pass is still under way");
        release.send_replace(true);
    };
    let (shut, ()) = world.run(async { tokio::join!(shutdown, test) }).await;
    shut.unwrap();
    assert!(!world.pool().alive(&profile));
}

#[tokio::test(start_paused = true)]
async fn a_core_held_stopped_for_a_credential_swap_cannot_start_until_the_swap_ends_then_restarts()
{
    let world = World::new(timing()).await;
    let profile = running(&world, "Swap").await;
    let before = world.fake.latest(&profile).unwrap();
    let (told, observed) = oneshot::channel();
    let pool = world.pool().clone();
    let id = profile.clone();
    let swapped = world
        .run(world.pool().while_stopped(&profile, async move {
            let alive = pool.alive(&id);
            let refused = pool.start(&id, false).await;
            let _ = told.send((alive, refused));
            Ok(())
        }))
        .await;
    swapped.unwrap();
    let (alive, refused) = observed.await.unwrap();
    assert!(!alive, "stopped before the swap");
    let refused = refused.unwrap_err();
    assert_eq!(refused.status, 409);
    assert!(refused.message.contains("reconnecting"), "{refused:?}");
    assert!(
        world.pool().running(&profile),
        "running again after the swap"
    );
    assert!(!before.alive());
    assert_eq!(world.launches(&profile), 2);
}

#[tokio::test(start_paused = true)]
async fn a_finished_request_drains_usage_once_and_once_more_after_the_activity_window() {
    let (world, profiles) = launched(timing(), &["Drain"]).await;
    let profile = &profiles[0];
    world.pass(Duration::from_secs(1)).await;
    let before = world.fake.calls(profile, "/usage-queue").len();
    world.scheduler.activity(profile, false);
    world.pass(Duration::from_secs(3)).await;
    let drains = world.fake.calls(profile, "/usage-queue");
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
    let settled = world.fake.calls(profile, "/usage-queue").len();
    world.pass(Duration::from_secs(60)).await;
    assert_eq!(
        world.fake.calls(profile, "/usage-queue").len(),
        settled,
        "no usage wakeups while idle"
    );
    world.scheduler.activity(profile, true);
    world
        .pass_by(Duration::from_secs(65), Duration::from_millis(500))
        .await;
    let open = world.fake.calls(profile, "/usage-queue");
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
    let before_stop = world.fake.calls(profile, "/usage-queue").len();
    world.run(world.pool().stop(profile)).await.unwrap();
    assert_eq!(
        world.fake.calls(profile, "/usage-queue").len(),
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
    let before = world.fake.calls(profile, "/usage-queue").len();
    world.scheduler.activity(profile, false);
    world
        .until("seven failed reads", Duration::from_secs(20), |world| {
            world.fake.calls(profile, "/usage-queue").len() >= before + 7
        })
        .await;
    let attempts = world.fake.calls(profile, "/usage-queue");
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
    let before = world.fake.calls(profile, "/usage-queue").len();
    world.scheduler.activity(profile, false);
    world
        .until("two failed reads", Duration::from_secs(5), |world| {
            world.fake.calls(profile, "/usage-queue").len() >= before + 2
        })
        .await;
    let again = gaps(&world.fake.calls(profile, "/usage-queue")[before..]);
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
    let syncs = || world.fake.calls(profile, "/auth-files").len();
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

// Settings tests use profiles without API-key accounts: the fake core reports only its
// routing, so a config whose other live entries are empty is the one it can show applied.

/// The routing the profile's core reports.
async fn strategy(world: &World, profile: &ProfileId) -> String {
    let config = world
        .run(world.pool().management(
            profile,
            &Method::GET,
            "/config",
            None,
            Duration::from_secs(15),
        ))
        .await
        .unwrap();
    config["routing"]["strategy"].as_str().unwrap().to_owned()
}

async fn save_strategy(world: &World, profile: &ProfileId, strategy: Strategy) {
    let id = profile.clone();
    world
        .store
        .call(move |db| {
            let mut record = db.profile(&id)?;
            record.strategy = strategy;
            db.save_profile(&record)
        })
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_settings_change_saved_while_the_core_starts_is_applied_once_the_core_is_ready() {
    let world = World::new(timing()).await;
    let profile = world.profile("Settings", false, false, false).await;
    let first = Arc::new(AtomicBool::new(true));
    // The core's first readiness answer waits until the change is saved.
    let (release, holding) = hold(&world, "/config", move |_| {
        first.swap(false, Ordering::SeqCst)
    });
    let test = async {
        let start = world.pool().start(&profile, false);
        let change = async {
            world
                .until("the held readiness", Duration::from_secs(1), |_| {
                    holding.load(Ordering::SeqCst) > 0
                })
                .await;
            assert!(world.pool().state().starting(&profile));
            // As PATCH /profiles/:profileId does: save, then configure.
            save_strategy(&world, &profile, Strategy::RoundRobin).await;
            let configure = world.pool().configure(&profile);
            release.send_replace(true);
            configure.await
        };
        tokio::join!(start, change)
    };
    let (started, configured) = world.run(test).await;
    started.unwrap();
    configured.unwrap();
    assert_eq!(strategy(&world, &profile).await, "round-robin");
}

#[tokio::test(start_paused = true)]
async fn a_core_that_misses_the_first_write_to_its_config_applies_the_settings_once_configure_writes_it_again()
 {
    let world = World::new(timing()).await;
    let profile = world.profile("Missed write", false, false, false).await;
    world.fake.script(
        &profile,
        [Script {
            misses_writes: 1,
            ..Script::default()
        }],
    );
    world
        .run(world.pool().start(&profile, false))
        .await
        .unwrap();
    save_strategy(&world, &profile, Strategy::RoundRobin).await;
    world.run(world.pool().configure(&profile)).await.unwrap();
    assert_eq!(strategy(&world, &profile).await, "round-robin");
}

#[tokio::test(start_paused = true)]
async fn settings_a_core_never_applies_are_reported() {
    let world = World::new(timing()).await;
    let profile = world.profile("Deaf", false, false, false).await;
    world.fake.script(
        &profile,
        [Script {
            misses_writes: usize::MAX,
            ..Script::default()
        }],
    );
    world
        .run(world.pool().start(&profile, false))
        .await
        .unwrap();
    save_strategy(&world, &profile, Strategy::RoundRobin).await;
    let error = world
        .run(world.pool().configure(&profile))
        .await
        .unwrap_err();
    assert_eq!(error.status, 503);
    assert_eq!(
        error.message,
        "The proxy has not applied the settings yet. Stop and start this profile to reload them."
    );
}

#[tokio::test(start_paused = true)]
async fn a_settings_change_that_waited_for_a_start_leaves_alone_a_core_that_a_swap_then_stops() {
    let world = World::new(timing()).await;
    let profile = world.profile("Swapped settings", false, false, false).await;
    let first = Arc::new(AtomicBool::new(true));
    let (release, holding) = hold(&world, "/config", move |_| {
        first.swap(false, Ordering::SeqCst)
    });
    let test = async {
        let start = world.pool().start(&profile, false);
        let rest = async {
            world
                .until("the held readiness", Duration::from_secs(1), |_| {
                    holding.load(Ordering::SeqCst) > 0
                })
                .await;
            save_strategy(&world, &profile, Strategy::RoundRobin).await;
            let configure = world.pool().configure(&profile);
            let swap = world.pool().while_stopped(&profile, async { Ok(()) });
            release.send_replace(true);
            tokio::join!(configure, swap)
        };
        tokio::join!(start, rest)
    };
    let (started, (configured, swapped)) = world.run(test).await;
    started.unwrap();
    configured.unwrap();
    swapped.unwrap();
    let cores = world.fake.launches(&profile);
    assert_eq!(cores.len(), 2);
    assert!(!cores[0].alive());
    // The successor wrote its own config from the saved settings.
    assert_eq!(strategy(&world, &profile).await, "round-robin");
}
