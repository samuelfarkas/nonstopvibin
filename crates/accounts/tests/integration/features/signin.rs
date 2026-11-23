//! Browser sign-in through fake cores: sessions, the callback listener, pasted redirects,
//! expiry, the idle hold, and Claude's reviewed seats (`tests/integration.test.ts`,
//! `tests/core-lifecycle.test.ts`, `tests/organization-seats.test.ts`).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use nsv_accounts::scheduler::TIMING;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::registry::OAuthProvider;
use nsv_domain::wire::{AccountPatch, OAuthAction, OAuthSession, OAuthStatus, Quota, QuotaStatus};
use nsv_net::Method;
use nsv_testkit::fake::response;
use nsv_testkit::world::{ACCOUNT, ORGANIZATION_A, ORGANIZATION_B, World, claude, names, skew};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use url::Url;

use super::common::{case, cases};

/// A raw request to the callback listener; what came back before the connection closed.
async fn request(port: u16, method: &str, target: String) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let head =
        format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    // A connection closed under the request ends the answer, which is what some tests expect.
    let _read = stream.read_to_end(&mut answer).await;
    String::from_utf8_lossy(&answer).into_owned()
}

fn codex() -> Value {
    json!({
        "type": "codex",
        "email": "codex@example.invalid",
        "access_token": "synthetic-codex-access",
        "refresh_token": "synthetic-codex-refresh",
    })
}

fn golden_error(name: &str) -> (String, u64) {
    let cases = cases("api");
    let error = &case(&cases, name)["output"]["body"]["error"];
    (
        error["message"].as_str().unwrap().to_owned(),
        error["code"].as_u64().unwrap(),
    )
}

#[tokio::test]
async fn oauth_sessions_are_profile_bound_cancellable_and_one_at_a_time() {
    let world = World::new("oauth-sessions").await;
    let (company, personal) = (
        world.profile("Company").await,
        world.profile("Personal").await,
    );
    world.fake.knobs(|knobs| {
        knobs.redirect = Some("http://localhost:0/auth/callback".to_owned());
    });
    let sign_in = &world.cores;
    let (first, second) = tokio::join!(
        sign_in.begin_sign_in(&company, OAuthProvider::Codex, None),
        sign_in.begin_sign_in(&personal, OAuthProvider::Codex, None),
    );
    let (session, refused) = match (first, second) {
        (Ok(session), Err(refused)) | (Err(refused), Ok(session)) => (session, refused),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        (refused.message.as_str(), refused.status),
        (
            "Finish or cancel the current sign-in before starting another.",
            409
        )
    );
    assert_eq!(
        Url::parse(&session.url).unwrap().host_str(),
        Some("auth.example.invalid")
    );
    let port = sign_in.callback_port().unwrap();
    assert!(
        request(port, "GET", "//[".to_owned())
            .await
            .starts_with("HTTP/1.1 400")
    );
    assert_eq!(
        sign_in
            .sign_in_status(&session.profile_id, &session.state)
            .await
            .unwrap(),
        OAuthStatus::Wait
    );
    let other = if session.profile_id == company {
        &personal
    } else {
        &company
    };
    let (message, status) = golden_error("GET oauth unknown state");
    for refused in [
        sign_in
            .sign_in_status(other, &session.state)
            .await
            .unwrap_err(),
        sign_in
            .cancel_sign_in(other, &session.state)
            .await
            .unwrap_err(),
        sign_in
            .sign_in_status(&session.profile_id, "nope")
            .await
            .unwrap_err(),
    ] {
        assert_eq!(
            (refused.message.as_str(), u64::from(refused.status)),
            (message.as_str(), status)
        );
    }
    assert_eq!(
        sign_in.sign_in_session(&session.profile_id),
        Some(session.clone())
    );
    assert_eq!(
        sign_in.sign_in_session(other),
        None,
        "GET oauth-session answers null"
    );
    sign_in
        .cancel_sign_in(&session.profile_id, &session.state)
        .await
        .unwrap();
    assert_eq!(sign_in.sign_in_session(&session.profile_id), None);
    assert_eq!(sign_in.callback_port(), None);
    assert_eq!(
        world.fake.calls(&session.profile_id, "/oauth-session"),
        [(
            Method::DELETE,
            format!("/oauth-session?state={}", session.state)
        )]
    );
    let next = sign_in
        .begin_sign_in(&personal, OAuthProvider::Codex, None)
        .await
        .unwrap();
    sign_in
        .cancel_sign_in(&personal, &next.state)
        .await
        .unwrap();
}

#[tokio::test]
async fn cancellation_closes_callbacks_queued_behind_it_and_a_matching_callback_completes() {
    let world = World::new("oauth-callbacks").await;
    let profile = world.profile("Callbacks").await;
    world.fake.knobs(|knobs| {
        knobs.redirect = Some("http://127.0.0.1:0/callback".to_owned());
        knobs.credential = codex();
    });
    let sign_in = Arc::clone(&world.cores);
    let session = sign_in
        .begin_sign_in(&profile, OAuthProvider::Codex, None)
        .await
        .unwrap();
    let port = sign_in.callback_port().unwrap();
    let (entered, (release, released)) = (Arc::new(Notify::new()), watch::channel(false));
    world
        .fake
        .knobs(|knobs| knobs.cancel_gate = Some((Arc::clone(&entered), released)));
    let cancel = tokio::spawn({
        let (sign_in, profile, state) =
            (Arc::clone(&sign_in), profile.clone(), session.state.clone());
        async move { sign_in.cancel_sign_in(&profile, &state).await }
    });
    entered.notified().await;
    // Its handler queues the callback behind the cancellation.
    let queued = tokio::spawn(request(
        port,
        "GET",
        format!("/callback?state={}", session.state),
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;
    release.send_replace(true);
    cancel.await.unwrap().unwrap();
    assert_eq!(
        queued.await.unwrap(),
        "",
        "the cancellation closed its connection"
    );
    assert_eq!(world.accepted.load(Ordering::SeqCst), 0);
    world.fake.knobs(|knobs| knobs.cancel_gate = None);

    let next = sign_in
        .begin_sign_in(&profile, OAuthProvider::Codex, None)
        .await
        .unwrap();
    let port = sign_in.callback_port().unwrap();
    for (method, target) in [
        ("GET", "/callback?state=wrong".to_owned()),
        ("GET", format!("/elsewhere?state={}", next.state)),
        ("POST", format!("/callback?state={}", next.state)),
    ] {
        let answer = request(port, method, target.clone()).await;
        assert!(
            answer.starts_with("HTTP/1.1 400"),
            "{method} {target}: {answer}"
        );
        assert!(answer.ends_with("This sign-in callback does not match the pending session."));
    }
    let answer = request(
        port,
        "GET",
        format!("/callback?state={}&code=synthetic", next.state),
    )
    .await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.contains("Sign-in received."));
    assert!(
        answer
            .to_ascii_lowercase()
            .contains("cache-control: no-store")
    );
    assert_eq!(world.accepted.load(Ordering::SeqCst), 1);
    assert_eq!(
        sign_in.callback_port(),
        None,
        "an accepted callback closes the listener"
    );
    assert_eq!(
        sign_in.sign_in_status(&profile, &next.state).await.unwrap(),
        OAuthStatus::Ok {
            account: None,
            reconnected: None
        }
    );
    assert_eq!(sign_in.sign_in_session(&profile), None);
    let accounts = world.cores.accounts(&profile).await.unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].provider, "codex");
    assert!(accounts[0].quota.is_some(), "its quota was checked");
}

#[tokio::test]
async fn a_busy_callback_port_is_refused_and_ends_the_pending_authorization() {
    let world = World::new("oauth-busy").await;
    let profile = world.profile("Busy").await;
    let busy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = busy.local_addr().unwrap().port();
    world.fake.knobs(|knobs| {
        knobs.redirect = Some(format!("http://localhost:{port}/auth/callback"));
    });
    let sign_in = &world.cores;
    let refused = sign_in
        .begin_sign_in(&profile, OAuthProvider::Codex, None)
        .await
        .unwrap_err();
    assert_eq!(
        (refused.message, refused.status),
        (
            format!(
                "Sign-in port {port} is in use. Close any pending sign-in in another app, then try again."
            ),
            409
        )
    );
    assert_eq!(sign_in.sign_in_session(&profile), None);
    assert_eq!(world.fake.calls(&profile, "/oauth-session").len(), 1);
    drop(busy);
    let session = sign_in
        .begin_sign_in(&profile, OAuthProvider::Codex, None)
        .await
        .unwrap();
    assert_eq!(sign_in.callback_port(), Some(port));
    sign_in
        .cancel_sign_in(&profile, &session.state)
        .await
        .unwrap();
    for unsupported in [
        "https://localhost:1455/cb",
        "http://example.invalid:1455/cb",
        "http://localhost/cb",
    ] {
        world
            .fake
            .knobs(|knobs| knobs.redirect = Some(unsupported.to_owned()));
        let refused = sign_in
            .begin_sign_in(&profile, OAuthProvider::Codex, None)
            .await
            .unwrap_err();
        assert_eq!(
            (refused.message.as_str(), refused.status),
            (
                "The provider returned an unsupported callback address.",
                502
            ),
            "{unsupported}"
        );
    }
}

#[tokio::test]
async fn a_pasted_redirect_url_completes_a_headless_sign_in_once() {
    let world = World::new("oauth-pasted").await;
    let (profile, other) = (
        world.profile("Headless").await,
        world.profile("Other").await,
    );
    world.fake.knobs(|knobs| knobs.credential = codex());
    let sign_in = &world.cores;
    let session = sign_in
        .begin_sign_in(&profile, OAuthProvider::Codex, None)
        .await
        .unwrap();
    assert_eq!(
        sign_in.callback_port(),
        None,
        "a link without a redirect needs no listener"
    );
    let redirect =
        |state: &str| format!("http://localhost:1455/auth/callback?code=synthetic&state={state}");
    let malformed = sign_in
        .sign_in_callback(&profile, "not a url")
        .await
        .unwrap_err();
    assert_eq!(
        (malformed.message.as_str(), malformed.status),
        ("Malformed sign-in callback.", 400)
    );
    for (owner, state) in [(&profile, "nope"), (&other, session.state.as_str())] {
        let refused = sign_in
            .sign_in_callback(owner, &redirect(state))
            .await
            .unwrap_err();
        assert_eq!(refused.status, 404);
    }
    sign_in
        .sign_in_callback(&profile, &redirect(&session.state))
        .await
        .unwrap();
    assert_eq!(world.fake.calls(&profile, "/oauth-callback").len(), 1);
    assert!(matches!(
        sign_in
            .sign_in_status(&profile, &session.state)
            .await
            .unwrap(),
        OAuthStatus::Ok { .. }
    ));
    let late = sign_in
        .sign_in_callback(&profile, &redirect(&session.state))
        .await
        .unwrap_err();
    assert_eq!(late.status, 404);
    assert_eq!(world.cores.accounts(&profile).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_failed_authorization_ends_the_sign_in_and_an_expired_one_is_cleaned_up() {
    let world = World::new("oauth-expiry").await;
    let profile = world.profile("Expiry").await;
    let sign_in = &world.cores;
    world.fake.knobs(|knobs| knobs.auth_fails = true);
    let session = sign_in
        .begin_sign_in(&profile, OAuthProvider::Codex, None)
        .await
        .unwrap();
    assert_eq!(
        sign_in
            .sign_in_status(&profile, &session.state)
            .await
            .unwrap(),
        OAuthStatus::Error {
            error: "Sign-in did not complete. Start a new sign-in.".to_owned()
        }
    );
    assert_eq!(sign_in.sign_in_session(&profile), None);
    world.fake.knobs(|knobs| {
        knobs.auth_fails = false;
        knobs.expires_in = Some(json!(1));
    });
    let session = sign_in
        .begin_sign_in(&profile, OAuthProvider::Codex, None)
        .await
        .unwrap();
    assert_eq!(session.expires_at - session.started_at, 1000);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        sign_in.sign_in_session(&profile),
        None,
        "the expiry timer ended it"
    );
    assert_eq!(
        world.fake.calls(&profile, "/oauth-session"),
        [(
            Method::DELETE,
            format!("/oauth-session?state={}", session.state)
        )]
    );
    assert_eq!(world.reports.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn a_pending_sign_in_keeps_an_idle_core_running() {
    let idle = Duration::from_millis(300);
    // Wakeups coalesce over 50 ms instead of two seconds.
    let restart = nsv_accounts::restart::RestartTiming {
        coalesce: 50,
        ..TIMING.restart
    };
    let world = World::with(
        "oauth-idle",
        None,
        nsv_accounts::scheduler::Timing {
            idle,
            restart,
            coalesce: Duration::from_millis(50),
            usage_retry: Duration::from_millis(50),
            ..*TIMING
        },
    )
    .await;
    let profile = world.profile("Lazy").await;
    world.cores.launch().await.unwrap();
    world.cores.wake(&profile).await.unwrap();
    let session = world
        .cores
        .begin_sign_in(&profile, OAuthProvider::Codex, None)
        .await
        .unwrap();
    tokio::time::sleep(idle * 4).await;
    assert!(
        world.cores.pool().state().running(&profile),
        "a pending sign-in"
    );
    world
        .cores
        .cancel_sign_in(&profile, &session.state)
        .await
        .unwrap();
    tokio::time::sleep(idle * 4).await;
    assert!(
        !world.cores.pool().state().running(&profile),
        "idle once it ended"
    );
    world.cores.shutdown().await.unwrap();
}

#[tokio::test]
async fn launch_reaps_and_deletes_sign_ins_an_earlier_run_left_pending() {
    let world = World::new("oauth-discard").await;
    let left = world.data().join("oauth-pending").join("claude-AbCdEf");
    std::fs::create_dir_all(&left).unwrap();
    world.cores.launch().await.unwrap();
    assert!(
        world.fake.reaped().contains(&left),
        "its own ledger was reaped"
    );
    assert!(!world.data().join("oauth-pending").exists());
    world.cores.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_undeletable_pending_sign_in_does_not_keep_launch_from_reaping_orphans() {
    use std::os::unix::fs::PermissionsExt;
    let world = World::new("oauth-undeletable").await;
    let pending = world.data().join("oauth-pending");
    std::fs::create_dir_all(pending.join("claude-AbCdEf")).unwrap();
    std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::create_dir(pending.join("probe")).is_ok() {
        // Root ignores directory permissions, so nothing is undeletable.
        std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let launched = world.cores.launch().await;
    std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        launched.is_err(),
        "the pending sign-in could not be deleted"
    );
    assert!(
        world.fake.reaped().contains(&world.data().to_path_buf()),
        "the data directory's orphans were reaped first"
    );
}

#[tokio::test]
async fn shutdown_lets_a_sign_in_action_finish_before_it_waits_for_the_changes_it_queues() {
    let world = World::new("oauth-shutdown-action").await;
    let profile = world.profile("Sign-in order").await;
    world.cores.pool().start(&profile, false).await.unwrap();
    let (entered, (release, released)) = (Arc::new(Notify::new()), watch::channel(false));
    let (queued, change) = tokio::sync::oneshot::channel();
    let queued = Arc::new(std::sync::Mutex::new(Some(queued)));
    world.fake.knobs(|knobs| {
        let (credentials, fake, entered) = (
            Arc::clone(&world.cores),
            world.fake.clone(),
            Arc::clone(&entered),
        );
        knobs.auth_url_hook = Some(Arc::new(move |session| {
            let (credentials, fake, entered, queued) = (
                Arc::clone(&credentials),
                fake.clone(),
                Arc::clone(&entered),
                Arc::clone(&queued),
            );
            let mut released = released.clone();
            Box::pin(async move {
                entered.notify_one();
                drop(released.wait_for(|released| *released).await);
                // The action queues a credential change as it ends, after shutdown began.
                let mut change = Box::pin(async move {
                    credentials
                        .after_credential_changes(async move {
                            tokio::task::yield_now().await;
                            Ok(!fake.calls(&session, "/oauth-session").is_empty())
                        })
                        .await
                });
                // One poll takes the change's turn in the queue before the action answers.
                tokio::select! {
                    biased;
                    _ = &mut change => panic!("the change waits for its turn"),
                    () = std::future::ready(()) => {}
                }
                let sender = queued.lock().unwrap().take().unwrap();
                let _sent = sender.send(tokio::spawn(change));
            })
        }));
    });
    let action = tokio::spawn({
        let (cores, profile) = (Arc::clone(&world.cores), profile.clone());
        async move {
            cores
                .begin_sign_in(&profile, OAuthProvider::Codex, None)
                .await
        }
    });
    entered.notified().await;
    let shutdown = tokio::spawn({
        let cores = Arc::clone(&world.cores);
        async move { cores.shutdown().await }
    });
    tokio::task::yield_now().await;
    release.send_replace(true);
    let _began = action.await.unwrap();
    let cleared = change.await.unwrap().await.unwrap().unwrap();
    assert!(
        !cleared,
        "the change ran before shutdown dropped the sign-in"
    );
    shutdown.await.unwrap().unwrap();
    assert!(!world.cores.pool().state().alive(&profile));
}

#[tokio::test]
async fn shutdown_lets_a_queued_credential_change_finish_before_it_stops_the_core() {
    let world = World::new("oauth-shutdown-order").await;
    let profile = world.profile("Order").await;
    world.cores.pool().start(&profile, false).await.unwrap();
    let (entered, (release, released)) = (Arc::new(Notify::new()), watch::channel(false));
    let change = tokio::spawn({
        let (credentials, pool, profile, entered) = (
            Arc::clone(&world.cores),
            world.cores.pool().clone(),
            profile.clone(),
            Arc::clone(&entered),
        );
        let mut released = released;
        async move {
            credentials
                .after_credential_changes(async move {
                    entered.notify_one();
                    let _released = released.wait_for(|released| *released).await;
                    Ok(pool.state().holding(&profile))
                })
                .await
        }
    });
    entered.notified().await;
    let shutdown = tokio::spawn({
        let cores = Arc::clone(&world.cores);
        async move { cores.shutdown().await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let refused = world.cores.pool().start(&profile, false).await.unwrap_err();
    assert_eq!(refused.status, 503);
    release.send_replace(true);
    assert!(
        !change.await.unwrap().unwrap(),
        "no stop began during the change"
    );
    shutdown.await.unwrap().unwrap();
    assert!(!world.cores.pool().state().alive(&profile));
}

/// Signs in to Claude on the profile's sandbox and pastes the redirect.
async fn authorize(
    world: &World,
    profile: &ProfileId,
    credential: Value,
    reconnect: Option<&AccountId>,
) -> OAuthSession {
    let verified = credential.clone();
    world.fake.knobs(|knobs| knobs.credential = credential);
    world.fetch.answer(move |_| {
        Ok(response(
            200,
            json!({
                "account": { "uuid": ACCOUNT, "email": verified["email"] },
                "organization": {
                    "uuid": verified["organization_uuid"],
                    "name": verified["organization_name"],
                },
            })
            .to_string(),
        ))
    });
    let session = world
        .cores
        .begin_sign_in(profile, OAuthProvider::Claude, reconnect.cloned())
        .await
        .unwrap();
    let mut callback = Url::parse("http://localhost:54545/callback").unwrap();
    callback
        .query_pairs_mut()
        .append_pair("state", &session.state)
        .append_pair("code", "synthetic-code");
    world
        .cores
        .sign_in_callback(profile, callback.as_str())
        .await
        .unwrap();
    session
}

async fn review(
    world: &World,
    profile: &ProfileId,
    session: &OAuthSession,
) -> (OAuthAction, Value) {
    match world
        .cores
        .sign_in_status(profile, &session.state)
        .await
        .unwrap()
    {
        OAuthStatus::Review { review } => (review.action, serde_json::to_value(review).unwrap()),
        other => panic!("{other:?}"),
    }
}

async fn confirm(
    world: &World,
    profile: &ProfileId,
    session: &OAuthSession,
) -> nsv_domain::wire::Account {
    review(world, profile, session).await;
    match world
        .cores
        .confirm_sign_in(profile, &session.state)
        .await
        .unwrap()
    {
        OAuthStatus::Ok {
            account: Some(account),
            ..
        } => {
            if !account.disabled {
                assert_eq!(remaining(account.quota.as_ref().unwrap()), Some(75.0));
            }
            account
        }
        other => panic!("{other:?}"),
    }
}

/// The first window's remaining headroom.
pub(crate) fn remaining(quota: &Quota) -> Option<f64> {
    serde_json::to_value(quota.windows.first()?.remaining_percent)
        .ok()?
        .as_f64()
}

fn file(world: &World, profile: &ProfileId, account: &AccountId) -> String {
    std::fs::read_to_string(world.credential_file(profile, account.as_str())).unwrap()
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the TypeScript test's sequence of seats, in order"
)]
async fn same_email_seats_stay_separate_through_review_reconnect_imports_and_cleanup() {
    let world = World::new("oauth-seats").await;
    let (first, second) = (
        world.profile("First profile").await,
        world.profile("Second profile").await,
    );
    let sign_in = Arc::clone(&world.cores);
    let accounts = Arc::clone(&world.cores);

    // A pending organization is invisible to active profiles until confirmation.
    let session = authorize(&world, &first, claude(ORGANIZATION_A, "a"), None).await;
    assert_eq!(accounts.accounts(&first).await.unwrap().len(), 0);
    let (action, shown) = review(&world, &first, &session).await;
    assert_eq!(action, OAuthAction::Add);
    assert_eq!(shown["account"]["organizationName"], "Example A");
    assert!(!shown.to_string().contains("synthetic-access"));
    assert_eq!(accounts.accounts(&first).await.unwrap().len(), 0);
    let elsewhere = sign_in
        .confirm_sign_in(&second, &session.state)
        .await
        .unwrap_err();
    assert_eq!(elsewhere.status, 404);
    let account = confirm(&world, &first, &session).await;
    let (first_id, first_index) = (account.id.clone(), account.auth_index.clone().unwrap());
    assert!(!first_id.as_str().contains('@'));
    assert_eq!(account.organization_name.as_deref(), Some("Example A"));
    let mode = std::fs::metadata(world.credential_file(&first, first_id.as_str()))
        .unwrap()
        .permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
        0o600
    );
    assert_eq!(world.pending().len(), 0);

    // A second organization with the same email adds a separate connection.
    let session = authorize(&world, &first, claude(ORGANIZATION_B, "b"), None).await;
    let account = confirm(&world, &first, &session).await;
    let second_id = account.id.clone();
    assert_ne!(second_id, first_id);
    assert_ne!(account.auth_index.as_deref(), Some(first_index.as_str()));
    let listed = accounts.accounts(&first).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .all(|account| account.email.as_deref() == Some("same@example.invalid"))
    );

    // Reconnecting keeps the id, paused state, label, priority, quota and the other seat.
    world
        .cores
        .set_account(
            &first,
            &first_id,
            AccountPatch {
                disabled: Some(true),
                priority: Some(7),
                name: Some("Work seat".to_owned()),
            },
        )
        .await
        .unwrap();
    let kept = Quota {
        status: QuotaStatus::Available,
        windows: Vec::new(),
        checked_at: "2026-09-06T00:00:00Z".to_owned(),
        error: None,
        plan: None,
        banked_resets: None,
    };
    {
        let (profile, account, quota) = (first.clone(), first_id.clone(), kept.clone());
        world
            .store
            .call(move |db| db.save_quota(&profile, &account, &quota))
            .await
            .unwrap();
    }
    let other_before = file(&world, &first, &second_id);
    let core_before = world.cores.pool().state().core(&first).unwrap().id;
    let session = authorize(
        &world,
        &first,
        claude(ORGANIZATION_A, "reauthorized"),
        Some(&first_id),
    )
    .await;
    assert_eq!(
        review(&world, &first, &session).await.0,
        OAuthAction::Reconnect
    );
    let account = confirm(&world, &first, &session).await;
    assert_ne!(
        world.cores.pool().state().core(&first).unwrap().id,
        core_before,
        "its core restarted"
    );
    assert!(world.cores.pool().state().running(&first));
    assert_eq!(account.id, first_id);
    assert_eq!(account.auth_index.as_deref(), Some(first_index.as_str()));
    assert!(account.disabled);
    assert_eq!(account.name, "Work seat");
    assert_eq!(account.priority, 7);
    assert_eq!(account.quota.unwrap().checked_at, kept.checked_at);
    assert!(file(&world, &first, &first_id).contains("synthetic-access-reauthorized"));
    assert_eq!(file(&world, &first, &second_id), other_before);
    assert_eq!(accounts.accounts(&first).await.unwrap().len(), 2);

    // Reconnecting a stopped profile keeps it stopped.
    world.cores.pool().stop(&first).await.unwrap();
    let session = authorize(
        &world,
        &first,
        claude(ORGANIZATION_A, "stopped"),
        Some(&first_id),
    )
    .await;
    let account = confirm(&world, &first, &session).await;
    assert_eq!(account.id, first_id);
    assert!(
        !world.cores.pool().state().alive(&first),
        "a stopped profile is not started"
    );
    assert!(account.disabled);
    assert!(file(&world, &first, &first_id).contains("synthetic-access-stopped"));
    world.cores.pool().start(&first, false).await.unwrap();

    // The wrong organization on reconnect is blocked before anything is replaced.
    let before = file(&world, &first, &first_id);
    let session = authorize(
        &world,
        &first,
        claude(ORGANIZATION_B, "wrong-seat"),
        Some(&first_id),
    )
    .await;
    let (action, shown) = review(&world, &first, &session).await;
    assert_eq!(action, OAuthAction::Blocked);
    assert!(
        shown["message"]
            .as_str()
            .unwrap()
            .starts_with("This is not the organization")
    );
    let blocked = sign_in
        .confirm_sign_in(&first, &session.state)
        .await
        .unwrap_err();
    assert!(
        blocked.message.starts_with("This is not the organization"),
        "{blocked:?}"
    );
    assert_eq!(blocked.status, 409);
    assert_eq!(file(&world, &first, &first_id), before);
    sign_in
        .cancel_sign_in(&first, &session.state)
        .await
        .unwrap();

    // The same seat can be added and reconnected independently in another profile.
    let session = authorize(
        &world,
        &second,
        claude(ORGANIZATION_A, "another-token"),
        None,
    )
    .await;
    assert_eq!(review(&world, &second, &session).await.0, OAuthAction::Add);
    let other = confirm(&world, &second, &session).await;
    assert_ne!(other.id, first_id);
    assert!(!other.disabled);
    let session = authorize(
        &world,
        &second,
        claude(ORGANIZATION_A, "reconnect-second"),
        Some(&other.id),
    )
    .await;
    assert_eq!(confirm(&world, &second, &session).await.id, other.id);
    let duplicate = world
        .cores
        .import(
            &second,
            claude(ORGANIZATION_A, "independent-token")
                .as_object()
                .unwrap()
                .clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (duplicate.message.as_str(), duplicate.status),
        (
            "This subscription is already connected to Second profile. Use its existing connection instead of importing another copy.",
            409
        )
    );
    let imported = world
        .cores
        .import(
            &second,
            claude(ORGANIZATION_B, "imported")
                .as_object()
                .unwrap()
                .clone(),
        )
        .await
        .unwrap();
    assert_eq!(remaining(&imported.quota.unwrap()), Some(75.0));
    assert_eq!(file(&world, &first, &first_id), before);
    assert_eq!(accounts.accounts(&first).await.unwrap().len(), 2);
    assert_eq!(accounts.accounts(&second).await.unwrap().len(), 2);

    // Provider verification can be retried without signing in again.
    let session = authorize(&world, &first, claude(ORGANIZATION_B, "retry-token"), None).await;
    let answer = world.fetch.count();
    world
        .fetch
        .answer(|_| Ok(response(503, "synthetic private provider body")));
    let failed = sign_in
        .sign_in_status(&first, &session.state)
        .await
        .unwrap_err();
    assert!(failed.message.contains("HTTP 503"), "{failed:?}");
    assert!(!failed.message.contains("private provider body"));
    assert!(sign_in.sign_in_session(&first).is_some());
    world.fetch.answer(move |_| {
        Ok(response(
            200,
            json!({ "account": { "uuid": ACCOUNT }, "organization": { "uuid": ORGANIZATION_B } })
                .to_string(),
        ))
    });
    assert_eq!(
        review(&world, &first, &session).await.0,
        OAuthAction::Reconnect
    );
    assert_eq!(
        world.fetch.count(),
        answer + 2,
        "only the verification was asked again"
    );
    sign_in
        .cancel_sign_in(&first, &session.state)
        .await
        .unwrap();

    // Cancellation and expiry reject late callbacks and leave no staging store.
    let session = authorize(&world, &first, claude(ORGANIZATION_B, "cancelled"), None).await;
    sign_in
        .cancel_sign_in(&first, &session.state)
        .await
        .unwrap();
    let late = sign_in
        .sign_in_callback(
            &first,
            &format!("http://localhost/?state={}", session.state),
        )
        .await
        .unwrap_err();
    assert_eq!(late.status, 404);
    assert_eq!(world.pending().len(), 0);
    let expired = sign_in
        .begin_sign_in(&first, OAuthProvider::Claude, None)
        .await
        .unwrap();
    skew(3_600_000);
    let refused = sign_in
        .confirm_sign_in(&first, &expired.state)
        .await
        .unwrap_err();
    assert_eq!(
        (refused.message.as_str(), refused.status),
        ("This sign-in expired. Start a new sign-in.", 410)
    );
    let next = sign_in
        .begin_sign_in(&first, OAuthProvider::Claude, None)
        .await
        .unwrap();
    skew(0);
    sign_in.cancel_sign_in(&first, &next.state).await.unwrap();
    assert_eq!(world.pending().len(), 0);

    // Seat metadata persists across a restart without renaming.
    world.cores.pool().stop(&first).await.unwrap();
    world.cores.pool().start(&first, false).await.unwrap();
    let listed = accounts.accounts(&first).await.unwrap();
    let ids: Vec<&str> = listed.iter().map(|account| account.id.as_str()).collect();
    assert!(ids.contains(&first_id.as_str()) && ids.contains(&second_id.as_str()));
    let reread = listed
        .iter()
        .find(|account| account.id == second_id)
        .unwrap();
    assert_eq!(reread.organization_uuid.as_deref(), Some(ORGANIZATION_B));

    // Shutdown cleans up a pending sign-in even after its core has stopped.
    world.cores.pool().start(&second, false).await.unwrap();
    sign_in
        .begin_sign_in(&first, OAuthProvider::Codex, None)
        .await
        .unwrap();
    world.cores.pool().stop(&first).await.unwrap();
    world.cores.shutdown().await.unwrap();
    assert_eq!(sign_in.sign_in_session(&first), None);
    assert!(!world.cores.pool().state().alive(&second));
    assert_eq!(names(&world.data().join("oauth-pending")).len(), 0);
}
