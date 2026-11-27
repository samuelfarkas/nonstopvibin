//! Core updates over fake cores and stub releases (`tests/core-updates.test.ts`), plus the
//! inventory's untested cases: a switch refused while a sign-in is pending, and shutdown
//! during a download.

use std::sync::Arc;

use nsv_accounts::scheduler::TIMING;
use nsv_core::binary::{CoreTarget, bundled_pin};
use nsv_core::install::CORE_ARCHIVES;
use nsv_core::pool::Phase;
use nsv_domain::registry::OAuthProvider;
use nsv_domain::version::{AppVersion, Version};
use nsv_domain::wire::CorePhase;
use nsv_testkit::TempDir;
use nsv_testkit::fake::FakeCores;
use nsv_testkit::world::{UPDATE, World, bundled_core, names, release};
use serde_json::json;
use tokio::sync::Notify;

const FEED: &str = "https://feed.example.invalid/core-release.json";

fn bundled_version() -> String {
    bundled_pin().unwrap().version.to_string()
}

async fn installed_world(name: &str) -> World {
    let world = World::with(name, Some(FEED), *TIMING).await;
    let (archive, pin) = release(world.directory.path(), b"update core");
    *world.releases.feed.lock().unwrap() = pin;
    *world.releases.archive.lock().unwrap() = archive;
    world
}

#[tokio::test]
async fn an_installed_build_updates_its_core_keeps_it_across_launches_and_goes_back() {
    let world = installed_world("updates-install").await;
    let profile = world.profile("Update fixture").await;
    world.cores.launch().await.unwrap();
    world.cores.start(&profile).await.unwrap();
    let updates = &world.cores;
    updates.check_core().await.unwrap();
    assert_eq!(updates.core_state().available.as_deref(), Some(UPDATE));
    assert_eq!(updates.core_version(), bundled_version());
    updates.install_core(UPDATE).await.unwrap();
    let state = updates.core_state();
    assert_eq!(
        (
            updates.core_version().as_str(),
            state.bundled.as_str(),
            state.available,
            state.phase
        ),
        (UPDATE, bundled_version().as_str(), None, None)
    );
    let cores = world.data().join("cores");
    let installed = cores.join(UPDATE).join("cli-proxy-api");
    let running = world.cores.pool().state();
    assert!(running.running(&profile));
    assert_eq!(*running.core(&profile).unwrap().binary, *installed);
    assert_eq!(names(&cores), [UPDATE]);
    let target = CoreTarget::current().unwrap();
    assert_eq!(
        *world.releases.downloads.lock().unwrap(),
        [format!(
            "{CORE_ARCHIVES}/v{UPDATE}/{}",
            target.archive(&Version::parse(UPDATE).unwrap())
        )]
    );
    // The new core started once on a throwaway store, which is gone.
    let checked: Vec<_> = world
        .fake
        .launches()
        .into_iter()
        .filter(|core| core.data.starts_with(world.data().join("oauth-pending")))
        .collect();
    assert_eq!(checked.len(), 1);
    assert!(
        checked[0]
            .binary
            .to_string_lossy()
            .contains("/cores/.staging-")
    );
    assert_eq!(world.pending().len(), 0);
    world.cores.shutdown().await.unwrap();

    let World {
        directory,
        store,
        bundled,
        fake,
        ..
    } = world;
    let data = store.directory().path().to_owned();
    store.close().await;
    let world = World::open(
        directory,
        &data,
        bundled.clone(),
        Some(FEED),
        *TIMING,
        fake,
        AppVersion::current(),
    )
    .await;
    world.cores.launch().await.unwrap();
    assert_eq!(world.cores.core_version(), UPDATE);
    world.cores.start(&profile).await.unwrap();
    assert_eq!(
        *world.cores.pool().state().core(&profile).unwrap().binary,
        *installed
    );
    world.cores.use_bundled_core().await.unwrap();
    assert_eq!(world.cores.core_version(), bundled_version());
    assert_eq!(names(&cores).len(), 0);
    assert_eq!(
        *world.cores.pool().state().core(&profile).unwrap().binary,
        *bundled
    );
    assert!(world.cores.pool().state().running(&profile));
    let again = world.cores.use_bundled_core().await.unwrap_err();
    assert_eq!(
        (again.message.as_str(), again.status),
        ("NonstopVibin already runs its bundled core.", 409)
    );
    world.cores.shutdown().await.unwrap();
}

#[tokio::test]
async fn no_other_start_slips_in_while_an_update_restarts_a_core_on_the_new_binary() {
    let world = installed_world("updates-swap").await;
    let profile = world.profile("Swap fixture").await;
    world.cores.launch().await.unwrap();
    world.cores.start(&profile).await.unwrap();
    world.cores.check_core().await.unwrap();
    // The stop's usage drain waits, so another start arrives between its stop and its start.
    let held = profile.clone();
    let (release, _holding) = world
        .fake
        .hold("/usage-queue", move |profile| *profile == held);
    let cores = Arc::clone(&world.cores);
    let install = tokio::spawn(async move { cores.install_core(UPDATE).await });
    let mut pool = world.cores.pool().subscribe();
    pool.wait_for(|state| {
        state
            .core(&profile)
            .is_some_and(|core| matches!(core.phase, Phase::Stopping { .. }))
    })
    .await
    .unwrap();
    // One poll queues the start in the pool's command queue, ahead of the stop's end.
    let mut other = std::pin::pin!(world.cores.start(&profile));
    let queued = std::future::poll_fn(|context| {
        std::task::Poll::Ready(other.as_mut().poll(context).is_pending())
    })
    .await;
    assert!(queued, "the start waits for the stop in flight");
    release.send_replace(true);
    install.await.unwrap().unwrap();
    let refused = other.await.unwrap_err();
    assert_eq!(refused.status, 409);
    let state = world.cores.pool().state();
    assert!(state.running(&profile));
    assert_eq!(
        *state.core(&profile).unwrap().binary,
        *world
            .data()
            .join("cores")
            .join(UPDATE)
            .join("cli-proxy-api")
    );
    world.cores.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_core_off_the_pin_too_new_for_this_app_not_newer_or_not_starting_is_refused() {
    let world = installed_world("updates-refused").await;
    let updates = &world.cores;
    let cores = world.data().join("cores");
    let (_, pin) = release(world.directory.path(), b"update core");
    *world.releases.archive.lock().unwrap() = b"not the reviewed archive".to_vec();
    let tampered = updates.install_core(UPDATE).await.unwrap_err();
    assert_eq!(
        (tampered.message.as_str(), tampered.status),
        (
            "The downloaded core does not match the reviewed pin. Nothing was changed.",
            502
        )
    );
    assert_eq!(
        (updates.core_version(), updates.core_state().phase),
        (bundled_version(), None)
    );
    assert_eq!(names(&cores).len(), 0);
    let mut too_new = pin.clone();
    too_new["minApp"] = json!("999.0.0");
    let mut same = pin.clone();
    same["version"] = json!(bundled_version());
    for refused in [too_new, same] {
        *world.releases.feed.lock().unwrap() = refused.clone();
        updates.check_core().await.unwrap();
        assert_eq!(updates.core_state().available, None);
        let version = refused["version"].as_str().unwrap();
        let install = updates.install_core(version).await.unwrap_err();
        assert_eq!(
            (install.message, install.status),
            (format!("CLIProxyAPI {version} is no longer offered."), 409)
        );
    }
    let (archive, broken) = release(world.directory.path(), b"broken core");
    *world.releases.feed.lock().unwrap() = broken;
    *world.releases.archive.lock().unwrap() = archive;
    let failed = updates.install_core(UPDATE).await.unwrap_err();
    assert_eq!(failed.status, 502);
    assert!(
        failed
            .message
            .starts_with("CLIProxyAPI 99.0.0 did not start: Could not launch proxy"),
        "{failed:?}"
    );
    assert!(failed.message.ends_with(" Nothing was changed."));
    assert_eq!(updates.core_version(), bundled_version());
    assert_eq!(names(&cores).len(), 0);
    assert_eq!(world.pending().len(), 0);
    *world.releases.feed.lock().unwrap() = json!({ "version": UPDATE });
    let unreadable = updates.check_core().await.unwrap_err();
    assert_eq!(
        (unreadable.message.as_str(), unreadable.status),
        (
            "Could not check for core updates: the feed is not a core pin.",
            502
        )
    );
}

#[tokio::test]
async fn a_beta_build_starts_and_takes_cores_that_admit_its_prerelease() {
    // `Cores::new` refused a `-beta.N` app version, so a beta build's daemon did not start.
    let directory = TempDir::new("updates-beta");
    let data = directory.path().join("data");
    let bundled = bundled_core(directory.path());
    let beta = AppVersion::parse("0.2.0-beta.1").unwrap();
    let world = World::open(
        directory,
        &data,
        bundled,
        Some(FEED),
        *TIMING,
        FakeCores::default(),
        beta,
    )
    .await;
    world.cores.launch().await.unwrap();
    let (_, pin) = release(world.directory.path(), b"update core");
    for (min_app, offered) in [
        ("0.2.0", None),
        ("0.2.0-beta.2", None),
        ("0.2.0-beta.1", Some(UPDATE)),
        ("0.1.9", Some(UPDATE)),
    ] {
        let mut feed = pin.clone();
        feed["minApp"] = json!(min_app);
        *world.releases.feed.lock().unwrap() = feed;
        world.cores.check_core().await.unwrap();
        assert_eq!(
            world.cores.core_state().available.as_deref(),
            offered,
            "{min_app}"
        );
    }
    world.cores.shutdown().await.unwrap();
}

#[tokio::test]
async fn going_back_to_a_bundled_core_that_fails_its_check_keeps_the_update() {
    let world = installed_world("updates-damaged").await;
    world.cores.install_core(UPDATE).await.unwrap();
    std::fs::write(&world.bundled, b"damaged").unwrap();
    let refused = world.cores.use_bundled_core().await.unwrap_err();
    assert_eq!(
        (refused.message.as_str(), refused.status),
        (
            "The proxy core failed its integrity check. Reinstall NonstopVibin.",
            503
        )
    );
    assert_eq!(world.cores.core_version(), UPDATE);
    assert_eq!(names(&world.data().join("cores")), [UPDATE]);
}

#[tokio::test]
async fn development_runs_never_select_install_or_delete_installed_cores() {
    let world = World::new("updates-development").await;
    let updates = &world.cores;
    let kept = world.data().join("cores").join(UPDATE);
    std::fs::create_dir_all(&kept).unwrap();
    updates.launch().await.unwrap();
    assert_eq!(updates.core_version(), bundled_version());
    assert!(kept.exists());
    for refused in [
        updates.check_core().await,
        updates.install_core(UPDATE).await,
        updates.use_bundled_core().await,
    ] {
        let refused = refused.unwrap_err();
        assert_eq!(
            (refused.message.as_str(), refused.status),
            ("Only installed builds update the core.", 409)
        );
    }
    assert!(!updates.core_state().updates);
}

#[tokio::test]
async fn a_switch_waits_for_no_sign_in_and_shutdown_aborts_a_download() {
    let world = installed_world("updates-sign-in").await;
    let profile = world.profile("Signing in").await;
    let updates = Arc::clone(&world.cores);
    let session = world
        .cores
        .begin_sign_in(&profile, OAuthProvider::Codex)
        .await
        .unwrap();
    let refused = updates.install_core(UPDATE).await.unwrap_err();
    assert_eq!(
        (refused.message.as_str(), refused.status),
        ("Finish or cancel the sign-in first.", 409)
    );
    let cores = world.data().join("cores");
    assert_eq!(names(&cores).len(), 0, "the staged core was discarded");
    assert_eq!(
        (updates.core_version(), updates.core_state().phase),
        (bundled_version(), None)
    );
    world
        .cores
        .cancel_sign_in(&profile, &session.state)
        .await
        .unwrap();

    let (waiting, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    *world.releases.hold.lock().unwrap() = Some((Arc::clone(&waiting), release));
    let install = tokio::spawn({
        let updates = Arc::clone(&updates);
        async move { updates.install_core(UPDATE).await }
    });
    waiting.notified().await;
    assert_eq!(updates.core_state().phase, Some(CorePhase::Downloading));
    for busy in [updates.use_bundled_core().await, updates.check_core().await] {
        let busy = busy.unwrap_err();
        assert_eq!(
            (busy.message.as_str(), busy.status),
            ("A core update is already in progress.", 409)
        );
    }
    world.cores.shutdown().await.unwrap();
    let aborted = install.await.unwrap().unwrap_err();
    assert_eq!(
        (aborted.message.as_str(), aborted.status),
        ("nonstopvibin is shutting down.", 503)
    );
    assert_eq!(names(&cores).len(), 0);
    assert_eq!(updates.core_state().phase, None);
}
