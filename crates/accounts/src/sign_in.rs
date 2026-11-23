//! Browser sign-in through a core (`src/server/sign-in.ts`), one at a time across all profiles.
//!
//! - The core makes the sign-in link (`/<provider>-auth-url`) and holds the pending
//!   authorization. The browser comes back to a loopback listener on the port the link's
//!   redirect names ([`nsv_core::upstream::SIGN_IN_PORTS`]), which forwards a callback with the
//!   session's path and state to the core. A headless server's user pastes the redirect URL
//!   instead ([`SignIn::callback`]).
//! - A provider with organizations (Claude) signs in on a throwaway store under
//!   `oauth-pending/` with a core of its own, recorded in that store's ledger. Its credential
//!   enters the profile only after Claude names the seat and the user confirms the review.
//! - Actions (begin, status, confirm, cancel, callback, expiry) run one at a time, each to its
//!   end. A pending sign-in keeps its profile's core from stopping for being idle, and core
//!   updates refuse to restart cores under it.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use hyper::Method as HttpMethod;
use nsv_core::pool::{Backend, MANAGEMENT_TIMEOUT, NoHooks, Pool};
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::js::{encode_uri_component, number, text};
use nsv_domain::registry::{OAuthProvider, Provider, registry};
use nsv_domain::sync::locked;
use nsv_domain::wire::{
    Account, AccountKind, AccountStatus, Extra, OAuthReview, OAuthSession, OAuthStatus,
};
use nsv_files::{DataDir, owner_only_directory, unique_directory};
use nsv_net::Method;
use nsv_store::Store;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, OwnedMutexGuard, watch};
use tokio::task::AbortHandle;
use url::Url;

use crate::accounts::{Accounts, PoolSource, credential_path, read_credential};
use crate::callback::{Callback, Handler, Reply};
use crate::credentials::{Credentials, SeatMatch, detached};
use crate::identity::{claude_identity, verify_claude_identity};
use crate::quota::Quotas;
use crate::scheduler::Scheduler;

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

/// Launches a data directory's cores: `Processes::new(directory)` in the daemon, so a throwaway
/// store's cores are recorded in its own ledger.
pub type Backends = Arc<dyn Fn(&DataDir) -> Arc<dyn Backend> + Send + Sync>;
/// Adds a message to the app's error list.
pub type Report = Arc<dyn Fn(String) + Send + Sync>;

pub struct SignInOptions {
    pub backends: Backends,
    /// How long a throwaway store's core gets to answer ([`nsv_core::pool::READY`]).
    pub ready: Duration,
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
    scheduler: Scheduler,
    accounts: Arc<Accounts>,
    credentials: Credentials,
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
    sandbox: Option<Sandbox>,
    expiry: Option<AbortHandle>,
    /// Sessions begun; names the one an expiry timer belongs to.
    begun: u64,
}

/// A sign-in for a provider with organizations: a throwaway store with its own core, so the
/// credential reaches a profile only after its seat is reviewed.
struct Sandbox {
    provider: OAuthProvider,
    directory: DataDir,
    store: Store,
    pool: Pool,
    accounts: Accounts,
    profile: ProfileId,
    progress: Progress,
}

/// How far a sandboxed sign-in got; each step is kept, so a failed later step is retried
/// without signing in again.
enum Progress {
    /// The browser has not finished signing in.
    Waiting,
    /// The core holds the credential; its account is not read yet.
    Authorized,
    /// The core lists this one account.
    Listed(String),
    /// Its credential, read with the core stopped, awaits the provider's verification.
    Read(Map<String, Value>),
    /// Verified and matched against the profile's seats.
    Reviewed {
        credential: Map<String, Value>,
        review: Box<OAuthReview>,
    },
}

/// The core's view of a pending authorization.
enum Authorization {
    Ok,
    Error,
    Wait,
}

/// What the provider's core calls it in its management paths.
fn core_name(provider: OAuthProvider) -> String {
    registry()
        .providers
        .get(Provider::from(provider))
        .sign_in
        .as_ref()
        .and_then(|sign_in| sign_in.core.clone())
        .unwrap_or_else(|| provider.as_str().to_owned())
}

fn state_of(url: &Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> Result<T, AppError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(AppError::internal)?
        .map_err(AppError::internal)
}

/// Deletes a directory tree; a missing one is no error.
async fn remove_tree(directory: PathBuf) -> Result<(), AppError> {
    blocking(move || match std::fs::remove_dir_all(&directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    })
    .await
}

impl SignIn {
    #[must_use]
    pub fn new(
        store: Store,
        scheduler: Scheduler,
        accounts: Arc<Accounts>,
        credentials: Credentials,
        quotas: Arc<Quotas<PoolSource>>,
        options: SignInOptions,
    ) -> Self {
        Self(Arc::new(Inner {
            store,
            scheduler,
            accounts,
            credentials,
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

    /// `POST …/oauth`: starts a sign-in to `provider` for the profile, or one that replaces
    /// `reconnect`'s credential.
    ///
    /// # Errors
    /// 503 during shutdown, 404 for an unknown profile or an account that cannot be
    /// reconnected, 409 while another sign-in is pending or when the callback port is taken,
    /// 502 when the core gives no usable sign-in link, or the core's error.
    pub async fn begin(
        &self,
        profile: &ProfileId,
        provider: OAuthProvider,
        reconnect: Option<AccountId>,
    ) -> Result<OAuthSession, AppError> {
        let profile = profile.clone();
        self.0
            .run(move |inner, mut state| async move {
                inner.begin(&mut state, profile, provider, reconnect).await
            })
            .await
    }

    /// `GET …/oauth`: whether the browser finished, and for a provider with organizations the
    /// review of the seat it signed in as.
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

    /// `POST …/oauth-confirm`: connects the reviewed seat, adding it or replacing the account it
    /// matches, then ends the sign-in.
    ///
    /// # Errors
    /// As [`SignIn::status`], 409 before the review or for a blocked seat, or the import's
    /// error; the sign-in stays pending then.
    pub async fn confirm(&self, profile: &ProfileId, state: &str) -> Result<OAuthStatus, AppError> {
        let (profile, key) = (profile.clone(), state.to_owned());
        self.0
            .run(move |inner, mut state| async move {
                inner.confirm(&mut state, &profile, &key).await
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
                move |inner, state| async move { inner.forward(&state, &profile, &redirect).await },
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
                    if inner.now() < session.expires_at {
                        return Err(AppError::new("Finish or cancel the sign-in first.", 409));
                    }
                    // Ended while the core that holds it still runs.
                    inner.clear(&mut state).await?;
                }
                action.await
            })
            .await
    }

    /// At startup, before any core runs: sign-ins pending when the app last stopped never
    /// resume, and core update checks never finish. Their cores are reaped and their stores
    /// deleted.
    ///
    /// # Errors
    /// A ledger or file-system error.
    pub async fn discard_pending(data: &DataDir, backends: &Backends) -> Result<(), AppError> {
        let pending = data.oauth_pending();
        let listed = pending.clone();
        let entries = blocking(move || match std::fs::read_dir(&listed) {
            Ok(listing) => listing
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<io::Result<Vec<PathBuf>>>(),
            Err(_) => Ok(Vec::new()),
        })
        .await?;
        for entry in entries {
            backends(&DataDir::new(entry))
                .reap_orphans()
                .await
                .map_err(AppError::internal)?;
        }
        remove_tree(pending).await
    }
}

impl Inner {
    fn pool(&self) -> &Pool {
        self.scheduler.pool()
    }

    fn now(&self) -> u64 {
        u64::try_from((self.options.clock)()).unwrap_or(0)
    }

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
        if self.now() >= session.expires_at {
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
        reconnect: Option<AccountId>,
    ) -> Result<OAuthSession, AppError> {
        if self.pool().closing() {
            return Err(AppError::shutting_down());
        }
        let id = profile.clone();
        self.store.call(move |db| db.profile(&id)).await?;
        let pending = self.session.borrow().clone();
        if pending.is_some_and(|session| self.now() < session.expires_at) {
            return Err(AppError::new(
                "Finish or cancel the current sign-in before starting another.",
                409,
            ));
        }
        self.clear(state).await?;
        let entry = registry().providers.get(Provider::from(provider));
        if let Some(account) = &reconnect {
            let listed = self
                .accounts
                .list(&profile)
                .await?
                .into_iter()
                .find(|listed| listed.id == *account);
            if !entry.reconnect
                || !listed.is_some_and(|listed| {
                    listed.provider == provider.as_str() && listed.kind == AccountKind::OAuth
                })
            {
                return Err(AppError::new(
                    format!(
                        "Choose a {} subscription in this profile to reconnect.",
                        registry().label(provider.as_str())
                    ),
                    404,
                ));
            }
        }
        let opened = if entry.organizations {
            match self.sandbox(provider).await {
                Ok(sandbox) => {
                    let (pool, core) = (sandbox.pool.clone(), sandbox.profile.clone());
                    state.sandbox = Some(sandbox);
                    self.open(&pool, &core, provider, &profile)
                        .await
                        .map(|session| OAuthSession {
                            reconnect_account_id: reconnect
                                .as_ref()
                                .map(|account| account.as_str().to_owned()),
                            ..session
                        })
                }
                Err(error) => Err(error),
            }
        } else {
            self.open(self.pool(), &profile, provider, &profile).await
        };
        let session = match opened {
            Ok(session) => session,
            Err(error) => {
                self.clear(state).await?;
                return Err(error);
            }
        };
        state.begun += 1;
        self.session.send_replace(Some(session.clone()));
        self.expire_later(state, session.expires_at);
        Ok(session)
    }

    /// Ends the session begun as `state.begun` once it expires.
    fn expire_later(self: &Arc<Self>, state: &mut State, expires_at: u64) {
        let delay = Duration::from_millis(expires_at.saturating_sub(self.now()).max(1));
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

    /// A throwaway store with one profile and a pool of its own, under `oauth-pending/`.
    async fn sandbox(&self, provider: OAuthProvider) -> Result<Sandbox, AppError> {
        let parent = self.store.directory().oauth_pending();
        let prefix = format!("{}-", provider.as_str());
        let directory = DataDir::new(
            blocking(move || {
                owner_only_directory(&parent, || Ok::<(), io::Error>(()))?;
                unique_directory(&parent, &prefix)
            })
            .await?,
        );
        let opened = async {
            let store = Store::open(&directory, self.store.vault().clone()).await?;
            match store
                .call(|db| Ok(db.create_profile("Pending sign-in")?.id))
                .await
            {
                Ok(profile) => Ok((store, profile)),
                Err(error) => {
                    store.close().await;
                    Err(AppError::from(error))
                }
            }
        }
        .await;
        let (store, profile) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                remove_tree(directory.path().to_owned()).await?;
                return Err(error);
            }
        };
        let pool = Pool::new(
            store.clone(),
            self.pool().state().binary.to_path_buf(),
            (self.options.backends)(&directory),
            self.options.ready,
            Arc::new(NoHooks),
        );
        Ok(Sandbox {
            provider,
            directory,
            accounts: Accounts::new(store.clone()),
            store,
            pool,
            profile,
            progress: Progress::Waiting,
        })
    }

    /// Asks `pool`'s core for a sign-in link for `core`, the profile that will hold the
    /// credential, and listens for the browser's return.
    async fn open(
        self: &Arc<Self>,
        pool: &Pool,
        core: &ProfileId,
        provider: OAuthProvider,
        profile: &ProfileId,
    ) -> Result<OAuthSession, AppError> {
        pool.start(core, false).await?;
        let path = format!("/{}-auth-url", core_name(provider));
        let response = pool
            .management(core, &Method::GET, &path, None, MANAGEMENT_TIMEOUT)
            .await?;
        let no_link = || AppError::new("The provider did not return a sign-in link.", 502);
        let (Some(link), Some(key)) = (
            text(response.get("url")).map(str::to_owned),
            text(response.get("state")).map(str::to_owned),
        ) else {
            return Err(no_link());
        };
        let now = self.now();
        let seconds = number(response.get("expires_in"))
            .unwrap_or(DEFAULT_EXPIRY_SECONDS)
            .max(1.0);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "at least one second; a lifetime beyond u64 milliseconds saturates"
        )]
        let lifetime = (seconds * 1000.0) as u64;
        let session = OAuthSession {
            profile_id: profile.clone(),
            provider,
            url: link.clone(),
            state: key,
            started_at: now,
            user_code: text(response.get("user_code")).map(str::to_owned),
            expires_at: now.saturating_add(lifetime),
            reconnect_account_id: None,
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
            pool.management(core, &Method::DELETE, &path, None, MANAGEMENT_TIMEOUT)
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

    /// Ends the sign-in in progress: its sandbox, or its pending authorization in the profile's
    /// core. A step that fails leaves the rest for the next try.
    async fn clear(&self, state: &mut State) -> Result<(), AppError> {
        if let Some(expiry) = state.expiry.take() {
            expiry.abort();
        }
        let pending = self.session.borrow().clone();
        if let Some(sandbox) = &state.sandbox {
            self.close_callback(None, false);
            sandbox.pool.shutdown().await?;
            sandbox.store.close().await;
            remove_tree(sandbox.directory.path().to_owned()).await?;
            state.sandbox = None;
        } else if let Some(session) = pending {
            let path = format!(
                "/oauth-session?state={}",
                encode_uri_component(&session.state)
            );
            let pool = self.pool();
            if pool
                .management(
                    &session.profile_id,
                    &Method::DELETE,
                    &path,
                    None,
                    MANAGEMENT_TIMEOUT,
                )
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
        // Its profile's core may stop for being idle again.
        self.scheduler.catch_up();
        Ok(())
    }

    async fn authorization(
        pool: &Pool,
        core: &ProfileId,
        state: &str,
    ) -> Result<Authorization, AppError> {
        let path = format!("/get-auth-status?state={}", encode_uri_component(state));
        let result = pool
            .management(core, &Method::GET, &path, None, MANAGEMENT_TIMEOUT)
            .await?;
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
        if state.sandbox.is_some() {
            return match self.review(state, profile, &session).await? {
                Some(review) => Ok(OAuthStatus::Review { review }),
                None if state.sandbox.is_none() => Ok(failed()),
                None => Ok(OAuthStatus::Wait),
            };
        }
        let pool = self.pool();
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
            self.scheduler.catch_up();
            return Ok(failed());
        }
        let provider = session.provider.as_str();
        self.quotas
            .refresh_accounts(profile, |account| {
                !account.disabled && account.provider == provider
            })
            .await?;
        self.scheduler.catch_up();
        Ok(OAuthStatus::Ok {
            account: None,
            reconnected: None,
        })
    }

    /// Takes a sandboxed sign-in as far as it goes: `None` while the browser has not finished,
    /// or once it failed and the sign-in ended; else the review. Each step that fails is
    /// retried by the next call.
    async fn review(
        &self,
        state: &mut State,
        profile: &ProfileId,
        session: &OAuthSession,
    ) -> Result<Option<OAuthReview>, AppError> {
        loop {
            let Some(sandbox) = state.sandbox.as_ref() else {
                return Ok(None);
            };
            let next = match &sandbox.progress {
                Progress::Waiting => {
                    match Self::authorization(&sandbox.pool, &sandbox.profile, &session.state)
                        .await?
                    {
                        Authorization::Wait => return Ok(None),
                        Authorization::Error => None,
                        Authorization::Ok => {
                            self.close_callback(None, false);
                            Some(Progress::Authorized)
                        }
                    }
                }
                Progress::Authorized => {
                    sandbox
                        .accounts
                        .sync(&sandbox.pool, &sandbox.profile)
                        .await?;
                    match sandbox.accounts.list(&sandbox.profile).await?.as_slice() {
                        [account] => Some(Progress::Listed(account.id.as_str().to_owned())),
                        _ => {
                            return Err(AppError::new(
                                format!(
                                    "{} did not return one subscription. Sign in again.",
                                    registry().label(sandbox.provider.as_str())
                                ),
                                502,
                            ));
                        }
                    }
                }
                Progress::Listed(id) => {
                    // Its core stops first, so no refresh replaces the token being moved.
                    sandbox.pool.stop(&sandbox.profile).await?;
                    let path = credential_path(&sandbox.directory, &sandbox.profile, id)?;
                    let limit = registry().limits.credential_file_bytes;
                    Some(Progress::Read(
                        tokio::task::spawn_blocking(move || read_credential(&path, limit))
                            .await
                            .map_err(AppError::internal)??,
                    ))
                }
                Progress::Read(credential) => {
                    let credential =
                        verify_claude_identity(self.quotas.fetch(), credential).await?;
                    let review = self
                        .review_of(profile, sandbox.provider, &credential, session)
                        .await?;
                    Some(Progress::Reviewed {
                        credential,
                        review: Box::new(review),
                    })
                }
                Progress::Reviewed { review, .. } => return Ok(Some(OAuthReview::clone(review))),
            };
            match (next, state.sandbox.as_mut()) {
                (Some(next), Some(sandbox)) => sandbox.progress = next,
                // The browser's sign-in failed.
                (None, _) => {
                    self.clear(state).await?;
                    return Ok(None);
                }
                (Some(_), None) => return Ok(None),
            }
        }
    }

    /// The pending account a verified credential shows, and how it matches the profile's seats.
    async fn review_of(
        &self,
        profile: &ProfileId,
        provider: OAuthProvider,
        credential: &Map<String, Value>,
        session: &OAuthSession,
    ) -> Result<OAuthReview, AppError> {
        let identity = claude_identity(&Value::Object(credential.clone()));
        let account = Account {
            id: AccountId::parse("pending").map_err(AppError::internal)?,
            name: registry().label(provider.as_str()),
            account_uuid: identity.account_uuid,
            organization_uuid: identity.organization_uuid,
            organization_name: identity.organization_name,
            provider: provider.as_str().to_owned(),
            kind: AccountKind::OAuth,
            email: text(credential.get("email")).map(str::to_owned),
            disabled: false,
            status: AccountStatus::Ready,
            priority: 0,
            auth_index: None,
            quota: None,
            prefix: None,
            model_count: None,
            extra: Extra::new(),
        };
        let reconnect = reconnect_of(session);
        let (action, message) = self
            .credentials
            .match_seat(profile, provider, credential, reconnect.as_ref())
            .await?
            .review();
        Ok(OAuthReview {
            account,
            action,
            message,
        })
    }

    async fn confirm(
        &self,
        state: &mut State,
        profile: &ProfileId,
        key: &str,
    ) -> Result<OAuthStatus, AppError> {
        let session = self.current(profile, key)?;
        let Some(Sandbox {
            provider,
            progress: Progress::Reviewed { credential, .. },
            ..
        }) = &state.sandbox
        else {
            return Err(AppError::new(
                format!(
                    "Wait for {} to verify the organization before connecting.",
                    registry().label(session.provider.as_str())
                ),
                409,
            ));
        };
        let (credentials, credential, provider, id, reconnect) = (
            self.credentials.clone(),
            credential.clone(),
            *provider,
            profile.clone(),
            reconnect_of(&session),
        );
        let connected = self
            .credentials
            .queue(async move {
                let matched = credentials
                    .match_seat(&id, provider, &credential, reconnect.as_ref())
                    .await?;
                let replaced = match matched {
                    SeatMatch::Blocked(message) => return Err(AppError::new(message, 409)),
                    SeatMatch::Add => None,
                    SeatMatch::Reconnect(account) => Some(account),
                };
                let reconnected = replaced.is_some();
                let account = credentials
                    .import_credential(&id, credential, replaced)
                    .await?;
                Ok(OAuthStatus::Ok {
                    account: Some(account),
                    reconnected: Some(reconnected),
                })
            })
            .await?;
        self.clear(state).await?;
        Ok(connected)
    }

    async fn forward(
        &self,
        state: &State,
        profile: &ProfileId,
        redirect: &str,
    ) -> Result<(), AppError> {
        let url = Url::parse(redirect).map_err(|_| AppError::new(MALFORMED.body, 400))?;
        let session = self.current(profile, &state_of(&url).unwrap_or_default())?;
        let (pool, core) = match &state.sandbox {
            // Its core already finished this authorization.
            Some(sandbox) if !matches!(sandbox.progress, Progress::Waiting) => {
                return Err(AppError::new(NOT_FOUND, 404));
            }
            Some(sandbox) => (&sandbox.pool, &sandbox.profile),
            None => (self.pool(), profile),
        };
        let body = json!({ "provider": core_name(session.provider), "redirect_url": redirect });
        pool.management(
            core,
            &Method::POST,
            "/oauth-callback",
            Some(&body),
            MANAGEMENT_TIMEOUT,
        )
        .await?;
        Ok(())
    }
}

fn reconnect_of(session: &OAuthSession) -> Option<AccountId> {
    session
        .reconnect_account_id
        .as_deref()
        .and_then(|account| AccountId::parse(account).ok())
}
