//! The core pool: one core per running profile. One owner task holds every profile's runtime
//! state and is its only writer. Callers send it commands through a bounded channel and read
//! the immutable [`PoolState`] it publishes through a `watch`. Launches, readiness waits and
//! stops run in tasks of their own and report back, so one profile's readiness wait never
//! delays another profile's command, and a caller that stops waiting never strands one.
//!
//! The management client lives here too: only the pool and the code that holds it can reach a
//! core's management API. A port of `src/server/core.ts`.

mod backend;
mod listener;
mod owner;

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::secret::{CoreKey, ManagementKey};
use nsv_domain::wire::{Model, Runtime};
use nsv_net::Method;
use nsv_store::Store;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::{applied, write_config};

pub use backend::{Backend, Call, CoreHandle, Processes};
use owner::{Command, Owner};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How long a new core gets to answer its management API before it is stopped.
pub const READY: Duration = Duration::from_secs(15);
/// The wait between readiness checks, and between checks that settings were applied.
const RETRY: Duration = Duration::from_millis(100);
/// The timeout of a management request.
const MANAGEMENT_TIMEOUT: Duration = Duration::from_secs(15);
/// Commands waiting for the owner before a caller waits to enqueue.
const QUEUE: usize = 64;
/// The most of a management or model-list answer read.
const MANAGEMENT_LIMIT: u64 = 32 << 20;
const MODELS_LIMIT: u64 = 16 << 20;

const STOPPED: &str = "This profile is stopped. Start it in nonstopvibin.";

/// One launch of a profile's core. A restart makes a new one, so an unchanged id means the same
/// core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CoreId(u64);

/// Where a core is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Launched; it has not answered its management API yet.
    Starting,
    /// It answered.
    Running,
    /// Being stopped. `answered`: it was running, so the final usage drain still reaches it.
    /// `signalled`: it was sent SIGTERM and may close its listener before it exits, so nothing
    /// reaches it any more.
    Stopping { answered: bool, signalled: bool },
    /// It exited after being stopped on purpose.
    Stopped,
    /// It could not launch, did not answer in time, or exited on its own: why.
    Failed(Arc<str>),
}

impl Phase {
    /// The process exists and has not exited.
    #[must_use]
    pub const fn alive(&self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping { .. })
    }

    /// It answered and has not exited. It may be stopping.
    #[must_use]
    pub const fn answered(&self) -> bool {
        matches!(self, Self::Running | Self::Stopping { answered: true, .. })
    }

    /// It answered and has not been signalled to stop: its port still leads to it.
    #[must_use]
    pub const fn reachable(&self) -> bool {
        matches!(
            self,
            Self::Running
                | Self::Stopping {
                    answered: true,
                    signalled: false
                }
        )
    }
}

/// A profile's latest core. Its keys never leave this module.
#[derive(Debug, Clone)]
pub struct Core {
    pub id: CoreId,
    pub port: u16,
    pub phase: Phase,
    /// The executable it was launched from.
    pub binary: Arc<Path>,
    keys: Arc<CoreKeys>,
}

#[derive(Debug)]
struct CoreKeys {
    management: ManagementKey,
    core: CoreKey,
}

/// What the pool knows about one profile.
#[derive(Debug, Clone, Default)]
pub struct ProfileCore {
    /// The latest core, kept after it exits for its error and port.
    pub core: Option<Core>,
    /// A start is in flight.
    pub starting: bool,
    /// A stop or a credential swap is in flight: it decides whether the core runs next.
    pub holding: bool,
}

/// The pool as its owner last published it.
#[derive(Debug, Clone)]
pub struct PoolState {
    /// Shutdown began: nothing starts and no request may be sent to a core.
    pub closing: bool,
    /// The executable new cores start from.
    pub binary: Arc<Path>,
    pub profiles: BTreeMap<ProfileId, ProfileCore>,
}

impl PoolState {
    fn profile(&self, profile: &ProfileId) -> Option<&ProfileCore> {
        self.profiles.get(profile)
    }

    #[must_use]
    pub fn core(&self, profile: &ProfileId) -> Option<&Core> {
        self.profile(profile)?.core.as_ref()
    }

    /// The core answered and has not exited. It may be stopping.
    #[must_use]
    pub fn running(&self, profile: &ProfileId) -> bool {
        self.core(profile).is_some_and(|core| core.phase.answered())
    }

    /// Requests may be sent: the core runs, is not stopping, and the pool is not shutting down.
    #[must_use]
    pub fn usable(&self, profile: &ProfileId) -> bool {
        !self.closing
            && self
                .core(profile)
                .is_some_and(|core| core.phase == Phase::Running)
    }

    /// The profile's core process exists and has not exited.
    #[must_use]
    pub fn alive(&self, profile: &ProfileId) -> bool {
        self.core(profile).is_some_and(|core| core.phase.alive())
    }

    #[must_use]
    pub fn starting(&self, profile: &ProfileId) -> bool {
        self.profile(profile).is_some_and(|state| state.starting)
    }

    #[must_use]
    pub fn holding(&self, profile: &ProfileId) -> bool {
        self.profile(profile).is_some_and(|state| state.holding)
    }

    /// What the interface shows: the profile's runtime and its last failure.
    #[must_use]
    pub fn runtime(&self, profile: &ProfileId) -> (Runtime, Option<&Arc<str>>) {
        match self.core(profile).map(|core| &core.phase) {
            None | Some(Phase::Stopped) => (Runtime::Stopped, None),
            Some(
                Phase::Starting
                | Phase::Stopping {
                    answered: false, ..
                },
            ) => (Runtime::Starting, None),
            Some(Phase::Running | Phase::Stopping { answered: true, .. }) => {
                (Runtime::Running, None)
            }
            Some(Phase::Failed(error)) => (Runtime::Error, Some(error)),
        }
    }
}

/// What happened to a profile's core, or to a start, stop or credential swap of it, told as it
/// happens and in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEvent {
    /// A start began; `automatic` is what its caller said.
    Starting { automatic: bool },
    /// That start ended; `running`: its core runs now; `error`: why not.
    Started {
        automatic: bool,
        running: bool,
        error: Option<String>,
    },
    /// The core exited or could not launch; `error` unless it was being stopped.
    Exited { error: Option<String> },
    /// A stop or credential swap began or ended; whether one is still in flight.
    Holding(bool),
}

/// What the pool asks of the code around it. The owner calls [`Hooks::event`] synchronously,
/// so it must not wait for the pool.
pub trait Hooks: Send + Sync + 'static {
    fn event(&self, profile: &ProfileId, event: CoreEvent);

    /// Runs each time a new core answers, before its start ends; an error retries until the
    /// readiness deadline.
    ///
    /// # Errors
    /// Why the core is not ready yet.
    fn ready(&self, profile: ProfileId, core: CoreId) -> BoxFuture<'static, Result<(), AppError>>;

    /// Runs before a running core is terminated: the usage queue lives only in its memory.
    fn before_stop(&self, profile: ProfileId) -> BoxFuture<'static, ()>;
}

/// The hooks of a pool that runs no features, such as a sign-in's.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl Hooks for NoHooks {
    fn event(&self, _: &ProfileId, _: CoreEvent) {}

    fn ready(&self, _: ProfileId, _: CoreId) -> BoxFuture<'static, Result<(), AppError>> {
        Box::pin(async { Ok(()) })
    }

    fn before_stop(&self, _: ProfileId) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// What the owner and its tasks share.
struct Context {
    store: Store,
    /// The daemon's port, where Claude Agent accounts are served; `None` where nothing serves
    /// them.
    daemon: Option<u16>,
    backend: Arc<dyn Backend>,
    hooks: Arc<dyn Hooks>,
    ready: Duration,
    state: watch::Receiver<Arc<PoolState>>,
}

impl Context {
    fn closing(&self) -> bool {
        self.state.borrow().closing
    }

    /// Ends once shutdown begins.
    async fn abandoned(&self) {
        let mut state = self.state.clone();
        // The sender lives as long as the owner; if it is gone, nothing waits for this.
        let _ = state.wait_for(|state| state.closing).await;
    }
}

/// A handle to the pool. Clones share the owner, which ends once every handle and task is gone;
/// cores it still runs are then stopped by their watchdogs.
#[derive(Clone)]
pub struct Pool {
    commands: mpsc::Sender<Command>,
    context: Arc<Context>,
}

/// How a pool's cores start.
pub struct Launch {
    /// The core binary.
    pub binary: PathBuf,
    pub backend: Arc<dyn Backend>,
    /// How long a new core gets to answer: [`READY`] in the daemon.
    pub ready: Duration,
    /// The daemon's port, at which cores reach Claude Agent accounts; `None` where nothing
    /// serves them.
    pub daemon: Option<u16>,
}

impl Pool {
    /// Starts the owner task on the current runtime.
    #[must_use]
    pub fn new(store: Store, launch: Launch, hooks: Arc<dyn Hooks>) -> Self {
        let Launch {
            binary,
            backend,
            ready,
            daemon,
        } = launch;
        let (commands, queue) = mpsc::channel(QUEUE);
        let (published, state) = watch::channel(Arc::new(PoolState {
            closing: false,
            binary: Arc::from(binary),
            profiles: BTreeMap::new(),
        }));
        let context = Arc::new(Context {
            store,
            daemon,
            backend,
            hooks,
            ready,
            state,
        });
        let owner = Owner::new(Arc::clone(&context), queue, commands.downgrade(), published);
        tokio::spawn(owner.run());
        Self { commands, context }
    }

    /// The pool as last published.
    #[must_use]
    pub fn state(&self) -> Arc<PoolState> {
        Arc::clone(&self.context.state.borrow())
    }

    /// Changes as they are published: a core became running, failed, exited or was replaced, a
    /// start, stop or swap began or ended, or shutdown began.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Arc<PoolState>> {
        self.context.state.clone()
    }

    /// The port of the profile's usable core.
    ///
    /// # Errors
    /// 503 while the profile is not usable.
    pub fn port(&self, profile: &ProfileId) -> Result<u16, AppError> {
        let state = self.context.state.borrow();
        match state.core(profile) {
            Some(core) if state.usable(profile) => Ok(core.port),
            _ => Err(AppError::new(STOPPED, 503)),
        }
    }

    async fn ask<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, AppError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(command(reply))
            .await
            .map_err(|_| AppError::shutting_down())?;
        answer.await.map_err(|_| AppError::shutting_down())
    }

    /// Starts the profile's core and waits until it answers, or joins the start in flight.
    /// Waits for a stop in flight first. `automatic` is named in this start's events.
    ///
    /// # Errors
    /// 503 during shutdown or when the core does not become ready, 409 while a credential swap
    /// holds it, 404 for an unknown profile.
    pub async fn start(&self, profile: &ProfileId, automatic: bool) -> Result<(), AppError> {
        let profile = profile.clone();
        self.ask(|reply| Command::Start {
            profile,
            automatic,
            reply,
        })
        .await?
    }

    /// Stops the profile's core after a start in flight ends: the final usage drain, then
    /// SIGTERM and SIGKILL after five seconds.
    ///
    /// # Errors
    /// The core could not be stopped.
    pub async fn stop(&self, profile: &ProfileId) -> Result<(), AppError> {
        let profile = profile.clone();
        self.ask(|reply| Command::Stop { profile, reply }).await?
    }

    /// A place in the owner's queue for a stop, taken before deciding to stop. The decision and
    /// [`StopPermit::stop`] can then happen with nothing in between.
    ///
    /// # Errors
    /// The pool is gone.
    pub async fn reserve(&self) -> Result<StopPermit, AppError> {
        let permit = self
            .commands
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| AppError::shutting_down())?;
        Ok(StopPermit(permit))
    }

    /// Stops the core, runs `replace`, then starts the core again if it was running or
    /// starting and shutdown has not begun. Nothing can start it in between: its own token
    /// refresh could overwrite a file being replaced. Runs to its end even if the caller stops
    /// waiting.
    ///
    /// # Errors
    /// The restart's error, else the stop's or `replace`'s.
    pub async fn while_stopped<F>(&self, profile: &ProfileId, replace: F) -> Result<(), AppError>
    where
        F: Future<Output = Result<(), AppError>> + Send + 'static,
    {
        let pool = self.clone();
        let profile = profile.clone();
        let swap = tokio::spawn(async move {
            let held = profile.clone();
            let restart = pool
                .ask(|reply| Command::Hold {
                    profile: held,
                    reply,
                })
                .await?;
            let outcome = match pool.stop(&profile).await {
                Ok(()) => replace.await,
                Err(error) => Err(error),
            };
            let released = pool
                .ask(|reply| Command::Release {
                    profile,
                    restart,
                    reply,
                })
                .await
                .and_then(|started| started);
            released.and(outcome)
        });
        swap.await.map_err(AppError::internal)?
    }

    /// Rewrites a running core's config and waits until the core has applied it. A core that is
    /// starting may have read the settings before this change, so it is applied once ready. If
    /// the core stops or is replaced meanwhile, this ends quietly: every start writes the saved
    /// settings.
    ///
    /// # Errors
    /// 503 when the core has not applied the settings after 60 checks 100 ms apart, or a write or
    /// management error from a core that is still current.
    pub async fn configure(&self, profile: &ProfileId) -> Result<(), AppError> {
        let mut state = self.context.state.clone();
        // A failed start leaves no core to configure.
        let _ = state.wait_for(|state| !state.starting(profile)).await;
        let Some(core) = self.context.state.borrow().core(profile).cloned() else {
            return Ok(());
        };
        // Checked right before each write and request. A start can only follow once this core
        // stops, so a write enqueued while it holds lands before the successor's own write.
        let current = || {
            let state = self.context.state.borrow();
            state.core(profile).is_some_and(|now| now.id == core.id)
                && state.usable(profile)
                && !state.holding(profile)
        };
        if !current() {
            return Ok(());
        }
        let outcome = async {
            let mut expected = self.write(profile, core.port).await?;
            // The core's file watcher reloads asynchronously; only the applied config counts.
            for attempt in 0..60 {
                if !current() {
                    return Ok(true);
                }
                let reported = self.call(&core, &Method::GET, "/config", None);
                if applied(&expected, &reported.await?) {
                    return Ok(true);
                }
                // A core that has only just answered can miss a write for good; another reaches it.
                if attempt % 10 == 9 {
                    if !current() {
                        return Ok(true);
                    }
                    expected = self.write(profile, core.port).await?;
                }
                tokio::time::sleep(RETRY).await;
            }
            Ok::<bool, AppError>(false)
        };
        match outcome.await {
            Ok(true) => Ok(()),
            // A stop marks its core before ending it: a failure from a core that went away is
            // not one.
            _ if !current() => Ok(()),
            Ok(false) => Err(AppError::new(
                "The proxy has not applied the settings yet. Stop and start this profile to reload them.",
                503,
            )),
            Err(error) => Err(error),
        }
    }

    async fn write(
        &self,
        profile: &ProfileId,
        port: u16,
    ) -> Result<crate::config::CoreConfiguration, AppError> {
        write_config(
            &self.context.store,
            profile.clone(),
            port,
            self.context.daemon,
        )
        .await
        .map_err(AppError::from)
    }

    /// A management request to the profile's latest core, once it answered and until it is
    /// signalled to stop: before, its port may be another user's listener, and after, the core
    /// may have closed it for anyone to take. The final usage drain runs before the signal.
    ///
    /// # Errors
    /// 409 when the profile has no core that answered and has not been signalled to stop, 502
    /// for an answer other than 2xx or one that is not JSON, or the connection's failure.
    pub async fn management(
        &self,
        profile: &ProfileId,
        method: &Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, AppError> {
        let core = self
            .context
            .state
            .borrow()
            .core(profile)
            .filter(|core| core.phase.reachable())
            .cloned()
            .ok_or_else(AppError::not_started)?;
        self.call(&core, method, path, body).await
    }

    async fn call(
        &self,
        core: &Core,
        method: &Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, AppError> {
        management(
            self.context.backend.as_ref(),
            core.port,
            &core.keys.management,
            method,
            path,
            body,
            MANAGEMENT_TIMEOUT,
        )
        .await
    }

    /// The models the profile's usable core serves.
    ///
    /// # Errors
    /// 503 while the profile is not usable, 502 when the core does not answer with its list.
    pub async fn models(&self, profile: &ProfileId) -> Result<Vec<Model>, AppError> {
        let core = {
            let state = self.context.state.borrow();
            match state.core(profile) {
                Some(core) if state.usable(profile) => core.clone(),
                _ => return Err(AppError::new(STOPPED, 503)),
            }
        };
        let failed = || AppError::new("Could not load the model catalog.", 502);
        let response = self
            .context
            .backend
            .send(Call {
                port: core.port,
                method: Method::GET,
                target: "/v1/models".to_owned(),
                authorization: format!("Bearer {}", core.keys.core.expose_secret()),
                body: None,
                timeout: MANAGEMENT_TIMEOUT,
                limit: MODELS_LIMIT,
            })
            .await
            .map_err(|_| failed())?;
        if !response.ok() {
            return Err(failed());
        }
        let body: Value = serde_json::from_slice(&response.body).map_err(|_| failed())?;
        Ok(body
            .get("data")
            .and_then(Value::as_array)
            .map_or_else(Vec::new, |data| {
                data.iter()
                    .filter_map(|model| {
                        Some(Model {
                            id: model.get("id")?.as_str()?.to_owned(),
                            owned_by: model
                                .get("owned_by")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        })
                    })
                    .collect()
            }))
    }

    /// Profiles start from `binary` from now on; running cores keep theirs.
    ///
    /// # Errors
    /// The pool is gone.
    pub async fn use_binary(&self, binary: PathBuf) -> Result<(), AppError> {
        self.ask(|reply| Command::UseBinary { binary, reply }).await
    }

    /// From now on nothing starts, no request may be sent to a core, and every start still
    /// waiting for its core ends with 503.
    ///
    /// # Errors
    /// The pool is gone.
    pub async fn begin_shutdown(&self) -> Result<(), AppError> {
        self.ask(|reply| Command::BeginShutdown { reply }).await
    }

    /// Stops every profile together, so quitting takes the slowest core's time rather than the
    /// sum, and settles every stop even after a failure.
    ///
    /// # Errors
    /// The failure of the one stop that failed, or all of them.
    pub async fn stop_all(&self) -> Result<(), AppError> {
        let profiles: Vec<ProfileId> = self.state().profiles.keys().cloned().collect();
        let stops: Vec<_> = profiles
            .iter()
            .map(|profile| {
                let pool = self.clone();
                let profile = profile.clone();
                tokio::spawn(async move { pool.stop(&profile).await })
            })
            .collect();
        let mut failures = Vec::new();
        for stop in stops {
            match stop.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(error),
                Err(error) => failures.push(AppError::internal(error)),
            }
        }
        match failures.as_slice() {
            [] => Ok(()),
            [only] => Err(only.clone()),
            all => Err(AppError::internal(format!(
                "Profiles did not stop cleanly: {}",
                all.iter()
                    .map(|error| error.message.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            ))),
        }
    }

    /// Shuts down a pool that runs no features, such as a sign-in's.
    ///
    /// # Errors
    /// See [`Pool::stop_all`].
    pub async fn shutdown(&self) -> Result<(), AppError> {
        self.begin_shutdown().await?;
        self.stop_all().await
    }

    /// Stops cores of a dead earlier instance on this data directory. Run before any core
    /// starts.
    ///
    /// # Errors
    /// The ledger could not be read.
    pub async fn reap_orphans(&self) -> io::Result<()> {
        self.context.backend.reap_orphans().await
    }
}

/// A reserved place in the owner's queue; see [`Pool::reserve`].
pub struct StopPermit(mpsc::OwnedPermit<Command>);

impl StopPermit {
    /// Enqueues the stop now and answers its outcome.
    ///
    /// # Errors
    /// See [`Pool::stop`].
    pub fn stop(self, profile: &ProfileId) -> impl Future<Output = Result<(), AppError>> + use<> {
        let (reply, answer) = oneshot::channel();
        self.0.send(Command::Stop {
            profile: profile.clone(),
            reply,
        });
        async move { answer.await.map_err(|_| AppError::shutting_down())? }
    }
}

/// `http://127.0.0.1:<port>/v0/management<path>` with the management key, as `httpManagement`.
async fn management(
    backend: &dyn Backend,
    port: u16,
    key: &ManagementKey,
    method: &Method,
    path: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<Value, AppError> {
    let response = backend
        .send(Call {
            port,
            method: method.clone(),
            target: format!("/v0/management{path}"),
            authorization: format!("Bearer {}", key.expose_secret()),
            body: body.map(|body| body.to_string().into_bytes()),
            timeout,
            limit: MANAGEMENT_LIMIT,
        })
        .await
        .map_err(|error| AppError::new(format!("Could not reach the proxy: {error}."), 502))?;
    if !response.ok() {
        let status = response.status.as_u16();
        let hint = if status == 429 {
            "Wait a moment and try again."
        } else {
            "Check the profile is running and try again."
        };
        return Err(AppError::new(
            format!("Proxy management returned HTTP {status}. {hint}"),
            502,
        ));
    }
    if response.status.as_u16() == 204 {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_slice(&response.body)
        .map_err(|_| AppError::new("The proxy returned an unreadable answer.", 502))
}
