//! tests/core-restart.test.ts: the scheduler starts again a profile the app keeps running (it
//! has a subscription) whose core is not running. Refresh is the quota routes' and stays out.

use std::sync::atomic::Ordering;
use std::time::Duration;

use nsv_accounts::scheduler::{TIMING, Timing};
use nsv_core::restart::RestartTiming;
use nsv_domain::id::ProfileId;
use serde_json::json;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::common::World;
use crate::fake::Script;

fn timing(coalesce: i64, backoff: i64, limit: u32) -> Timing {
    Timing {
        restart: RestartTiming {
            coalesce,
            restart_backoff: backoff,
            restart_limit: limit,
        },
        ..TIMING
    }
}

/// Attempt n + 1 comes min(coalesce × 2^(n − 1), backoff) after attempt n ended: 1 s, 2 s, 4 s,
/// then the 8 s bound.
const WAITS: [u64; 8] = [1, 2, 4, 8, 8, 8, 8, 8];

fn backoff() -> Timing {
    timing(1_000, 8_000, 20)
}

/// Each gap between consecutive instants, in whole milliseconds.
fn gaps(times: &[Instant]) -> Vec<u64> {
    times
        .windows(2)
        .map(|pair| u64::try_from(pair[1].duration_since(pair[0]).as_millis()).unwrap())
        .collect()
}

fn assert_backoff(gaps: &[u64]) {
    assert!(gaps.len() >= WAITS.len(), "{gaps:?}");
    for (index, wait) in WAITS.iter().enumerate() {
        let gap = gaps[index];
        // No sooner than its wait; a wakeup coalesced with an earlier one may come up to one
        // coalescing period later, and the clock moves in 50 ms steps.
        assert!(
            (wait * 1000..=wait * 1000 + 1_050).contains(&gap),
            "attempt {} came {gap} ms after the one before, expected {wait} s: {gaps:?}",
            index + 2
        );
    }
}

async fn subscribed(world: &World, name: &str) -> ProfileId {
    world.profile(name, false, true, false).await
}

/// The core lists one Claude subscription.
fn claude_listing(world: &World, profile: &ProfileId) {
    world.fake.knobs(|knobs| {
        knobs.listings.insert(
            profile.clone(),
            json!({ "files": [{ "name": "claude.json", "provider": "claude", "auth_index": "1" }] }),
        );
    });
}

#[tokio::test(start_paused = true)]
async fn a_core_that_exits_unexpectedly_runs_again_after_the_next_pass_with_its_quota_refreshed() {
    let world = World::new(timing(200, 300_000, 10)).await;
    let profile = world.profile("Restart fixture", false, false, false).await;
    claude_listing(&world, &profile);
    world
        .run(world.pool().start(&profile, false))
        .await
        .unwrap();
    world.scheduler.launch().await.unwrap();
    world.settle().await;
    assert!(
        world.host.quota_checks.lock().unwrap().is_empty(),
        "a start alone checks no quota"
    );
    world.fake.kill(&profile);
    world
        .until("the restarted core", Duration::from_secs(5), |world| {
            world.launches(&profile) == 2 && world.pool().usable(&profile)
        })
        .await;
    world.scheduler.settled().await;
    assert_eq!(
        *world.host.quota_stored.lock().unwrap(),
        std::slice::from_ref(&profile)
    );
    assert!(
        world.reports().is_empty(),
        "a one-off exit adds no error entry"
    );
}

#[tokio::test(start_paused = true)]
async fn a_core_that_keeps_failing_to_start_is_retried_with_backoff_to_a_bound_and_reported_once() {
    let world = World::new(backoff()).await;
    // A first subscription whose core never started: the profile never ran.
    let profile = subscribed(&world, "Failing fixture").await;
    let exits = Script {
        answers: None,
        exits_after: Some(Duration::ZERO),
        ..Script::default()
    };
    world.fake.script(&profile, vec![exits; 12]);
    world.scheduler.launch().await.unwrap();
    world
        .until("ten launches", Duration::from_secs(60), |world| {
            world.launches(&profile) == WAITS.len() + 2
        })
        .await;
    let times: Vec<Instant> = world
        .fake
        .launches(&profile)
        .iter()
        .map(|core| core.launched)
        .collect();
    assert_backoff(&gaps(&times));
    let reports = world.reports();
    assert_eq!(reports.len(), 1, "one error entry: {reports:?}");
    assert!(
        reports[0].starts_with("Failing fixture: Proxy exited (1). "),
        "{reports:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_core_that_exits_right_after_each_start_is_restarted_with_the_same_backoff_and_reported_once()
 {
    let world = World::new(backoff()).await;
    let profile = subscribed(&world, "Crash fixture").await;
    let crashes = Script {
        exits_after: Some(Duration::from_millis(10)),
        ..Script::default()
    };
    world.fake.script(&profile, vec![crashes; 12]);
    // The start needs no clock. Moving it could end the core's 10 ms while the start still
    // syncs its accounts off the runtime, and the start would then rightly fail.
    world.pool().start(&profile, false).await.unwrap();
    world.scheduler.launch().await.unwrap();
    world
        .until("ten launches", Duration::from_secs(60), |world| {
            world.launches(&profile) == WAITS.len() + 2
        })
        .await;
    // Each core lives 10 ms, so the wait from one launch to the next is the backoff plus that.
    let times: Vec<Instant> = world
        .fake
        .launches(&profile)
        .iter()
        .map(|core| core.launched)
        .collect();
    assert_backoff(&gaps(&times[1..]));
    let reports = world.reports();
    assert_eq!(reports.len(), 1, "one error entry: {reports:?}");
    assert!(
        reports[0].starts_with("Crash fixture: Proxy exited (1). "),
        "{reports:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn only_a_profile_with_a_subscription_is_started_again_whether_or_not_it_ran_before() {
    let world = World::new(timing(200, 300_000, 10)).await;
    let kept = subscribed(&world, "Kept").await;
    // Ran before, but nothing in it needs a core.
    let bare = world.profile("Bare", false, false, false).await;
    // Never started: one with no subscription, as a new profile, and one with a subscription.
    let fresh = world.profile("Fresh", false, false, false).await;
    let unstarted = subscribed(&world, "Unstarted").await;
    world.run(world.pool().start(&kept, false)).await.unwrap();
    world.run(world.pool().start(&bare, false)).await.unwrap();
    world.scheduler.launch().await.unwrap();
    world.fake.kill(&bare);
    world.fake.kill(&kept);
    world
        .until(
            "the kept profiles running",
            Duration::from_secs(5),
            |world| {
                world.launches(&kept) == 2
                    && world.pool().running(&kept)
                    && world.pool().running(&unstarted)
            },
        )
        .await;
    world.pass(Duration::from_secs(5)).await;
    assert_eq!(world.launches(&bare), 1);
    assert!(!world.pool().alive(&bare));
    assert_eq!(world.launches(&fresh), 0);
}

#[tokio::test(start_paused = true)]
async fn a_profile_held_for_a_credential_swap_and_every_profile_once_shutdown_began_is_left_stopped()
 {
    let world = World::new(timing(100, 300_000, 10)).await;
    let held = subscribed(&world, "Held").await;
    let other = subscribed(&world, "Other").await;
    world.run(world.pool().start(&held, false)).await.unwrap();
    world.run(world.pool().start(&other, false)).await.unwrap();
    world.scheduler.launch().await.unwrap();
    let (release, released) = watch::channel(false);
    let swap = world.pool().while_stopped(&held, async move {
        let mut released = released;
        let _ = released.wait_for(|released| *released).await;
        Ok(())
    });
    let test = async {
        world
            .until("the held core stopped", Duration::from_secs(5), |world| {
                !world.pool().alive(&held)
            })
            .await;
        // A pass runs while the held profile is stopped and due a restart.
        world.fake.kill(&other);
        world
            .until(
                "the other profile restarted",
                Duration::from_secs(5),
                |world| world.launches(&other) == 2 && world.pool().running(&other),
            )
            .await;
        assert!(!world.pool().alive(&held), "held stays stopped");
        release.send_replace(true);
    };
    let (swapped, ()) = tokio::join!(world.run(swap), test);
    swapped.unwrap();
    assert!(world.pool().running(&held), "the swap itself starts it");
    assert_eq!(world.launches(&held), 2);
    assert!(world.reports().is_empty(), "{:?}", world.reports());

    world.scheduler.begin_shutdown().await;
    world.fake.kill(&other);
    // Twenty coalesce periods: any restart would have begun by now.
    world.pass(Duration::from_secs(2)).await;
    assert_eq!(world.launches(&other), 2);
    world.run(world.scheduler.shutdown()).await.unwrap();
    assert!(!world.pool().alive(&held));
    assert!(!world.pool().alive(&other));
}

#[tokio::test(start_paused = true)]
async fn shutdown_abandons_an_automatic_start_still_waiting_for_its_core_to_answer() {
    let world = World::new(timing(100, 300_000, 10)).await;
    let profile = subscribed(&world, "Unready fixture").await;
    // The core never answers: its start would wait out the 15 s readiness window.
    world.fake.script(
        &profile,
        [Script {
            answers: None,
            ..Script::default()
        }],
    );
    world.scheduler.launch().await.unwrap();
    world
        .until("the automatic start", Duration::from_secs(2), |world| {
            world.launches(&profile) == 1
        })
        .await;
    let core = world.fake.latest(&profile).unwrap();
    let began = Instant::now();
    world.run(world.scheduler.shutdown()).await.unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(3),
        "{:?}",
        began.elapsed()
    );
    assert!(!core.alive());
    assert!(!world.pool().state().starting(&profile));
}

#[tokio::test(start_paused = true)]
async fn a_restart_blocked_by_a_credential_swap_arms_no_wakeup_of_its_own_and_begins_when_the_swap_ends()
 {
    let world = World::new(timing(100, 300_000, 10)).await;
    let profile = subscribed(&world, "Swap fixture").await;
    let (release, released) = watch::channel(false);
    let swap = world.pool().while_stopped(&profile, async move {
        let mut released = released;
        let _ = released.wait_for(|released| *released).await;
        Ok(())
    });
    let test = async {
        world.settle().await;
        // Its start is due, but nothing may start it until the swap ends.
        world.scheduler.launch().await.unwrap();
        world.settle().await;
        world.assert_only_cleanup_due("no core runs");
        world.pass(Duration::from_secs(2)).await;
        assert_eq!(world.launches(&profile), 0);
        release.send_replace(true);
    };
    let (swapped, ()) = tokio::join!(world.run(swap), test);
    swapped.unwrap();
    world
        .until("the restart", Duration::from_secs(2), |world| {
            world.pool().running(&profile)
        })
        .await;
    assert_eq!(world.launches(&profile), 1);
}

#[tokio::test(start_paused = true)]
async fn a_core_that_answers_but_never_finishes_its_readiness_keeps_its_backoff_and_is_reported_once()
 {
    let world = World::with_ready(backoff(), Duration::from_millis(300)).await;
    let profile = subscribed(&world, "Unready fixture").await;
    // The core answers, so it counts as running, but the readiness account sync always fails.
    world
        .fake
        .knobs(|knobs| knobs.failing_listing.insert(profile.clone()));
    world.scheduler.launch().await.unwrap();
    world
        .until("ten launches", Duration::from_secs(60), |world| {
            world.launches(&profile) == WAITS.len() + 2
        })
        .await;
    let times: Vec<Instant> = world
        .fake
        .launches(&profile)
        .iter()
        .map(|core| core.launched)
        .collect();
    // Each attempt waits out the 300 ms readiness window before it fails.
    let waits: Vec<u64> = gaps(&times).iter().map(|gap| gap - 300).collect();
    assert_backoff(&waits);
    let reports = world.reports();
    assert_eq!(reports.len(), 1, "one error entry: {reports:?}");
    assert!(
        reports[0].starts_with(
            "Unready fixture: Proxy did not become ready. Proxy management returned HTTP 500."
        ),
        "{reports:?}"
    );
}

/// A profile with a subscription, never started, that the scheduler starts with a limit of
/// four; `scripts` say how its cores behave.
async fn giving_up(name: &str, scripts: Vec<Script>) -> (World, ProfileId) {
    let world = World::new(timing(1_000, 8_000, 4)).await;
    let profile = subscribed(&world, name).await;
    world.fake.script(&profile, scripts);
    world.scheduler.launch().await.unwrap();
    (world, profile)
}

const GAVE_UP: &str =
    "Stopped restarting after 4 failed starts in a row. Use Restart to try again.";

fn failing() -> Script {
    Script {
        answers: None,
        exits_after: Some(Duration::ZERO),
        ..Script::default()
    }
}

fn crashing() -> Script {
    Script {
        exits_after: Some(Duration::from_millis(10)),
        ..Script::default()
    }
}

async fn assert_gave_up(world: &World, profile: &ProfileId) {
    world
        .until("the give-up", Duration::from_secs(60), |world| {
            world.scheduler.gave_up(profile).is_some()
        })
        .await;
    world.settle().await;
    assert_eq!(world.scheduler.gave_up(profile).as_deref(), Some(GAVE_UP));
    world.assert_only_cleanup_due("no restart is due");
    let (runtime, _) = world.pool().state().runtime(profile);
    assert_eq!(runtime, nsv_domain::wire::Runtime::Error);
    let reports = world.reports();
    assert_eq!(
        reports
            .iter()
            .filter(|report| report.contains("Stopped restarting"))
            .count(),
        1,
        "{reports:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_core_that_cannot_start_is_started_the_limits_number_of_times_then_no_more() {
    let (world, profile) = giving_up("Failing", vec![failing(); 8]).await;
    assert_gave_up(&world, &profile).await;
    world
        .pass_by(Duration::from_secs(600), Duration::from_secs(1))
        .await;
    assert_eq!(world.launches(&profile), 4);
}

#[tokio::test(start_paused = true)]
async fn a_core_that_exits_as_soon_as_it_runs_gives_up_the_same_way() {
    let (world, profile) = giving_up("Crashing", vec![crashing(); 8]).await;
    assert_gave_up(&world, &profile).await;
    world
        .pass_by(Duration::from_secs(600), Duration::from_secs(1))
        .await;
    assert_eq!(world.launches(&profile), 4);
}

#[tokio::test(start_paused = true)]
async fn a_start_that_succeeds_after_the_scheduler_gave_up_gives_the_profile_its_full_attempts_again()
 {
    let (world, profile) = giving_up("Recovering", vec![failing(); 4]).await;
    assert_gave_up(&world, &profile).await;
    world
        .run(world.pool().start(&profile, false))
        .await
        .unwrap();
    world.settle().await;
    assert_eq!(world.scheduler.gave_up(&profile), None);
    // Four more crashes in a row before it gives up again.
    world.fake.script(&profile, vec![crashing(); 4]);
    world.fake.kill(&profile);
    world
        .until("the second give-up", Duration::from_secs(60), |world| {
            world.scheduler.gave_up(&profile).is_some()
        })
        .await;
    assert_eq!(world.launches(&profile), 4 + 1 + 4);
}

#[tokio::test(start_paused = true)]
async fn a_start_that_fails_after_the_scheduler_gave_up_leaves_it_given_up() {
    let (world, profile) = giving_up("Still failing", vec![failing(); 8]).await;
    assert_gave_up(&world, &profile).await;
    let reports = world.reports();
    let started = world.run(world.pool().start(&profile, false)).await;
    assert_eq!(started.unwrap_err().status, 503);
    world
        .pass_by(Duration::from_secs(60), Duration::from_secs(1))
        .await;
    assert_eq!(world.scheduler.gave_up(&profile).as_deref(), Some(GAVE_UP));
    assert_eq!(world.launches(&profile), 5, "only the user's start ran");
    assert_eq!(world.reports(), reports);
}

#[tokio::test(start_paused = true)]
async fn the_limits_last_core_up_past_the_stability_bound_with_no_tick_in_between_is_restarted_when_it_exits()
 {
    let (world, profile) = giving_up("Steady", vec![crashing(); 3]).await;
    world
        .until("the last allowed core", Duration::from_secs(60), |world| {
            world.launches(&profile) == 4 && world.pool().usable(&profile)
        })
        .await;
    // Time itself is what this waits for: the core must outlast the bound.
    world.pass(Duration::from_secs(16)).await;
    let next = world.scheduler.wakes_at().unwrap();
    assert!(
        next > Instant::now() + Duration::from_secs(60),
        "no tick looks at the core before its idle stop"
    );
    world.fake.kill(&profile);
    world
        .until("the restart", Duration::from_secs(5), |world| {
            world.launches(&profile) == 5 && world.pool().usable(&profile)
        })
        .await;
    assert!(world.scheduler.gave_up(&profile).is_none());
    assert!(
        !world
            .reports()
            .iter()
            .any(|report| report.contains("Stopped restarting")),
        "{:?}",
        world.reports()
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_abandons_a_restarted_profiles_quota_check_and_stores_nothing_for_it() {
    let world = World::new(timing(100, 300_000, 10)).await;
    let profile = subscribed(&world, "Quota fixture").await;
    let (_open, gate) = watch::channel(false);
    *world.host.quota_gate.lock().unwrap() = Some(gate);
    world.scheduler.launch().await.unwrap();
    world
        .until("the quota check", Duration::from_secs(2), |world| {
            !world.host.quota_checks.lock().unwrap().is_empty()
        })
        .await;
    world.run(world.scheduler.shutdown()).await.unwrap();
    assert!(world.host.quota_stored.lock().unwrap().is_empty());
    assert!(world.reports().is_empty(), "{:?}", world.reports());
    assert!(!world.pool().alive(&profile));
}

#[tokio::test(start_paused = true)]
async fn a_quota_answer_that_comes_as_shutdown_begins_is_not_stored() {
    // Each round's restart hears of shutdown in the wakeup that brings its quota answer. If
    // either could win that tie, all sixteen rounds would pick shutdown only once in 65,536 runs.
    // One profile a round: the fake's cores do not hold their ports, so concurrent starts could
    // be given the same one.
    for _ in 0..16 {
        let world = World::new(timing(100, 300_000, 10)).await;
        subscribed(&world, "Quota fixture").await;
        world.host.quota_at_shutdown.store(true, Ordering::SeqCst);
        world.scheduler.launch().await.unwrap();
        // The restart waits out the coalescing period. Its start and account sync need no
        // clock, and moving it on while they run could only time them out.
        world.pass(Duration::from_millis(100)).await;
        for _ in 0..1_000 {
            if !world.host.quota_checks.lock().unwrap().is_empty() {
                break;
            }
            world.settle().await;
        }
        assert_eq!(world.host.quota_checks.lock().unwrap().len(), 1);
        world.run(world.scheduler.shutdown()).await.unwrap();
        assert_eq!(*world.host.quota_stored.lock().unwrap(), []);
        assert!(world.reports().is_empty(), "{:?}", world.reports());
    }
}

#[tokio::test(start_paused = true)]
async fn a_failed_quota_check_after_an_automatic_start_is_reported() {
    let world = World::new(timing(100, 300_000, 10)).await;
    subscribed(&world, "Quota fixture").await;
    *world.host.quota_error.lock().unwrap() = Some("Claude returned HTTP 401.".to_owned());
    world.scheduler.launch().await.unwrap();
    world
        .until("the report", Duration::from_secs(2), |world| {
            !world.reports().is_empty()
        })
        .await;
    assert_eq!(
        world.reports(),
        ["Quota fixture: Claude returned HTTP 401."]
    );
}

#[tokio::test(start_paused = true)]
async fn an_automatic_start_waiting_for_its_core_does_not_hold_up_another_profiles_maintenance() {
    let world = World::new(timing(100, 300_000, 10)).await;
    let healthy = world.profile("Healthy", false, true, false).await;
    world
        .run(world.pool().start(&healthy, false))
        .await
        .unwrap();
    let unready = subscribed(&world, "Unready").await;
    // Its core never answers, so its automatic start waits out the readiness window.
    world.fake.script(
        &unready,
        [Script {
            answers: None,
            ..Script::default()
        }],
    );
    world.scheduler.launch().await.unwrap();
    world
        .until("the automatic start", Duration::from_secs(2), |world| {
            world.launches(&unready) == 1
        })
        .await;
    let before = world.fake.calls(&healthy, "/auth-files").len();
    let watching = tokio::spawn({
        let scheduler = world.scheduler.clone();
        async move { scheduler.watch().await }
    });
    world
        .until(
            "the healthy profile's sync",
            Duration::from_secs(2),
            |world| world.fake.calls(&healthy, "/auth-files").len() > before,
        )
        .await;
    assert!(
        world.pool().state().starting(&unready),
        "the unready start is still pending"
    );
    world.run(world.scheduler.shutdown()).await.unwrap();
    watching.abort();
}

#[tokio::test(start_paused = true)]
async fn a_start_joined_to_an_automatic_one_ends_with_shutdowns_503_in_its_retry_wait() {
    let world = World::new(timing(100, 300_000, 10)).await;
    let profile = subscribed(&world, "Joined").await;
    // Readiness fails at once, so the start waits before it checks again.
    world.fake.knobs(|knobs| {
        knobs.failing_listing.insert(profile.clone());
    });
    world.scheduler.launch().await.unwrap();
    world
        .until("the failed readiness", Duration::from_secs(2), |world| {
            !world.fake.calls(&profile, "/auth-files").is_empty()
        })
        .await;
    assert!(world.pool().state().starting(&profile));
    let (joined, shut) = world
        .run(async {
            tokio::join!(
                world.pool().start(&profile, false),
                world.scheduler.shutdown()
            )
        })
        .await;
    shut.unwrap();
    assert_eq!(joined.unwrap_err().status, 503);
    assert!(world.reports().is_empty(), "{:?}", world.reports());
}

#[tokio::test(start_paused = true)]
async fn a_profile_the_launch_started_that_exited_before_scheduling_is_reported_on_its_first_failed_restart()
 {
    let world = World::new(timing(100, 300_000, 10)).await;
    let early = world.profile("Early", true, true, true).await;
    let late = world.profile("Late", true, true, true).await;
    // Early starts, then exits while the launch still waits for Late; its restart never answers.
    world.fake.script(
        &early,
        [
            Script::default(),
            Script {
                answers: None,
                ..Script::default()
            },
        ],
    );
    world.fake.script(
        &late,
        [Script {
            answers: Some(Duration::from_secs(1)),
            ..Script::default()
        }],
    );
    let mut launch = std::pin::pin!(world.scheduler.launch());
    let mut pool = world.pool().subscribe();
    // Early's start needs no clock, so Late cannot answer before it ends. Moving the clock
    // while its accounts sync could make its start fail, which the launch would report.
    tokio::select! {
        _ = &mut launch => panic!("the launch ended before Late answered"),
        _ = pool.wait_for(|pool| pool.running(&early) && !pool.starting(&early)) => {}
    }
    world.fake.kill(&early);
    world.run(launch).await.unwrap();
    assert!(
        !world.fake.latest(&early).unwrap().alive(),
        "Early exited during the launch"
    );
    assert!(
        world.reports().is_empty(),
        "the launch reports nothing: {:?}",
        world.reports()
    );
    world
        .until("the failed restart", Duration::from_secs(30), |world| {
            !world.reports().is_empty()
        })
        .await;
    let reports = world.reports();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(
        reports[0].starts_with("Early: Proxy did not become ready."),
        "{reports:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_start_that_succeeds_while_the_last_automatic_starts_quota_check_is_pending_gives_a_fresh_run()
 {
    let (open, gate) = watch::channel(true);
    let (world, profile) = giving_up("Pending check", vec![crashing(); 3]).await;
    *world.host.quota_gate.lock().unwrap() = Some(gate);
    world
        .until("the third core", Duration::from_secs(60), |world| {
            world.launches(&profile) == 3
        })
        .await;
    // From here on quota checks wait. A started core skips its check while an earlier one's
    // runs, and the third core's may begin after this, so the check left pending when the
    // limit's fourth core is up is that core's or the third's: either way an automatic
    // start's check is in flight.
    open.send_replace(false);
    world
        .until(
            "an automatic start's pending check",
            Duration::from_secs(60),
            |world| {
                world.launches(&profile) == 4
                    && world.pool().usable(&profile)
                    && world.host.quota_checks.lock().unwrap().len()
                        > world.host.quota_stored.lock().unwrap().len()
            },
        )
        .await;
    let fourth = world.fake.latest(&profile).unwrap();
    world.fake.kill(&profile);
    world.settle().await;
    world
        .run(world.pool().start(&profile, false))
        .await
        .unwrap();
    let started = world.fake.latest(&profile).unwrap();
    assert_ne!(started.index, fourth.index);
    assert!(world.pool().running(&profile));
    world.fake.kill(&profile);
    open.send_replace(true);
    world
        .until("the restart", Duration::from_secs(10), |world| {
            world.launches(&profile) == 6 && world.pool().usable(&profile)
        })
        .await;
    assert!(world.scheduler.gave_up(&profile).is_none());
    assert!(
        !world
            .reports()
            .iter()
            .any(|report| report.contains("Stopped restarting")),
        "{:?}",
        world.reports()
    );
}
