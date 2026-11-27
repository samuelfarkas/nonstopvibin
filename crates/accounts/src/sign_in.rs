//! Browser sign-in through a core (`src/server/sign-in.ts`), one at a time across all profiles.
//!
//! - The core makes the sign-in link (`/<provider>-auth-url`) and holds the pending
//!   authorization. The browser comes back to a loopback listener on the port the link's
//!   redirect names ([`nsv_core::upstream::SIGN_IN_PORTS`]), which forwards a callback with the
//!   session's path and state to the core. A headless server's user pastes the redirect URL
//!   instead ([`SignIn::callback`]).
//! - Actions (begin, status, cancel, callback, expiry) run one at a time, each to its
//!   end. A pending sign-in keeps its profile's core from stopping for being idle, and core
//!   updates refuse to restart cores under it.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use hyper::Method as HttpMethod;
use nsv_core::pool::Pool;
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::js::{encode_uri_component, number, text};
use nsv_domain::registry::OAuthProvider;
use nsv_domain::sync::locked;
use nsv_domain::wire::{OAuthSession, OAuthStatus};
use nsv_net::Method;
use nsv_store::Store;
use serde_json::json;
use tokio::sync::{Mutex, OwnedMutexGuard, watch};
use tokio::task::AbortHandle;
use url::Url;

use crate::accounts::Accounts;
use crate::callback::{Callback, Handler, Reply};
use crate::quota::Quotas;
use crate::source::PoolSource;
use crate::task::{Report, detached};

const NOT_FOUND: &str = "Sign-in session not found for this profile.";
const DID_NOT_COMPLETE: &str = "Sign-in did not complete. Start a new sign-in.";
/// How long a sign-in link lives when the core does not say.
const DEFAULT_EXPIRY_SECONDS: f64 = 300.0;

const RECEIVED: &str = concat!(
    "<!doctype html><title>nonstopvibin</title><body style=\"font:16px system-ui;padding:64px;",
    "background:#fbfcf9;color:#275b42\"><h1>Sign-in received.</h1><p>Return to nonstopvibin to ",
    "finish connecting your subscription. You can close this tab.</p>"
);
const MALFORMED: Reply = Reply {
    status: 400,
    headers: &[("cache-control", "no-store")],
    body: "Malformed sign-in callback.",
};
const MISMATCH: Reply = Reply {
    status: 400,
    headers: &[],
    body: "This sign-in callback does not match the pending session.",
};
const INCOMPLETE: Reply = Reply {
    status: 400,
    headers: &[],
    body: "Sign-in could not be completed. Return to nonstopvibin and start again.",
};
const ACCEPTED: Reply = Reply {
    status: 200,
    headers: &[
        ("content-type", "text/html; charset=utf-8"),
        ("cache-control", "no-store"),
        (
            "content-security-policy",
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
        ),
    ],
    body: RECEIVED,
};

pub struct SignInOptions {
    /// Milliseconds since the epoch; sessions expire by it.
    pub clock: fn() -> i64,
    pub report: Report,
    /// A browser callback was accepted: the desktop shell brings an open main window forward.
    pub accepted: Arc<dyn Fn() + Send + Sync>,
}

/// Browser sign-in. Clones share the one sign-in in progress.
pub struct SignIn(Arc<Inner>);

impl Clone for SignIn {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

struct Inner {
    store: Store,
    pool: Pool,
    accounts: Arc<Accounts>,
    quotas: Arc<Quotas<PoolSource>>,
    options: SignInOptions,
    /// The action queue, and what only actions change.
    state: Arc<Mutex<State>>,
    /// The session in progress, readable without waiting for an action.
    session: watch::Sender<Option<OAuthSession>>,
    /// The listener for the provider's redirect, closed as soon as it is done with.
    callback: std::sync::Mutex<Option<Callback>>,
    callbacks: AtomicU64,
}

#[derive(Default)]
struct State {
    expiry: Option<AbortHandle>,
    /// Sessions begun; names the one an expiry timer belongs to.
    begun: u64,
}

/// The core's view of a pending authorization.
enum Authorization {
    Ok,
    Error,
    Wait,
}

fn state_of(url: &Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
}

impl SignIn {
    #[must_use]
    pub fn new(
        store: Store,
        pool: Pool,
        accounts: Arc<Accounts>,
        quotas: Arc<Quotas<PoolSource>>,
        options: SignInOptions,
    ) -> Self {
        Self(Arc::new(Inner {
            store,
            pool,
            accounts,
            quotas,
            options,
            state: Arc::new(Mutex::new(State::default())),
            session: watch::Sender::new(None),
            callback: std::sync::Mutex::new(None),
            callbacks: AtomicU64::new(0),
        }))
    }

    /// `GET …/oauth-session`: the sign-in in progress for this profile.
    #[must_use]
    pub fn session(&self, profile: &ProfileId) -> Option<OAuthSession> {
        self.0
            .session
            .borrow()
            .as_ref()
            .filter(|session| session.profile_id == *profile)
            .cloned()
    }

    /// The session in progress as it changes.
    #[must_use]
    pub fn sessions(&self) -> watch::Receiver<Option<OAuthSession>> {
        self.0.session.subscribe()
    }

    /// Whether a sign-in is pending in this profile's core: it must not stop for being idle.
    #[must_use]
    pub fn signing_in(&self, profile: &ProfileId) -> bool {
        self.0
            .session
            .borrow()
            .as_ref()
            .is_some_and(|session| session.profile_id == *profile)
    }

    /// The port the callback listener is bound to, while one is open.
    #[must_use]
    pub fn callback_port(&self) -> Option<u16> {
        locked(&self.0.callback).as_ref().map(Callback::port)
    }

    /// `POST …/oauth`: starts a sign-in to `provider` for the profile.
    ///
    /// # Errors
    /// 503 during shutdown, 404 for an unknown profile, 409 while another sign-in is pending or when the callback port is taken,
    /// 502 when the core gives no usable sign-in link, or the core's error.
    pub async fn begin(
        &self,
        profile: &ProfileId,
        provider: OAuthProvider,
    ) -> Result<OAuthSession, AppError> {
        let profile = profile.clone();
        self.0
            .run(move |inner, mut state| async move {
                inner.begin(&mut state, profile, provider).await
            })
            .await
    }

    /// `GET …/oauth`: whether the browser finished.
    ///
    /// # Errors
    /// 404 for another profile's or an unknown session, 410 for an expired one, or the failure
    /// of a step, which a later call retries.
    pub async fn status(&self, profile: &ProfileId, state: &str) -> Result<OAuthStatus, AppError> {
        let (profile, key) = (profile.clone(), state.to_owned());
        self.0
            .run(move |inner, mut state| async move {
                inner.status(&mut state, &profile, &key).await
            })
            .await
    }

    /// `DELETE …/oauth`: ends the sign-in and its pending authorization.
    ///
    /// # Errors
    /// As [`SignIn::status`], or what cleaning up failed at.
    pub async fn cancel(&self, profile: &ProfileId, state: &str) -> Result<(), AppError> {
        let (profile, key) = (profile.clone(), state.to_owned());
        self.0
            .run(move |inner, mut state| async move {
                inner.current(&profile, &key)?;
                inner.clear(&mut state).await
            })
            .await
    }

    /// `POST …/callback`, and the callback listener: hands the provider's redirect URL to the
    /// core that holds the authorization.
    ///
    /// # Errors
    /// 400 for text that is not a URL, as [`SignIn::status`] for its state, 404 once the
    /// authorization finished, or the core's error.
    pub async fn callback(&self, profile: &ProfileId, redirect: &str) -> Result<(), AppError> {
        let (profile, redirect) = (profile.clone(), redirect.to_owned());
        self.0
            .run(
                // The state stays held until the core has the callback.
                move |inner, state| async move {
                    let forwarded = inner.forward(&profile, &redirect).await;
                    drop(state);
                    forwarded
                },
            )
            .await
    }

    /// Ends the sign-in in progress, if any; shutdown calls it after the actions settle.
    ///
    /// # Errors
    /// What cleaning up failed at.
    pub async fn clear(&self) -> Result<(), AppError> {
        self.0
            .run(|inner, mut state| async move { inner.clear(&mut state).await })
            .await
    }

    /// Waits for the actions queued so far.
    pub async fn settled(&self) {
        drop(self.0.state.lock().await);
    }

    /// Runs `action` between sign-in actions, and only while no sign-in is pending: one in a
    /// profile's core would not survive that core restarting. An expired one is ended first.
    ///
    /// # Errors
    /// 409 "Finish or cancel the sign-in first." while one is pending, or `action`'s error.
    pub async fn while_idle<T: Send + 'static>(
        &self,
        action: impl Future<Output = Result<T, AppError>> + Send + 'static,
    ) -> Result<T, AppError> {
        self.0
            .run(move |inner, mut state| async move {
                let pending = inner.session.borrow().clone();
                if let Some(session) = pending {
                    if (inner.options.clock)() < session.expires_at {
                        return Err(AppError::new("Finish or cancel the sign-in first.", 409));
                    }
                    // Ended while the core that holds it still runs.
                    inner.clear(&mut state).await?;
                }
                action.await
            })
            .await
    }
}

impl Inner {
    /// Runs `action` after the actions queued before it, holding the state.
    async fn run<T, A>(
        self: &Arc<Self>,
        action: impl FnOnce(Arc<Self>, OwnedMutexGuard<State>) -> A,
    ) -> Result<T, AppError>
    where
        T: Send + 'static,
        A: Future<Output = Result<T, AppError>> + Send + 'static,
    {
        let state = Arc::clone(&self.state).lock_owned().await;
        detached((), action(Arc::clone(self), state)).await
    }

    /// The pending session for `profile` with `state`.
    fn current(&self, profile: &ProfileId, state: &str) -> Result<OAuthSession, AppError> {
        let session = self
            .session
            .borrow()
            .clone()
            .filter(|session| session.profile_id == *profile && session.state == state)
            .ok_or_else(|| AppError::new(NOT_FOUND, 404))?;
        if (self.options.clock)() >= session.expires_at {
            return Err(AppError::new(
                "This sign-in expired. Start a new sign-in.",
                410,
            ));
        }
        Ok(session)
    }

    /// Closes the callback listener, or only listener `only`; draining lets the request in
    /// flight finish.
    fn close_callback(&self, only: Option<u64>, drain: bool) {
        let closed = {
            let mut callback = locked(&self.callback);
            callback.take_if(|callback| only.is_none_or(|id| callback.id() == id))
        };
        if let Some(callback) = closed {
            if drain {
                callback.drain();
            } else {
                drop(callback);
            }
        }
    }

    async fn begin(
        self: &Arc<Self>,
        state: &mut State,
        profile: ProfileId,
        provider: OAuthProvider,
    ) -> Result<OAuthSession, AppError> {
        if self.pool.state().closing {
            return Err(AppError::shutting_down());
        }
        let id = profile.clone();
        self.store.call(move |db| db.profile(&id)).await?;
        let pending = self.session.borrow().clone();
        if pending.is_some_and(|session| (self.options.clock)() < session.expires_at) {
            return Err(AppError::new(
                "Finish or cancel the current sign-in before starting another.",
                409,
            ));
        }
        self.clear(state).await?;
        let session = self.open(provider, &profile).await?;
        state.begun += 1;
        self.session.send_replace(Some(session.clone()));
        self.expire_later(state, session.expires_at);
        Ok(session)
    }

    /// Ends the session begun as `state.begun` once it expires.
    fn expire_later(self: &Arc<Self>, state: &mut State, expires_at: i64) {
        let left = expires_at.saturating_sub((self.options.clock)()).max(1);
        let delay = Duration::from_millis(left.unsigned_abs());
        let (weak, begun) = (Arc::downgrade(self), state.begun);
        let timer = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let Some(inner) = Weak::upgrade(&weak) else {
                return;
            };
            // An action of its own reports, so a clear that aborts this timer loses nothing.
            let _expired = inner
                .run(move |inner, mut state| async move {
                    let pending = inner.session.borrow().is_some();
                    if state.begun != begun || !pending {
                        return Ok(());
                    }
                    let cleared = inner.clear(&mut state).await;
                    if cleared.is_err() {
                        (inner.options.report)(
                            "Could not clean up an expired sign-in. Restart the app.".to_owned(),
                        );
                    }
                    cleared
                })
                .await;
        });
        state.expiry = Some(timer.abort_handle());
    }

    /// Asks the profile's core for a sign-in link and listens for the browser's return.
    async fn open(
        self: &Arc<Self>,
        provider: OAuthProvider,
        profile: &ProfileId,
    ) -> Result<OAuthSession, AppError> {
        let pool = &self.pool;
        pool.start(profile, false).await?;
        let path = format!("/{}-auth-url", provider.as_str());
        let response = pool.management(profile, &Method::GET, &path, None).await?;
        let no_link = || AppError::new("The provider did not return a sign-in link.", 502);
        let (Some(link), Some(key)) = (
            text(response.get("url")).map(str::to_owned),
            text(response.get("state")).map(str::to_owned),
        ) else {
            return Err(no_link());
        };
        let now = (self.options.clock)();
        let seconds = number(response.get("expires_in"))
            .unwrap_or(DEFAULT_EXPIRY_SECONDS)
            .max(1.0);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "at least one second; a lifetime beyond i64 milliseconds saturates"
        )]
        let lifetime = (seconds * 1000.0) as i64;
        let session = OAuthSession {
            profile_id: profile.clone(),
            provider,
            url: link.clone(),
            state: key,
            started_at: now,
            user_code: text(response.get("user_code")).map(str::to_owned),
            expires_at: now.saturating_add(lifetime),
        };
        let link = Url::parse(&link).map_err(|_| no_link())?;
        let Some(redirect) = link
            .query_pairs()
            .find(|(key, _)| key == "redirect_uri")
            .map(|(_, value)| value.into_owned())
            .filter(|redirect| !redirect.is_empty())
        else {
            return Ok(session);
        };
        let unsupported = || {
            AppError::new(
                "The provider returned an unsupported callback address.",
                502,
            )
        };
        let callback = Url::parse(&redirect).map_err(|_| unsupported())?;
        let port = callback
            .port()
            .filter(|_| {
                callback.scheme() == "http"
                    && matches!(callback.host_str(), Some("localhost" | "127.0.0.1"))
            })
            .ok_or_else(unsupported)?;
        self.close_callback(None, false);
        let id = self.callbacks.fetch_add(1, Ordering::Relaxed);
        let handler = self.handler(id, &callback, &session)?;
        let Ok(listening) = Callback::listen(id, port, handler).await else {
            let path = format!(
                "/oauth-session?state={}",
                encode_uri_component(&session.state)
            );
            pool.management(profile, &Method::DELETE, &path, None)
                .await?;
            return Err(AppError::new(
                format!(
                    "Sign-in port {port} is in use. Close any pending sign-in in another app, then try again."
                ),
                409,
            ));
        };
        *locked(&self.callback) = Some(listening);
        Ok(session)
    }

    /// Answers the browser's return to the callback listener `id`: a GET on the redirect's
    /// path with the session's state is forwarded to the core.
    fn handler(
        self: &Arc<Self>,
        id: u64,
        callback: &Url,
        session: &OAuthSession,
    ) -> Result<Handler, AppError> {
        let base = Url::parse(&callback.origin().ascii_serialization()).map_err(|_| {
            AppError::new(
                "The provider returned an unsupported callback address.",
                502,
            )
        })?;
        let path = callback.path().to_owned();
        let (key, profile) = (session.state.clone(), session.profile_id.clone());
        let weak = Arc::downgrade(self);
        Ok(Arc::new(move |method: HttpMethod, target: String| {
            let (base, path, key, profile, weak) = (
                base.clone(),
                path.clone(),
                key.clone(),
                profile.clone(),
                Weak::clone(&weak),
            );
            Box::pin(async move {
                let Ok(incoming) = base.join(&target) else {
                    return MALFORMED;
                };
                if method != HttpMethod::GET
                    || incoming.path() != path
                    || state_of(&incoming).as_deref() != Some(key.as_str())
                {
                    return MISMATCH;
                }
                let Some(inner) = weak.upgrade() else {
                    return INCOMPLETE;
                };
                if SignIn(Arc::clone(&inner))
                    .callback(&profile, incoming.as_str())
                    .await
                    .is_err()
                {
                    return INCOMPLETE;
                }
                inner.close_callback(Some(id), true);
                (inner.options.accepted)();
                ACCEPTED
            })
        }))
    }

    /// Ends the sign-in in progress and its pending authorization in the profile's core.
    async fn clear(&self, state: &mut State) -> Result<(), AppError> {
        if let Some(expiry) = state.expiry.take() {
            expiry.abort();
        }
        let pending = self.session.borrow().clone();
        if let Some(session) = pending {
            let path = format!(
                "/oauth-session?state={}",
                encode_uri_component(&session.state)
            );
            let pool = &self.pool;
            if pool
                .management(&session.profile_id, &Method::DELETE, &path, None)
                .await
                .is_err()
            {
                // A stopped or unreachable core cannot acknowledge the cancellation; stopping
                // it keeps it from completing this authorization after cleanup.
                pool.stop(&session.profile_id).await?;
            }
        }
        self.close_callback(None, false);
        self.session.send_replace(None);
        Ok(())
    }

    async fn authorization(
        pool: &Pool,
        core: &ProfileId,
        state: &str,
    ) -> Result<Authorization, AppError> {
        let path = format!("/get-auth-status?state={}", encode_uri_component(state));
        let result = pool.management(core, &Method::GET, &path, None).await?;
        Ok(match text(result.get("status")) {
            Some("ok") => Authorization::Ok,
            Some("error") => Authorization::Error,
            _ => Authorization::Wait,
        })
    }

    async fn status(
        &self,
        state: &mut State,
        profile: &ProfileId,
        key: &str,
    ) -> Result<OAuthStatus, AppError> {
        let session = self.current(profile, key)?;
        let failed = || OAuthStatus::Error {
            error: DID_NOT_COMPLETE.to_owned(),
        };
        let pool = &self.pool;
        let authorization = Self::authorization(pool, profile, key).await?;
        if matches!(authorization, Authorization::Wait) {
            return Ok(OAuthStatus::Wait);
        }
        if let Some(expiry) = state.expiry.take() {
            expiry.abort();
        }
        self.session.send_replace(None);
        self.close_callback(None, false);
        if matches!(authorization, Authorization::Error) {
            self.accounts.sync(pool, profile).await?;
            return Ok(failed());
        }
        let provider = session.provider.as_str();
        self.quotas
            .refresh_accounts(profile, |account| {
                !account.disabled && account.provider == provider
            })
            .await?;
        Ok(OAuthStatus::Ok)
    }

    async fn forward(&self, profile: &ProfileId, redirect: &str) -> Result<(), AppError> {
        let url = Url::parse(redirect).map_err(|_| AppError::new(MALFORMED.body, 400))?;
        let session = self.current(profile, &state_of(&url).unwrap_or_default())?;
        let body = json!({ "provider": session.provider.as_str(), "redirect_url": redirect });
        self.pool
            .management(profile, &Method::POST, "/oauth-callback", Some(&body))
            .await?;
        Ok(())
    }
}
