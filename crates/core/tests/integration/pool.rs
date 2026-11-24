//! The pool's starts, stops, swaps, settings and shutdown on fake cores under a paused clock,
//! from tests/core-lifecycle.test.ts. The scheduler's drains and polling are tested with it in
//! nsv-accounts.

use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use nsv_core::pool::{BoxFuture, CoreEvent, CoreId, Hooks, Pool, READY};
use nsv_core::processes::KILL_GRACE;
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::registry::Strategy;
use nsv_domain::wire::Runtime;
use nsv_net::Method;
use nsv_testkit::fake::Script;
use nsv_testkit::paused::Paused;
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

/// A pool over [`Paused`]'s store and fake cores.
struct World {
    paused: Paused,
    pool: Pool,
}

impl Deref for World {
    type Target = Paused;

    fn deref(&self) -> &Paused {
        &self.paused
    }
}

impl World {
    async fn new() -> Self {
        Self::with_ready(READY).await
    }

    async fn with_ready(ready: Duration) -> Self {
        let paused = Paused::new().await;
        let drain = Arc::new(Drain::default());
        let pool = Pool::new(
            paused.store.clone(),
            paused.binary.clone(),
            Arc::new(paused.backend()),
            ready,
            Arc::<Drain>::clone(&drain),
        );
        drain.0.set(pool.clone()).ok().unwrap();
        Self { paused, pool }
    }

    fn pool(&self) -> &Pool {
        &self.pool
    }
}

/// The scheduler's hook a stop waits for: it reads the stopping core's usage queue, which lives
/// only in the core's memory. What the queue held is the scheduler's concern.
#[derive(Default)]
struct Drain(OnceLock<Pool>);

impl Hooks for Drain {
    fn event(&self, _: &ProfileId, _: CoreEvent) {}

    fn ready(&self, _: ProfileId, _: CoreId) -> BoxFuture<'static, Result<(), AppError>> {
        Box::pin(async { Ok(()) })
    }

    fn before_stop(&self, profile: ProfileId) -> BoxFuture<'static, ()> {
        let pool = self.0.get().unwrap().clone();
        Box::pin(async move {
            let _ = pool
                .management(&profile, &Method::GET, "/usage-queue", None)
                .await;
        })
    }
}

/// A profile whose core has started.
async fn running(world: &World, name: &str) -> ProfileId {
    let profile = world.profile(name, false, true, false).await;
    world
        .run(world.pool().start(&profile, false))
        .await
        .unwrap();
    profile
}

#[tokio::test(start_paused = true)]
async fn a_core_that_never_becomes_ready_and_ignores_sigterm_is_killed_before_start_fails() {
    let world = World::with_ready(Duration::from_secs(1)).await;
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
        (Runtime::Error, Some(&Arc::from(error.message.as_str())))
    );
}

#[tokio::test(start_paused = true)]
async fn a_start_sends_no_key_to_a_listener_on_its_port_that_is_not_its_core() {
    let world = World::with_ready(Duration::from_secs(1)).await;
    let profile = world.profile("Squatted", false, true, false).await;
    world.fake.script(
        &profile,
        [Script {
            squatted: true,
            ..Script::default()
        }],
    );
    let began = Instant::now();
    let started = world.run(world.pool().start(&profile, false)).await;
    assert_eq!(
        world.fake.squatted(),
        0,
        "calls carrying a key reached the squatter"
    );
    let error = started.unwrap_err();
    assert_eq!(error.status, 503);
    assert!(
        error.message.starts_with("Proxy did not become ready. "),
        "{error:?}"
    );
    assert!(began.elapsed() >= Duration::from_secs(1));
    assert!(!world.fake.latest(&profile).unwrap().alive());
    assert_eq!(world.pool().state().runtime(&profile).0, Runtime::Error);
}

#[tokio::test(start_paused = true)]
async fn shutdown_completes_after_the_core_could_not_spawn() {
    let world = World::new().await;
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
    assert!(!world.pool().state().alive(&profile));
    world.run(world.pool().shutdown()).await.unwrap();
    assert!(!world.pool().state().starting(&profile));
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_slow_exiting_profiles_together_in_about_the_time_of_the_slowest() {
    let world = World::new().await;
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
    world.settle().await;
    let began = Instant::now();
    world.run(world.pool().shutdown()).await.unwrap();
    let took = began.elapsed();
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(2),
        "shutdown took {took:?}; one at a time takes over 3 s"
    );
    for profile in &profiles {
        assert!(
            world.fake.called_at(profile, "/usage-queue").last() >= Some(&began),
            "every profile's usage was drained"
        );
        assert!(!world.pool().state().alive(profile));
        assert_eq!(world.pool().state().runtime(profile).0, Runtime::Stopped);
    }
}

#[tokio::test(start_paused = true)]
async fn a_core_held_stopped_for_a_credential_swap_cannot_start_until_the_swap_ends_then_restarts()
{
    let world = World::new().await;
    let profile = running(&world, "Swap").await;
    let before = world.fake.latest(&profile).unwrap();
    let (told, observed) = oneshot::channel();
    let pool = world.pool().clone();
    let id = profile.clone();
    let swapped = world
        .run(world.pool().while_stopped(&profile, async move {
            let alive = pool.state().alive(&id);
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
        world.pool().state().running(&profile),
        "running again after the swap"
    );
    assert!(!before.alive());
    assert_eq!(world.fake.launches_of(&profile).len(), 2);
}

/// The routing the profile's core reports.
async fn strategy(world: &World, profile: &ProfileId) -> String {
    let config = world
        .run(
            world
                .pool()
                .management(profile, &Method::GET, "/config", None),
        )
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
    let world = World::new().await;
    let profile = world.profile("Settings", false, false, false).await;
    let first = Arc::new(AtomicBool::new(true));
    // The core's first readiness answer waits until the change is saved.
    let (release, holding) = world
        .fake
        .hold("/config", move |_| first.swap(false, Ordering::SeqCst));
    let test = async {
        let start = world.pool().start(&profile, false);
        let change = async {
            world
                .until("the held readiness", Duration::from_secs(1), || {
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
    let world = World::new().await;
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
    let world = World::new().await;
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

/// Holds the first core's first readiness answer. The counter counts `/config` calls to that
/// core once the flag is set: after it answered, only a settings confirmation would ask.
fn held_readiness(
    world: &World,
) -> (
    watch::Sender<bool>,
    Arc<AtomicUsize>,
    Arc<AtomicBool>,
    Arc<AtomicUsize>,
) {
    let first = Arc::new(AtomicBool::new(true));
    let (release, holding) = world
        .fake
        .hold("/config", move |_| first.swap(false, Ordering::SeqCst));
    let (answered, confirmations) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicUsize::new(0)),
    );
    let (seen, counted) = (Arc::clone(&answered), Arc::clone(&confirmations));
    world.fake.knobs(|knobs| {
        knobs.intercept = Some(Arc::new(move |core, path| {
            if core.index == 0 && path == "/config" && seen.load(Ordering::SeqCst) {
                counted.fetch_add(1, Ordering::SeqCst);
            }
            None
        }));
    });
    (release, holding, answered, confirmations)
}

async fn until_held(world: &World, holding: &AtomicUsize) {
    world
        .until("the held readiness", Duration::from_secs(1), || {
            holding.load(Ordering::SeqCst) > 0
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_settings_change_that_waited_for_a_start_leaves_alone_a_core_that_a_swap_then_stops() {
    let world = World::new().await;
    let profile = world.profile("Swapped settings", false, false, false).await;
    let (release, holding, answered, confirmations) = held_readiness(&world);
    let test = async {
        let start = world.pool().start(&profile, false);
        let rest = async {
            until_held(&world, &holding).await;
            save_strategy(&world, &profile, Strategy::RoundRobin).await;
            let configure = world.pool().configure(&profile);
            let swap = world.pool().while_stopped(&profile, async { Ok(()) });
            answered.store(true, Ordering::SeqCst);
            release.send_replace(true);
            tokio::join!(configure, swap)
        };
        tokio::join!(start, rest)
    };
    let (started, (configured, swapped)) = world.run(test).await;
    started.unwrap();
    configured.unwrap();
    swapped.unwrap();
    assert_eq!(
        confirmations.load(Ordering::SeqCst),
        0,
        "no request to the stopped core"
    );
    let cores = world.fake.launches_of(&profile);
    assert_eq!(cores.len(), 2);
    assert!(!cores[0].alive());
    // The successor wrote its own config from the saved settings.
    assert_eq!(strategy(&world, &profile).await, "round-robin");
}

#[tokio::test(start_paused = true)]
async fn a_settings_change_that_waited_for_a_start_leaves_alone_a_core_that_shutdown_then_stops() {
    let world = World::new().await;
    let profile = world
        .profile("Shutdown settings", false, false, false)
        .await;
    let (release, holding, answered, confirmations) = held_readiness(&world);
    let test = async {
        let start = world.pool().start(&profile, false);
        let rest = async {
            until_held(&world, &holding).await;
            save_strategy(&world, &profile, Strategy::RoundRobin).await;
            let configure = world.pool().configure(&profile);
            let shutdown = world.pool().shutdown();
            answered.store(true, Ordering::SeqCst);
            release.send_replace(true);
            tokio::join!(configure, shutdown)
        };
        tokio::join!(start, rest)
    };
    let (started, (configured, shut)) = world.run(test).await;
    // Shutdown ends the start that was still waiting for its core.
    assert_eq!(started.unwrap_err().status, 503);
    configured.unwrap();
    shut.unwrap();
    assert_eq!(
        confirmations.load(Ordering::SeqCst),
        0,
        "no request to the stopped core"
    );
    assert!(!world.fake.launches_of(&profile)[0].alive());
    assert!(!world.pool().state().alive(&profile));
}

#[tokio::test(start_paused = true)]
async fn a_settings_change_that_waited_for_a_start_leaves_alone_a_core_that_a_plain_stop_then_stops()
 {
    for change_first in [true, false] {
        let world = World::new().await;
        let profile = world.profile("Stopped settings", false, false, false).await;
        let (release, holding, answered, confirmations) = held_readiness(&world);
        let test = async {
            let start = world.pool().start(&profile, false);
            let rest = async {
                until_held(&world, &holding).await;
                save_strategy(&world, &profile, Strategy::RoundRobin).await;
                // As sign-in cleanup does for a core it cannot reach: neither a swap nor shutdown.
                let (configure, stop) = (
                    world.pool().configure(&profile),
                    world.pool().stop(&profile),
                );
                answered.store(true, Ordering::SeqCst);
                release.send_replace(true);
                if change_first {
                    let (configured, stopped) = tokio::join!(configure, stop);
                    (configured, stopped)
                } else {
                    let (stopped, configured) = tokio::join!(stop, configure);
                    (configured, stopped)
                }
            };
            tokio::join!(start, rest)
        };
        let (started, (configured, stopped)) = world.run(test).await;
        started.unwrap();
        configured.unwrap();
        stopped.unwrap();
        assert_eq!(
            confirmations.load(Ordering::SeqCst),
            0,
            "change first {change_first}: no request to the stopping core"
        );
        assert!(!world.fake.launches_of(&profile)[0].alive());
        world.fake.knobs(|knobs| knobs.intercept = None);
        world
            .run(world.pool().start(&profile, false))
            .await
            .unwrap();
        assert_eq!(
            strategy(&world, &profile).await,
            "round-robin",
            "change first {change_first}: the next start runs with the saved setting"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_confirmation_outstanding_while_a_swap_replaces_the_core_leaves_the_successors_config_alone()
 {
    let world = World::new().await;
    let profile = world
        .profile("Replaced confirmation", false, false, false)
        .await;
    world
        .run(world.pool().start(&profile, false))
        .await
        .unwrap();
    let first = world.fake.latest(&profile).unwrap();
    let (answer, answered) = watch::channel(false);
    let (polls, held) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicBool::new(false)),
    );
    {
        let (polls, held, first) = (Arc::clone(&polls), Arc::clone(&held), first.index);
        world.fake.knobs(|knobs| {
            // The first core's confirmations report the old settings; the tenth waits for `answer`.
            knobs.intercept = Some(Arc::new(move |core, path| {
                if core.index != first || path != "/config" {
                    return None;
                }
                if polls.fetch_add(1, Ordering::SeqCst) < 9 {
                    return Some(Box::pin(async { Some(serde_json::json!({})) }));
                }
                held.store(true, Ordering::SeqCst);
                let mut answered = answered.clone();
                Some(Box::pin(async move {
                    let _answered = answered.wait_for(|answered| *answered).await;
                    Some(serde_json::json!({}))
                }))
            }));
        });
    }
    save_strategy(&world, &profile, Strategy::RoundRobin).await;
    let test = async {
        let configure = world.pool().configure(&profile);
        let rest = async {
            world
                .until("the held confirmation", Duration::from_secs(15), || {
                    held.load(Ordering::SeqCst)
                })
                .await;
            world
                .pool()
                .while_stopped(&profile, async { Ok(()) })
                .await
                .unwrap();
            let successor = world.fake.latest(&profile).unwrap();
            assert_ne!(successor.index, first.index);
            assert_ne!(successor.port, first.port);
            answer.send_replace(true);
            successor
        };
        tokio::join!(configure, rest)
    };
    let (configured, successor) = world.run(test).await;
    configured.unwrap();
    let config =
        std::fs::read_to_string(nsv_files::DataDir::new(&world.data).core_config(&profile))
            .unwrap();
    assert!(
        config
            .lines()
            .any(|line| line == format!("\"port\": {}", successor.port)),
        "the successor's config keeps its own port: {} in {config}",
        successor.port
    );
    assert_eq!(strategy(&world, &profile).await, "round-robin");
}
