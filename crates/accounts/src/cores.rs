//! The account features around one scheduler and its pool, as `src/server/cores.ts` assembled
//! them: what the server's routes call, the scheduler's [`Host`], launch and the shutdown
//! order.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use nsv_core::binary::{bundled_pin, verify_core};
use nsv_core::install::Releases;
use nsv_core::pool::{BoxFuture, Pool};
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::wire::{AccountKind, OAuthSession, QuotaStatus};
use nsv_store::Store;
use tokio::sync::watch;

use crate::accounts::{Accounts, PoolSource};
use crate::credentials::Credentials;
use crate::fetch::Fetch;
use crate::quota::Quotas;
use crate::resets::CodexResets;
use crate::scheduler::{Host, Scheduler, Timing};
use crate::sign_in::{Backends, Report, SignIn, SignInOptions};
use crate::updates::{CoreUpdates, UpdateOptions};

pub struct CoresOptions<F, R> {
    pub store: Store,
    /// The bundled core.
    pub binary: PathBuf,
    /// Launches a data directory's cores: `Arc::new(Processes::new(directory))`.
    pub backends: Backends,
    /// How long a new core gets to answer ([`nsv_core::pool::READY`]).
    pub ready: Duration,
    pub timing: Timing,
    /// Provider requests: quota, model discovery, identity checks.
    pub fetch: F,
    /// The pin feed and release archives.
    pub releases: R,
    /// The reviewed pin feed of an installed build; `None` in development runs. See
    /// [`UpdateOptions::feed`].
    pub feed: Option<String>,
    /// Whether the bundled core must also match the pin compiled in (headless releases).
    pub require_pinned_core: bool,
    /// A development run starts without its bundled core; each profile start then reports it
    /// missing.
    pub allow_missing_core: bool,
    /// Milliseconds since the epoch: quota check times and sign-in expiry.
    pub clock: fn() -> i64,
    pub report: Report,
    /// A browser sign-in callback was accepted: the desktop shell brings an open main window
    /// forward, never creating one.
    pub sign_in_accepted: Arc<dyn Fn() + Send + Sync>,
}

/// The pool's scheduler and the account features that drive it.
pub struct Cores<F, R> {
    pub scheduler: Scheduler,
    pub accounts: Arc<Accounts>,
    pub quotas: Arc<Quotas<PoolSource, F>>,
    pub credentials: Credentials<F>,
    pub sign_in: SignIn<F>,
    pub resets: CodexResets,
    pub updates: CoreUpdates<F, R>,
    store: Store,
    binary: PathBuf,
    backends: Backends,
    require_pinned_core: bool,
    allow_missing_core: bool,
}

/// What the scheduler asks of the features. It holds no handle to the scheduler, so nothing
/// keeps the scheduler alive in a cycle.
struct FeatureHost<F> {
    report: Report,
    quotas: OnceLock<Arc<Quotas<PoolSource, F>>>,
    sessions: OnceLock<watch::Receiver<Option<OAuthSession>>>,
}

impl<F: Fetch + 'static> Host for FeatureHost<F> {
    fn report(&self, message: String) {
        (self.report)(message);
    }

    fn signing_in(&self, profile: &ProfileId) -> bool {
        self.sessions.get().is_some_and(|sessions| {
            sessions
                .borrow()
                .as_ref()
                .is_some_and(|session| session.profile_id == *profile)
        })
    }

    fn refresh_quotas(
        &self,
        pool: Pool,
        profile: ProfileId,
    ) -> BoxFuture<'static, Result<(), AppError>> {
        let quotas = self.quotas.get().cloned();
        Box::pin(async move {
            let Some(quotas) = quotas else {
                return Ok(());
            };
            // Each account is checked as its turn comes; a stop or shutdown ends the pass.
            quotas
                .refresh_accounts(&profile, |account| {
                    pool.usable(&profile)
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

impl<F: Fetch + 'static, R: Releases> Cores<F, R> {
    /// # Errors
    /// The bundled pin or this platform is unknown, which a build rules out.
    pub fn new(options: CoresOptions<F, R>) -> Result<Self, AppError> {
        let CoresOptions {
            store,
            binary,
            backends,
            ready,
            timing,
            fetch,
            releases,
            feed,
            require_pinned_core,
            allow_missing_core,
            clock,
            report,
            sign_in_accepted,
        } = options;
        let accounts = Arc::new(Accounts::new(store.clone()));
        let host = Arc::new(FeatureHost {
            report: Arc::clone(&report),
            quotas: OnceLock::new(),
            sessions: OnceLock::new(),
        });
        let hooks: Arc<dyn Host> = Arc::<FeatureHost<F>>::clone(&host);
        let scheduler = Scheduler::new(
            store.clone(),
            binary.clone(),
            backends(store.directory()),
            ready,
            Arc::clone(&accounts),
            hooks,
            timing,
        );
        let source = PoolSource {
            pool: scheduler.pool().clone(),
            accounts: Arc::clone(&accounts),
        };
        let quotas = Arc::new(Quotas::new(store.clone(), source, fetch, clock));
        let credentials = Credentials::new(
            store.clone(),
            scheduler.clone(),
            Arc::clone(&accounts),
            Arc::clone(&quotas),
        );
        let sign_in = SignIn::new(
            store.clone(),
            scheduler.clone(),
            Arc::clone(&accounts),
            credentials.clone(),
            Arc::clone(&quotas),
            SignInOptions {
                backends: Arc::clone(&backends),
                ready,
                clock,
                report: Arc::clone(&report),
                accepted: sign_in_accepted,
            },
        );
        let updates = CoreUpdates::new(
            store.clone(),
            scheduler.clone(),
            sign_in.clone(),
            credentials.clone(),
            UpdateOptions {
                releases,
                feed,
                bundled: binary.clone(),
                require_pinned_core,
                backends: Arc::clone(&backends),
                ready,
                report,
            },
        )?;
        // Set once, before anything is scheduled: `launch` comes after `new`.
        let _quotas = host.quotas.set(Arc::clone(&quotas));
        let _sessions = host.sessions.set(sign_in.sessions());
        Ok(Self {
            resets: CodexResets::new(store.clone()),
            scheduler,
            accounts,
            quotas,
            credentials,
            sign_in,
            updates,
            store,
            binary,
            backends,
            require_pinned_core,
            allow_missing_core,
        })
    }

    #[must_use]
    pub fn pool(&self) -> &Pool {
        self.scheduler.pool()
    }

    /// At startup: profiles start from the selected installed core, or the bundled one once it
    /// passes its integrity check (a development run may lack it). Cores a dead earlier instance left are stopped (the
    /// profiles' and the pending sign-ins'), pending sign-ins and core checks are deleted, the
    /// profiles start and scheduling begins. An installed build then checks for a core update
    /// in the background; offline stays quiet, as Settings checks again.
    ///
    /// # Errors
    /// A bundled core that is missing or fails its check, a ledger or file-system error, or
    /// the scheduler's.
    pub async fn launch(&self) -> Result<(), AppError> {
        let missing = self.allow_missing_core && !self.binary.exists();
        if !self.updates.select().await? && !missing {
            let pinned = if self.require_pinned_core {
                Some(bundled_pin().map_err(AppError::internal)?)
            } else {
                None
            };
            let binary = self.binary.clone();
            tokio::task::spawn_blocking(move || verify_core(&binary, pinned))
                .await
                .map_err(AppError::internal)??;
        }
        // Orphans first: if the pending sign-ins cannot be removed, startup fails without
        // leaving one running.
        self.pool()
            .reap_orphans()
            .await
            .map_err(AppError::internal)?;
        SignIn::<F>::discard_pending(self.store.directory(), &self.backends).await?;
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
        let pool = self.pool();
        let keeps_running = !self.accounts.list(profile).await?.is_empty();
        let failure = if keeps_running {
            pool.start(profile, false).await.err()
        } else {
            None
        };
        // Once shutdown began nothing new may begin: its work waits for no provider request.
        if let Some(failure) = failure.as_ref().filter(|_| pool.closing()) {
            return Err(failure.clone());
        }
        // A core that is not running could only record a failed check.
        self.quotas
            .refresh_accounts(profile, |account| {
                !account.disabled && (account.kind == AccountKind::ApiKey || pool.usable(profile))
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
