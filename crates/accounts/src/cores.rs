//! The account features around one scheduler and its pool, as `src/server/cores.ts` assembled
//! them: every operation the server's routes, the gateway, the tray and the credential socket
//! ask of them, each profile's status, the scheduler's [`Host`], launch and the shutdown order.
//! An operation that changes a profile or its accounts answers only after the scheduler heard
//! the facts the store published.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nsv_core::install::CoreSource;
use nsv_core::pool::{BoxFuture, Launch, Pool, PoolState};
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::registry::OAuthProvider;
use nsv_domain::version::AppVersion;
use nsv_domain::wire::{
    Account, AccountKind, AccountPatch, ApiAccountInput, CoreState, Model, OAuthSession,
    OAuthStatus, Profile, ProfilePatch, QuotaStatus, ResetCredits, ResetResult, Runtime,
};
use nsv_store::Store;
use serde_json::{Map, Value};
use tokio::sync::watch;

use crate::accounts::Accounts;
use crate::credentials::Credentials;
use crate::fetch::Outside;
use crate::quota::{QuotaError, Quotas};
use crate::resets::Resets;
use crate::sandbox::{self, Backends};
use crate::scheduler::{Host, Scheduler, Timing, Tracked};
use crate::sign_in::{SignIn, SignInOptions};
use crate::source::PoolSource;
use crate::task::Report;
use crate::updates::{CoreUpdates, UpdateOptions};

mod hub;

pub struct CoresOptions {
    pub store: Store,
    /// The bundled core.
    pub binary: PathBuf,
    /// Launches a data directory's cores: `Arc::new(Processes::new(directory))`.
    pub backends: Backends,
    /// How long a new core gets to answer ([`nsv_core::pool::READY`]).
    pub ready: Duration,
    pub timing: Timing,
    /// Provider requests (quota, model discovery, identity checks), the pin feed and release
    /// archives.
    pub outside: Arc<dyn Outside>,
    /// Where the bundled core comes from.
    pub source: CoreSource,
    /// Milliseconds since the epoch: quota check times and sign-in expiry.
    pub clock: fn() -> i64,
    pub report: Report,
    /// A browser sign-in callback was accepted: the desktop shell brings an open main window
    /// forward, never creating one.
    pub sign_in_accepted: Arc<dyn Fn() + Send + Sync>,
    /// This build's version: [`AppVersion::current`].
    pub app: AppVersion,
    /// The daemon's loopback port, where cores reach Claude Agent accounts; `None` where
    /// nothing serves them.
    pub daemon_port: Option<u16>,
}

/// The pool's scheduler and the account features that drive it.
pub struct Cores {
    scheduler: Scheduler,
    pool: Pool,
    accounts: Arc<Accounts>,
    quotas: Arc<Quotas<PoolSource>>,
    credentials: Credentials,
    sign_in: SignIn,
    resets: Resets,
    updates: CoreUpdates,
    store: Store,
    backends: Backends,
}

/// A profile as the API, the tray and the credential socket show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileStatus {
    /// Its core runs, or sleeps until the next request starts it.
    pub serving: bool,
    pub runtime: Runtime,
    /// What it shows once the scheduler stopped starting it, else its core's last failure.
    pub error: Option<Arc<str>>,
    /// A gateway request or `WebSocket` connection is open.
    pub streaming: bool,
}

impl ProfileStatus {
    fn of(pool: &PoolState, tracked: Tracked, profile: &ProfileId) -> Self {
        let (runtime, error) = pool.runtime(profile);
        Self {
            serving: pool.running(profile) || tracked.dormant,
            // Once the scheduler stopped restarting it, the profile says so instead of its last
            // failure.
            error: tracked
                .gave_up
                .filter(|_| runtime != Runtime::Running)
                .or_else(|| error.cloned()),
            runtime,
            streaming: tracked.open,
        }
    }
}

/// What the scheduler asks of the features. It holds no handle to the scheduler, so nothing
/// keeps the scheduler alive in a cycle.
struct FeatureHost {
    report: Report,
    clock: fn() -> i64,
    quotas: Arc<Quotas<PoolSource>>,
    sessions: watch::Receiver<Option<OAuthSession>>,
}

impl Host for FeatureHost {
    fn report(&self, message: String) {
        (self.report)(message);
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn signing_in(&self, profile: &ProfileId) -> bool {
        self.sessions
            .borrow()
            .as_ref()
            .is_some_and(|session| session.profile_id == *profile)
    }

    fn refresh_quotas(
        &self,
        pool: Pool,
        profile: ProfileId,
    ) -> BoxFuture<'static, Result<(), AppError>> {
        let quotas = Arc::clone(&self.quotas);
        Box::pin(async move {
            // Each account is checked as its turn comes; a stop or shutdown ends the pass.
            quotas
                .refresh_accounts(&profile, |account| {
                    pool.state().usable(&profile)
                        && !account.disabled
                        && account
                            .quota
                            .as_ref()
                            .is_none_or(|quota| quota.status != QuotaStatus::Unavailable)
                })
                .await
        })
    }
}

impl Cores {
    /// # Errors
    /// The bundled pin or this platform is unknown, which a build rules out.
    pub fn new(options: CoresOptions) -> Result<Self, AppError> {
        let CoresOptions {
            store,
            binary,
            backends,
            ready,
            timing,
            outside,
            source: core_source,
            clock,
            report,
            sign_in_accepted,
            app,
            daemon_port,
        } = options;
        let accounts = Arc::new(Accounts::new(store.clone()));
        let (scheduler, (source, quotas, credentials, sign_in)) = Scheduler::new(
            store.clone(),
            Launch {
                binary: binary.clone(),
                backend: backends(store.directory()),
                ready,
                daemon: daemon_port,
            },
            Arc::clone(&accounts),
            timing,
            |pool| {
                let source = PoolSource {
                    pool: pool.clone(),
                    accounts: Arc::clone(&accounts),
                };
                let quotas = Arc::new(Quotas::new(
                    store.clone(),
                    source.clone(),
                    Arc::clone(&outside),
                    clock,
                ));
                let credentials = Credentials::new(
                    store.clone(),
                    pool.clone(),
                    Arc::clone(&accounts),
                    Arc::clone(&quotas),
                    Arc::clone(&outside),
                );
                let sign_in = SignIn::new(
                    store.clone(),
                    pool.clone(),
                    Arc::clone(&accounts),
                    credentials.clone(),
                    Arc::clone(&quotas),
                    SignInOptions {
                        backends: Arc::clone(&backends),
                        outside: Arc::clone(&outside),
                        ready,
                        clock,
                        report: Arc::clone(&report),
                        accepted: sign_in_accepted,
                    },
                );
                let host = Arc::new(FeatureHost {
                    report: Arc::clone(&report),
                    clock,
                    quotas: Arc::clone(&quotas),
                    sessions: sign_in.sessions(),
                });
                (host, (source, quotas, credentials, sign_in))
            },
        );
        let pool = scheduler.pool().clone();
        let updates = CoreUpdates::new(
            store.clone(),
            pool.clone(),
            sign_in.clone(),
            credentials.clone(),
            UpdateOptions {
                outside,
                source: core_source,
                bundled: binary,
                backends: Arc::clone(&backends),
                ready,
                report,
                app,
            },
        )?;
        Ok(Self {
            resets: Resets::new(store.clone(), source, Arc::clone(&quotas), clock),
            scheduler,
            pool,
            accounts,
            quotas,
            credentials,
            sign_in,
            updates,
            store,
            backends,
        })
    }

    /// The pool the features drive; tests start, stop and read cores through it.
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    #[must_use]
    pub fn status(&self, profile: &ProfileId) -> ProfileStatus {
        self.statuses(|status| status(profile))
    }

    /// `read` with every profile's status from one look at the pool and the scheduler; `read`
    /// must not call back into the scheduler.
    pub fn statuses<T>(&self, read: impl FnOnce(&dyn Fn(&ProfileId) -> ProfileStatus) -> T) -> T {
        let pool = self.pool.state();
        self.scheduler
            .tracked(|tracked| read(&|profile| ProfileStatus::of(&pool, tracked(profile), profile)))
    }

    /// Changes the pool publishes, and the scheduler's to whether a profile is dormant or has
    /// requests open: the tray follows both.
    #[must_use]
    pub fn changes(&self) -> (watch::Receiver<Arc<PoolState>>, watch::Receiver<()>) {
        (self.pool.subscribe(), self.scheduler.changes())
    }

    /// Gateway activity on the profile: a request opened or closed, and whether any is still
    /// open.
    pub fn activity(&self, profile: &ProfileId, open: bool) {
        self.scheduler.activity(profile, open);
    }

    /// Starts a dormant profile's core and waits for it; see [`Scheduler::wake`].
    ///
    /// # Errors
    /// The start's error.
    pub async fn wake(&self, profile: &ProfileId) -> Result<(), AppError> {
        self.scheduler.wake(profile).await
    }

    /// The port of the profile's usable core.
    ///
    /// # Errors
    /// See [`Pool::port`].
    pub fn port(&self, profile: &ProfileId) -> Result<u16, AppError> {
        self.pool.port(profile)
    }

    /// The models the profile's running core lists.
    ///
    /// # Errors
    /// See [`Pool::models`].
    pub async fn models(&self, profile: &ProfileId) -> Result<Vec<Model>, AppError> {
        self.pool.models(profile).await
    }

    /// The models the profile's core lists; a dormant core starts for them.
    ///
    /// # Errors
    /// The start's or the core's error.
    pub async fn woken_models(&self, profile: &ProfileId) -> Result<Vec<Model>, AppError> {
        self.scheduler.wake(profile).await?;
        self.pool.models(profile).await
    }

    /// A UI read the state; see [`Scheduler::watch`].
    pub async fn watch(&self) {
        self.scheduler.watch().await;
    }

    /// The profile's accounts as the UI lists them.
    ///
    /// # Errors
    /// A store failure.
    pub async fn accounts(&self, profile: &ProfileId) -> Result<Vec<Account>, AppError> {
        self.accounts.list(profile).await
    }

    /// Runs `change` after the credential changes queued before it, so it ends even if its
    /// caller stops waiting; see `Credentials::queue`.
    ///
    /// # Errors
    /// What `change` returns.
    pub async fn after_credential_changes<T: Send + 'static>(
        &self,
        change: impl Future<Output = Result<T, AppError>> + Send + 'static,
    ) -> Result<T, AppError> {
        self.credentials.queue(change).await
    }

    /// `POST /profiles`.
    ///
    /// # Errors
    /// A store failure.
    pub async fn create_profile(&self, name: String) -> Result<Profile, AppError> {
        let profile = self.store.call(move |db| db.create_profile(&name)).await?;
        self.scheduler.catch_up();
        Ok(profile)
    }

    /// `PATCH /profiles/:profileId`: the stored profile changes, then its running core.
    ///
    /// # Errors
    /// 404 for an unknown profile, a store failure, or the core's.
    pub async fn patch_profile(
        &self,
        profile: &ProfileId,
        patch: ProfilePatch,
    ) -> Result<(), AppError> {
        let id = profile.clone();
        self.store
            .call(move |db| {
                let mut stored = db.profile(&id)?;
                patch.apply(&mut stored);
                db.save_profile(&stored)
            })
            .await?;
        self.scheduler.catch_up();
        self.pool.configure(profile).await
    }

    /// `POST /profiles/:profileId/start`.
    ///
    /// # Errors
    /// The start's error.
    pub async fn start(&self, profile: &ProfileId) -> Result<(), AppError> {
        self.pool.start(profile, false).await
    }

    /// The core in use.
    #[must_use]
    pub fn core_version(&self) -> String {
        self.updates.version()
    }

    /// What Settings shows of core updates.
    #[must_use]
    pub fn core_state(&self) -> CoreState {
        self.updates.state()
    }

    /// `POST /core/check`; see `CoreUpdates::check`.
    ///
    /// # Errors
    /// The check's.
    pub async fn check_core(&self) -> Result<(), AppError> {
        self.updates.check().await
    }

    /// `POST /core/install`; see `CoreUpdates::install`.
    ///
    /// # Errors
    /// The install's.
    pub async fn install_core(&self, version: &str) -> Result<(), AppError> {
        self.updates.install(version).await
    }

    /// `DELETE /core/install`; see `CoreUpdates::use_bundled`.
    ///
    /// # Errors
    /// The switch's.
    pub async fn use_bundled_core(&self) -> Result<(), AppError> {
        self.updates.use_bundled().await
    }

    /// `POST …/oauth`; see [`SignIn::begin`].
    ///
    /// # Errors
    /// The sign-in's.
    pub async fn begin_sign_in(
        &self,
        profile: &ProfileId,
        provider: OAuthProvider,
        reconnect: Option<AccountId>,
    ) -> Result<OAuthSession, AppError> {
        if provider == OAuthProvider::Claude {
            self.accounts.refuse_beside_claude_agent(profile).await?;
        }
        self.sign_in.begin(profile, provider, reconnect).await
    }

    /// `GET …/oauth/status`; see [`SignIn::status`]. A sign-in that ended lets its profile's
    /// core stop for being idle again.
    ///
    /// # Errors
    /// The sign-in's.
    pub async fn sign_in_status(
        &self,
        profile: &ProfileId,
        state: &str,
    ) -> Result<OAuthStatus, AppError> {
        let status = self.sign_in.status(profile, state).await?;
        if !matches!(status, OAuthStatus::Wait) {
            self.scheduler.catch_up();
        }
        Ok(status)
    }

    /// `POST …/oauth/confirm`; see [`SignIn::confirm`].
    ///
    /// # Errors
    /// The sign-in's.
    pub async fn confirm_sign_in(
        &self,
        profile: &ProfileId,
        state: &str,
    ) -> Result<OAuthStatus, AppError> {
        self.sign_in.confirm(profile, state).await
    }

    /// `DELETE …/oauth`; see [`SignIn::cancel`]. Its profile's core may stop for being idle
    /// again.
    ///
    /// # Errors
    /// The sign-in's.
    pub async fn cancel_sign_in(&self, profile: &ProfileId, state: &str) -> Result<(), AppError> {
        self.sign_in.cancel(profile, state).await?;
        self.scheduler.catch_up();
        Ok(())
    }

    /// `GET …/oauth-session`.
    #[must_use]
    pub fn sign_in_session(&self, profile: &ProfileId) -> Option<OAuthSession> {
        self.sign_in.session(profile)
    }

    /// The port of the sign-in's callback listener while it listens.
    #[must_use]
    pub fn callback_port(&self) -> Option<u16> {
        self.sign_in.callback_port()
    }

    /// `POST …/oauth/callback`; see [`SignIn::callback`].
    ///
    /// # Errors
    /// The sign-in's.
    pub async fn sign_in_callback(
        &self,
        profile: &ProfileId,
        redirect: &str,
    ) -> Result<(), AppError> {
        self.sign_in.callback(profile, redirect).await
    }

    /// `POST …/credentials`; see `Credentials::import`.
    ///
    /// # Errors
    /// The import's.
    pub async fn import(
        &self,
        profile: &ProfileId,
        raw: Map<String, Value>,
    ) -> Result<Account, AppError> {
        let account = self.credentials.import(profile, raw).await?;
        self.scheduler.catch_up();
        Ok(account)
    }

    /// `POST …/api-accounts`; see `Credentials::add_api_account`.
    ///
    /// # Errors
    /// The change's.
    pub async fn add_api_account(
        &self,
        profile: &ProfileId,
        input: ApiAccountInput,
    ) -> Result<(), AppError> {
        let added = self.credentials.add_api_account(profile, input).await;
        self.scheduler.catch_up();
        added
    }

    /// `POST …/claude-agent`; see `Credentials::add_claude_agent`.
    ///
    /// # Errors
    /// As `Credentials::add_claude_agent`.
    pub async fn add_claude_agent(
        &self,
        profile: &ProfileId,
        name: String,
        models: Vec<String>,
    ) -> Result<(), AppError> {
        let added = self
            .credentials
            .add_claude_agent(profile, name, models)
            .await;
        self.scheduler.catch_up();
        added
    }

    /// `PATCH …/accounts/:accountId`; see `Credentials::set_account`.
    ///
    /// # Errors
    /// The change's.
    pub async fn set_account(
        &self,
        profile: &ProfileId,
        account: &AccountId,
        patch: AccountPatch,
    ) -> Result<(), AppError> {
        self.credentials.set_account(profile, account, patch).await
    }

    /// `DELETE …/accounts/:accountId`; see `Credentials::remove_account`.
    ///
    /// # Errors
    /// The change's.
    pub async fn remove_account(
        &self,
        profile: &ProfileId,
        account: &AccountId,
    ) -> Result<(), AppError> {
        self.credentials.remove_account(profile, account).await?;
        self.scheduler.catch_up();
        Ok(())
    }

    /// `GET …/reset-credits`; see [`Resets::credits`].
    ///
    /// # Errors
    /// The resets'.
    pub async fn reset_credits(
        &self,
        profile: &ProfileId,
        account: &AccountId,
    ) -> Result<ResetCredits, QuotaError> {
        self.resets.credits(profile, account).await
    }

    /// `POST …/reset-credits/consume`; see [`Resets::consume`].
    ///
    /// # Errors
    /// The resets'.
    pub async fn consume_reset(
        &self,
        profile: &ProfileId,
        account: &AccountId,
        credit_id: &str,
    ) -> Result<ResetResult, QuotaError> {
        self.resets.consume(profile, account, credit_id).await
    }

    /// At startup: profiles start from the core `CoreUpdates::select` picks. Cores a dead
    /// earlier instance left are stopped (the profiles' and the pending sign-ins'), pending
    /// sign-ins and core checks are deleted, the profiles start and scheduling begins. An installed build then checks for a core update
    /// in the background; offline stays quiet, as Settings checks again.
    ///
    /// # Errors
    /// A bundled core that is missing or fails its check, a ledger or file-system error, or
    /// the scheduler's.
    pub async fn launch(&self) -> Result<(), AppError> {
        self.updates.select().await?;
        // Orphans first: if the pending sign-ins cannot be removed, startup fails without
        // leaving one running.
        self.pool.reap_orphans().await.map_err(AppError::internal)?;
        sandbox::discard_all(self.store.directory(), &self.backends).await?;
        self.scheduler.launch().await?;
        if self.updates.enabled() {
            let updates = self.updates.clone();
            tokio::spawn(async move {
                // Offline at launch stays quiet: Settings checks again when it opens.
                let _checked = updates.check().await;
            });
        }
        Ok(())
    }

    /// `POST /profiles/:profileId/refresh`: a profile the app keeps running starts first,
    /// since signed-in quota is asked through its core; then the accounts are read again and
    /// each enabled one's quota checked. API-key quota is checked even when the core cannot
    /// start; the start's failure is answered after.
    ///
    /// # Errors
    /// 404 for an unknown profile, the start's error, or the checks'.
    pub async fn refresh(&self, profile: &ProfileId) -> Result<(), AppError> {
        let id = profile.clone();
        self.store.call(move |db| db.profile(&id)).await?;
        let pool = &self.pool;
        let keeps_running = self
            .store
            .facts()
            .borrow()
            .iter()
            .any(|fact| fact.id == *profile && fact.kept());
        let failure = if keeps_running {
            pool.start(profile, false).await.err()
        } else {
            None
        };
        // Once shutdown began nothing new may begin: its work waits for no provider request.
        if let Some(failure) = failure.as_ref().filter(|_| pool.state().closing) {
            return Err(failure.clone());
        }
        // A core that is not running could only record a failed check.
        self.quotas
            .refresh_accounts(profile, |account| {
                !account.disabled
                    && (account.kind == AccountKind::ApiKey || pool.state().usable(profile))
            })
            .await?;
        failure.map_or(Ok(()), Err)
    }

    /// Refuses new work and abandons automatic starts, aborts a core download and lets the
    /// update in progress finish, lets sign-in actions and then credential changes finish,
    /// drops the pending sign-in, lets the scheduler's running work finish, then stops every
    /// core. Close the store only after this: each stop drains usage into it.
    ///
    /// # Errors
    /// Ending the pending sign-in's, or a core's that did not stop.
    pub async fn shutdown(&self) -> Result<(), AppError> {
        self.scheduler.begin_shutdown().await;
        self.updates.shutdown().await;
        self.sign_in.settled().await;
        self.credentials.settled().await;
        let cleared = self.sign_in.clear().await;
        let stopped = self.scheduler.shutdown().await;
        cleared.and(stopped)
    }
}
