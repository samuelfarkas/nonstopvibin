//! Reviewed core updates for installed builds (`src/server/core-updates.ts`). The feed is the
//! core pin on `main`, so a core runs only if its archive and executable match a pin someone
//! reviewed; archives always come from upstream's GitHub releases. Each update is kept as
//! `cores/<version>/`, the executable beside its manifest, and startup selects the newest
//! intact one above the bundled core, so an app update that bundles a newer or equal core wins.
//!
//! One check, install or revert runs at a time, in phases: checking, downloading, testing (the
//! new core starts on a throwaway store under `oauth-pending/`), restarting. Nothing changes
//! unless every step before the restart succeeds. The restart waits for sign-in actions and
//! credential changes, refuses while a sign-in is pending, then restarts each running profile on
//! the new core and reports the ones that do not start again. Shutdown aborts a download or
//! start check and waits for the operation to end.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nsv_core::binary::{BINARY, CorePin, CoreTarget, bundled_pin, verify_core};
use nsv_core::install::{
    ActiveCore, CORE_ARCHIVES, Staging, fetch_pin, install_core, prune, select_installed_core,
};
use nsv_core::pool::Pool;
use nsv_domain::error::AppError;
use nsv_domain::sync::locked;
use nsv_domain::version::{AppVersion, Version};
use nsv_domain::wire::{CorePhase, CoreState};
use nsv_store::Store;
use tokio::sync::watch;

use crate::credentials::Credentials;
use crate::fetch::Outside;
use crate::sandbox::Sandbox;
use crate::sign_in::{Backends, Report, SignIn, about};
use crate::task::{blocking, detached};

pub struct UpdateOptions {
    pub outside: Arc<dyn Outside>,
    /// The reviewed pin feed of an installed build: [`nsv_core::install::CORE_FEED`], or
    /// `NONSTOPVIBIN_CORE_FEED`. `None` in development runs, which never select, install or
    /// delete installed cores.
    pub feed: Option<String>,
    /// The bundled core, which profiles start from unless an update is selected.
    pub bundled: PathBuf,
    /// Whether the bundled core must also match the pin compiled in (headless releases).
    pub require_pinned_core: bool,
    pub backends: Backends,
    /// How long the new core gets to answer its check.
    pub ready: Duration,
    pub report: Report,
    /// The app these cores run under, which a pin's `minApp` admits.
    pub app: AppVersion,
}

/// Checks the feed, installs a reviewed core, or goes back to the bundled one.
pub struct CoreUpdates(Arc<Inner>);

impl Clone for CoreUpdates {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

struct Inner {
    store: Store,
    pool: Pool,
    sign_in: SignIn,
    credentials: Credentials,
    options: UpdateOptions,
    cores: PathBuf,
    bundled: ActiveCore,
    target: CoreTarget,
    status: Mutex<Status>,
    /// The operation in progress.
    phase: watch::Sender<Option<CorePhase>>,
    /// Shutdown began: a download or start check stops.
    closing: watch::Sender<bool>,
}

struct Status {
    active: ActiveCore,
    /// What the last check fetched.
    pin: Option<CorePin>,
    /// The check under way, which later callers share.
    checking: Option<watch::Receiver<Option<Result<(), AppError>>>>,
}

fn io_error(error: &io::Error) -> AppError {
    AppError::internal(error)
}

impl CoreUpdates {
    /// # Errors
    /// The bundled pin or this platform is unknown, which a build rules out.
    pub fn new(
        store: Store,
        pool: Pool,
        sign_in: SignIn,
        credentials: Credentials,
        options: UpdateOptions,
    ) -> Result<Self, AppError> {
        let pin = bundled_pin().map_err(AppError::internal)?;
        let bundled = ActiveCore {
            version: pin.version,
            binary: options.bundled.clone(),
        };
        Ok(Self(Arc::new(Inner {
            cores: store.directory().cores(),
            store,
            pool,
            sign_in,
            credentials,
            options,
            status: Mutex::new(Status {
                active: bundled.clone(),
                pin: None,
                checking: None,
            }),
            bundled,
            target: CoreTarget::current()?,
            phase: watch::Sender::new(None),
            closing: watch::Sender::new(false),
        })))
    }

    /// Whether this run installs reviewed core updates.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.0.options.feed.is_some()
    }

    /// The version of the core profiles start from (`state.coreVersion`).
    #[must_use]
    pub fn version(&self) -> String {
        self.0.status().active.version.to_string()
    }

    /// `state.core`.
    #[must_use]
    pub fn state(&self) -> CoreState {
        let status = self.0.status();
        CoreState {
            bundled: self.0.bundled.version.to_string(),
            updates: self.enabled(),
            available: status
                .pin
                .as_ref()
                .filter(|pin| self.0.offers(&status.active, pin))
                .map(|pin| pin.version.to_string()),
            phase: *self.0.phase.borrow(),
        }
    }

    /// At startup, before any core runs: profiles start from the newest intact installed core
    /// above the bundled one, if any, and every other entry of `cores/` is deleted. Answers
    /// whether one was selected; development runs select none and delete nothing.
    ///
    /// # Errors
    /// A file-system error other than an unreadable entry.
    pub async fn select(&self) -> Result<bool, AppError> {
        if !self.enabled() {
            return Ok(false);
        }
        let (cores, bundled, target) =
            (self.0.cores.clone(), self.0.bundled.version, self.0.target);
        let selected =
            blocking(move || Ok(select_installed_core(&cores, bundled, target)?)).await?;
        match selected {
            Some(core) => {
                self.0.activate(core).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// `POST /core/check`: reads the feed. A check already under way, such as the one at
    /// launch, answers this one too.
    ///
    /// # Errors
    /// 409 in development runs or while an install or revert runs, 503 during shutdown, 502
    /// when the feed cannot be read or is not a pin.
    pub async fn check(&self) -> Result<(), AppError> {
        let mut checking = {
            let mut status = self.0.status();
            if let Some(checking) = &status.checking {
                checking.clone()
            } else {
                let (sender, receiver) = watch::channel(None);
                status.checking = Some(receiver.clone());
                let inner = Arc::clone(&self.0);
                tokio::spawn(async move {
                    let checked = Arc::clone(&inner)
                        .run(CorePhase::Checking, |inner| async move {
                            let pin = inner.fetch_pin().await?;
                            inner.status().pin = Some(pin);
                            Ok(())
                        })
                        .await;
                    inner.status().checking = None;
                    sender.send_replace(Some(checked));
                });
                receiver
            }
        };
        let checked = checking
            .wait_for(Option::is_some)
            .await
            .map_err(|_| AppError::shutting_down())?
            .clone();
        checked.unwrap_or_else(|| Err(AppError::shutting_down()))
    }

    /// `POST /core/install`: installs `version` if the feed still offers it, checks that it
    /// starts, then restarts running profiles on it.
    ///
    /// # Errors
    /// As [`CoreUpdates::check`], 409 when `version` is no longer offered or a sign-in is
    /// pending, 502 for a download that does not match the pin or a core that does not start;
    /// nothing changed then.
    pub async fn install(&self, version: &str) -> Result<(), AppError> {
        self.0.after_check().await;
        let version = version.to_owned();
        Arc::clone(&self.0)
            .run(CorePhase::Checking, |inner| async move {
                inner.install(&version).await
            })
            .await
    }

    /// `DELETE /core/install`: restarts running profiles on the bundled core, then deletes the
    /// installed updates.
    ///
    /// # Errors
    /// As [`CoreUpdates::check`], 409 when the bundled core already runs or a sign-in is
    /// pending, 503 when the bundled core fails its integrity check; the update stays then.
    pub async fn use_bundled(&self) -> Result<(), AppError> {
        self.0.after_check().await;
        Arc::clone(&self.0)
            .run(CorePhase::Restarting, |inner| async move {
                if inner.status().active == inner.bundled {
                    return Err(AppError::new(
                        "NonstopVibin already runs its bundled core.",
                        409,
                    ));
                }
                // Startup checked the installed core instead; keep it unless this one is intact.
                inner.verify_bundled().await?;
                let bundled = inner.bundled.clone();
                inner.switch_to(bundled, None).await?;
                // Only now does no start read an installed executable. A crash before this
                // leaves the update in place, and the next launch runs it.
                let cores = inner.cores.clone();
                blocking(move || prune(&cores, None).map_err(|error| io_error(&error))).await
            })
            .await
    }

    /// Checks the bundled core, which profiles start from unless an update is selected.
    ///
    /// # Errors
    /// A bundled core that is missing or fails its integrity check.
    pub(crate) async fn verify_bundled(&self) -> Result<(), AppError> {
        self.0.verify_bundled().await
    }

    /// Aborts a download or start check, then waits for the operation in progress to end.
    pub async fn shutdown(&self) {
        self.0.closing.send_replace(true);
        let mut phase = self.0.phase.subscribe();
        // The sender lives in `self`, so the wait ends only when the operation does.
        let _ended = phase.wait_for(Option::is_none).await;
    }
}

impl Inner {
    /// Checks the bundled core, and in a headless release that it is the pin compiled in.
    async fn verify_bundled(&self) -> Result<(), AppError> {
        let binary = self.bundled.binary.clone();
        let pinned = if self.options.require_pinned_core {
            Some(bundled_pin().map_err(AppError::internal)?)
        } else {
            None
        };
        blocking(move || Ok(verify_core(&binary, pinned)?)).await
    }

    fn status(&self) -> MutexGuard<'_, Status> {
        locked(&self.status)
    }

    /// Whether `pin` is newer than the core in use and this app may run it. A pin names every
    /// target, so each one covers this platform.
    fn offers(&self, active: &ActiveCore, pin: &CorePin) -> bool {
        pin.version > active.version
            && pin
                .min_app
                .is_none_or(|minimum| self.options.app >= minimum)
    }

    async fn activate(&self, core: ActiveCore) -> Result<(), AppError> {
        self.pool.use_binary(core.binary.clone()).await?;
        self.status().active = core;
        Ok(())
    }

    /// A check is short and its answer is about to be replaced, so an install or revert waits
    /// for it.
    async fn after_check(&self) {
        let checking = self.status().checking.clone();
        if let Some(mut checking) = checking {
            let _checked = checking.wait_for(Option::is_some).await;
        }
    }

    /// Runs `action` as the one operation in progress, starting in `phase`, to its end even if
    /// the caller stops waiting.
    async fn run<T, A>(
        self: Arc<Self>,
        phase: CorePhase,
        action: impl FnOnce(Arc<Self>) -> A,
    ) -> Result<T, AppError>
    where
        T: Send + 'static,
        A: Future<Output = Result<T, AppError>> + Send + 'static,
    {
        if self.options.feed.is_none() {
            return Err(AppError::new("Only installed builds update the core.", 409));
        }
        if self.pool.state().closing || *self.closing.borrow() {
            return Err(AppError::shutting_down());
        }
        let began = self.phase.send_if_modified(|current| {
            let idle = current.is_none();
            if idle {
                *current = Some(phase);
            }
            idle
        });
        if !began {
            return Err(AppError::new("A core update is already in progress.", 409));
        }
        let ended = Arc::clone(&self);
        // Boxed: a whole install would otherwise sit in each caller's future.
        let work = Box::pin(action(self));
        detached((), async move {
            let result = work.await;
            ended.phase.send_replace(None);
            result
        })
        .await
    }

    /// `work`, or 503 once shutdown begins.
    async fn abortable<T>(
        &self,
        work: impl Future<Output = Result<T, AppError>>,
    ) -> Result<T, AppError> {
        let mut closing = self.closing.subscribe();
        tokio::select! {
            result = work => result,
            _ = closing.wait_for(|closing| *closing) => Err(AppError::shutting_down()),
        }
    }

    async fn fetch_pin(&self) -> Result<CorePin, AppError> {
        let feed = self.options.feed.as_deref().unwrap_or_default();
        self.abortable(fetch_pin(&*self.options.outside, feed))
            .await
    }

    async fn install(self: &Arc<Self>, version: &str) -> Result<(), AppError> {
        let pin = self.fetch_pin().await?;
        let offered = {
            let mut status = self.status();
            status.pin = Some(pin.clone());
            pin.version.to_string() == version && self.offers(&status.active, &pin)
        };
        if !offered {
            return Err(AppError::new(
                format!("CLIProxyAPI {version} is no longer offered."),
                409,
            ));
        }
        self.phase.send_replace(Some(CorePhase::Downloading));
        let cores = self.cores.clone();
        let staging =
            blocking(move || Staging::create(&cores).map_err(|error| io_error(&error))).await?;
        self.abortable(async {
            Ok(install_core(
                &*self.options.outside,
                CORE_ARCHIVES,
                &pin,
                self.target,
                staging.path(),
                false,
            )
            .await?)
        })
        .await?;
        self.phase.send_replace(Some(CorePhase::Testing));
        self.preflight(&staging.path().join(BINARY), pin.version)
            .await?;
        self.phase.send_replace(Some(CorePhase::Restarting));
        let core = ActiveCore {
            version: pin.version,
            binary: self.cores.join(pin.version.to_string()).join(BINARY),
        };
        self.switch_to(core, Some(staging)).await?;
        let cores = self.cores.clone();
        blocking(move || prune(&cores, Some(pin.version)).map_err(|error| io_error(&error))).await
    }

    /// Starts `binary` on a throwaway profile, as a sign-in sandbox does, and lists its models
    /// through its client key. Startup reaps and deletes what a crash leaves.
    async fn preflight(&self, binary: &Path, version: Version) -> Result<(), AppError> {
        let sandbox = Sandbox::open(
            &self.store,
            "core-".to_owned(),
            "Core check",
            binary.to_owned(),
            &self.options.backends,
            self.options.ready,
        )
        .await?;
        let checked = self
            .abortable(async {
                sandbox.pool.start(&sandbox.profile, false).await?;
                sandbox.pool.models(&sandbox.profile).await?;
                Ok(())
            })
            .await;
        let discarded = sandbox.discard().await;
        match checked {
            Err(error) if *self.closing.borrow() => Err(error),
            Err(error) => Err(AppError::new(
                format!(
                    "CLIProxyAPI {version} did not start: {} Nothing was changed.",
                    error.message
                ),
                502,
            )),
            Ok(()) => discarded,
        }
    }

    /// Commits `staging` if given, puts `core` in use, and restarts each running profile on
    /// it. It waits for sign-in actions and credential changes and refuses while a sign-in is
    /// pending, so a restart interrupts none.
    async fn switch_to(
        self: &Arc<Self>,
        core: ActiveCore,
        staging: Option<Staging>,
    ) -> Result<(), AppError> {
        let (inner, credentials) = (Arc::clone(self), self.credentials.clone());
        self.sign_in
            .while_idle(async move {
                credentials
                    .queue(async move { inner.switch(core, staging).await })
                    .await
            })
            .await
    }

    async fn switch(&self, core: ActiveCore, staging: Option<Staging>) -> Result<(), AppError> {
        if let Some(staging) = staging {
            let version = core.version;
            blocking(move || {
                staging
                    .commit(version)
                    .map(drop)
                    .map_err(|error| io_error(&error))
            })
            .await?;
        }
        self.activate(core.clone()).await?;
        let profiles = self.store.call(|db| db.profiles()).await?;
        let pool = &self.pool;
        for profile in profiles {
            // Starts are refused once shutdown begins; that is no failure of the core.
            if pool.state().closing {
                return Ok(());
            }
            // A start under way may have read the previous core.
            let mut changes = pool.subscribe();
            drop(changes.wait_for(|state| !state.starting(&profile.id)).await);
            let state = pool.state();
            let current = state
                .core(&profile.id)
                .is_some_and(|running| *running.binary == *core.binary);
            if current || !state.alive(&profile.id) {
                continue;
            }
            // Nothing else starts it between its stop and its start on the new core.
            let restarted = pool.while_stopped(&profile.id, std::future::ready(Ok(())));
            if let Err(error) = restarted.await {
                if pool.state().closing {
                    return Ok(());
                }
                (self.options.report)(about(&profile.name, &error.message));
            }
        }
        Ok(())
    }
}
