//! The daemon's parts and their order: what starts before the port accepts, what restores once
//! it does, and how it shuts down.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use nsv_accounts::accounts::Accounts;
use nsv_accounts::catalog::ModelCatalog;
use nsv_accounts::fetch::Fetch;
use nsv_accounts::imports::ImportCatalog;
use nsv_accounts::pricing::UsagePrices;
use nsv_accounts::scheduler::{self, Scheduler, TIMING};
use nsv_agents::{AgentDirectories, AgentSetup, Connection, ConnectionSource};
use nsv_core::binary::{CoreTarget, bundled_pin, verify_core};
use nsv_core::install::select_installed_core;
use nsv_core::pool::{BoxFuture, Pool, PoolState, Processes, READY};
use nsv_domain::error::AppError;
use nsv_domain::helper::Origin;
use nsv_domain::id::ProfileId;
use nsv_domain::secret::SessionToken;
use nsv_domain::time::iso_string;
use nsv_gateway::Gateway;
use nsv_shell::{HostEvent, ShellHost, TrayStatus};
use nsv_store::layout::cores_directory;
use nsv_store::{AuthSnapshot, ProfileNames, ServerLock, Store, StoreError, Vault, VaultError};
use tokio::net::TcpListener;
use tokio::sync::{Notify, OnceCell, broadcast, watch};

use crate::features::{FeatureContext, FeatureHost, Features};
use crate::http::{GatewayHost, Phase, valid_allow_host};
use crate::session::Session;

/// This build's version, as `package.json` states it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How the daemon runs.
pub struct Options<F> {
    /// The data directory, whose lock the caller holds.
    pub directory: PathBuf,
    /// The loopback port; 0 takes any free one.
    pub port: u16,
    /// Exact DNS names an HTTPS reverse proxy (such as Tailscale Serve) uses for the interface
    /// and the management API. The agent gateway stays loopback-only.
    pub allow_hosts: Vec<String>,
    /// Accepts the Vite development server's Host and Origin.
    pub development: bool,
    /// The desktop app: `POST /api/desktop/show`, and the tray's status.
    pub desktop: bool,
    /// The built interface, an absolute path.
    pub client: PathBuf,
    pub core: CoreOptions,
    pub agents: AgentDirectories,
    /// The outside requests: models.dev, the euro rate and what the features need.
    pub fetch: F,
    /// Milliseconds since the epoch, for usage periods, pricing and retention.
    pub clock: fn() -> i64,
}

/// Which core profiles start from.
#[derive(Debug, Clone)]
pub struct CoreOptions {
    /// The core this build bundles.
    pub binary: PathBuf,
    /// Its hash must also match the pin compiled into this build: headless releases.
    pub pinned: bool,
    /// An installed build: a reviewed core update installed over the bundled one runs instead.
    pub updates: bool,
    /// A development run starts without it; each profile start then reports it missing.
    pub allow_missing: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("Invalid allowed host: {0}")]
    InvalidHost(String),
    #[error(
        "Port {0} on 127.0.0.1 is already in use. Stop the other program, or choose another port with --port."
    )]
    PortInUse(u16),
    #[error(transparent)]
    App(#[from] AppError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Vault(#[from] VaultError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("{0}")]
    Other(String),
}

/// What closing failed to do; every later step still ran.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct CloseError(String);

pub(crate) fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The error list the interface shows: the last five distinct messages, oldest first.
#[derive(Debug, Default)]
pub(crate) struct Errors(Mutex<Vec<String>>);

impl Errors {
    pub(crate) fn report(&self, message: String) {
        let mut list = locked(&self.0);
        list.retain(|known| *known != message);
        list.push(message);
        let excess = list.len().saturating_sub(5);
        list.drain(..excess);
    }

    pub(crate) fn list(&self) -> Vec<String> {
        locked(&self.0).clone()
    }
}

/// The tray's view: running profiles by name, and whether any request streams. Only the
/// desktop keeps it current.
pub(crate) struct Tray {
    status: watch::Sender<TrayStatus>,
    names: Mutex<ProfileNames>,
    /// Profiles with a gateway request or WebSocket open.
    open: Mutex<BTreeSet<ProfileId>>,
}

impl Tray {
    fn new() -> Self {
        Self {
            status: watch::Sender::new(TrayStatus::default()),
            names: Mutex::new(Arc::from([])),
            open: Mutex::new(BTreeSet::new()),
        }
    }

    pub(crate) fn has(&self, profile: &ProfileId) -> bool {
        locked(&self.names).iter().any(|(id, _)| id == profile)
    }

    pub(crate) fn opened(&self, profile: &ProfileId, open: bool) {
        let mut set = locked(&self.open);
        if open {
            set.insert(profile.clone());
        } else {
            set.remove(profile);
        }
    }

    /// `Application.status()`: a dormant profile still serves, since its core starts on the next
    /// request. Tells the tray only when something it shows changed.
    pub(crate) fn update(&self, pool: &PoolState, scheduler: &Scheduler) {
        let names = Arc::clone(&locked(&self.names));
        let open = locked(&self.open).clone();
        let mut status = TrayStatus::default();
        for (id, name) in names.iter() {
            if pool.running(id) || scheduler.is_dormant(id) {
                status.running.push(name.clone());
                status.streaming |= open.contains(id);
            }
        }
        self.status.send_if_modified(|current| {
            let changed = *current != status;
            *current = status;
            changed
        });
    }
}

/// What the daemon's hosts, features and shell share.
pub(crate) struct Shared {
    pub(crate) errors: Errors,
    pub(crate) tray: Tray,
    pub(crate) desktop: bool,
    events: broadcast::Sender<HostEvent>,
    closing: AtomicBool,
    stop: Notify,
}

impl Shared {
    pub(crate) fn sign_in_accepted(&self) {
        // No receiver means no shell: nothing to bring forward.
        let _ = self.events.send(HostEvent::SignInAccepted);
    }

    pub(crate) fn show_main(&self) {
        let _ = self.events.send(HostEvent::ShowMain);
    }

    pub(crate) fn closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }
}

/// The scheduler's view of the daemon. The features exist only once the scheduler's pool does.
struct SchedulerHost {
    shared: Arc<Shared>,
    features: OnceLock<Arc<dyn Features>>,
}

impl scheduler::Host for SchedulerHost {
    fn report(&self, message: String) {
        self.shared.errors.report(message);
    }

    fn signing_in(&self, profile: &ProfileId) -> bool {
        self.features
            .get()
            .is_some_and(|features| features.signing_in(profile))
    }

    fn refresh_quotas(
        &self,
        pool: Pool,
        profile: ProfileId,
    ) -> BoxFuture<'static, Result<(), AppError>> {
        match self.features.get() {
            Some(features) => features.refresh_quotas(pool, profile),
            None => Box::pin(async { Ok(()) }),
        }
    }
}

/// Everything a request may reach.
pub(crate) struct App<F> {
    pub(crate) shared: Arc<Shared>,
    pub(crate) directory: PathBuf,
    pub(crate) allow_hosts: Vec<String>,
    pub(crate) development: bool,
    pub(crate) client: PathBuf,
    core: CoreOptions,
    pub(crate) clock: fn() -> i64,
    pub(crate) port: u16,
    pub(crate) origin: Origin,
    pub(crate) base: url::Url,
    pub(crate) token: SessionToken,
    /// Starts every static `ETag`: file times in a package are not trustworthy, so unhashed
    /// files revalidate after each launch.
    pub(crate) asset_version: String,
    pub(crate) store: Store,
    pub(crate) scheduler: Scheduler,
    pub(crate) gateway: Gateway<GatewayHost<F>>,
    pub(crate) agents: AgentSetup,
    pub(crate) catalog: Arc<ModelCatalog<F>>,
    pub(crate) prices: Arc<UsagePrices<F>>,
    pub(crate) imports: Arc<Mutex<ImportCatalog>>,
    pub(crate) features: Arc<dyn Features>,
    /// The version of the core profiles start from.
    pub(crate) core_version: Mutex<String>,
    pub(crate) phase: watch::Sender<Phase>,
    lock: Mutex<Option<ServerLock>>,
    started_at: Mutex<Option<String>>,
    closed: OnceCell<Result<(), CloseError>>,
}

/// The running daemon. Clones share it.
pub struct Daemon<F: Fetch + Clone + 'static>(Arc<App<F>>);

impl<F: Fetch + Clone + 'static> Clone for Daemon<F> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes)?;
    Ok(bytes)
}

/// `crypto.randomUUID()`.
fn random_uuid() -> io::Result<String> {
    let mut bytes = random::<16>()?;
    if let Some(version) = bytes.get_mut(6) {
        *version = *version & 0x0f | 0x40;
    }
    if let Some(variant) = bytes.get_mut(8) {
        *variant = *variant & 0x3f | 0x80;
    }
    let hex = bytes.iter().fold(String::new(), |mut hex, byte| {
        let _infallible = write!(hex, "{byte:02x}");
        hex
    });
    Ok([0..8, 8..12, 12..16, 16..20, 20..32]
        .into_iter()
        .filter_map(|range| hex.get(range))
        .collect::<Vec<_>>()
        .join("-"))
}

impl<F: Fetch + Clone + 'static> Daemon<F> {
    /// Opens the store, prunes old usage and starts accepting on the loopback port; requests
    /// are served from here on. Then [`Daemon::restore`] starts the profiles.
    ///
    /// # Errors
    /// An invalid allowed host, a port in use, or a store that does not open.
    pub async fn start(
        options: Options<F>,
        lock: ServerLock,
        features: impl FnOnce(FeatureContext<F>) -> Arc<dyn Features>,
    ) -> Result<Self, StartError> {
        if let Some(host) = options
            .allow_hosts
            .iter()
            .find(|host| !valid_allow_host(host))
        {
            return Err(StartError::InvalidHost(
                serde_json::Value::String(host.clone()).to_string(),
            ));
        }
        let vault = Vault::open(&options.directory)?;
        let store = Store::open(&options.directory, vault).await?;
        match Self::assemble(options, lock, store.clone(), features).await {
            Ok(daemon) => Ok(daemon),
            Err(error) => {
                store.close().await;
                Err(error)
            }
        }
    }

    async fn assemble(
        options: Options<F>,
        lock: ServerLock,
        store: Store,
        features: impl FnOnce(FeatureContext<F>) -> Arc<dyn Features>,
    ) -> Result<Self, StartError> {
        let now = UNIX_EPOCH + Duration::from_millis(u64::try_from((options.clock)()).unwrap_or(0));
        store.call(move |db| db.prune_usage(now)).await?;
        let listener = bind(options.port)?;
        let port = listener.local_addr()?.port();
        let origin_text = format!("http://127.0.0.1:{port}");
        let origin =
            Origin::parse(&origin_text).map_err(|error| StartError::Other(error.to_string()))?;
        let base =
            url::Url::parse(&origin_text).map_err(|error| StartError::Other(error.to_string()))?;
        let shared = Arc::new(Shared {
            errors: Errors::default(),
            tray: Tray::new(),
            desktop: options.desktop,
            events: broadcast::Sender::new(8),
            closing: AtomicBool::new(false),
            stop: Notify::new(),
        });
        let accounts = Arc::new(Accounts::new(store.clone()));
        let scheduler_host = Arc::new(SchedulerHost {
            shared: Arc::clone(&shared),
            features: OnceLock::new(),
        });
        let host: Arc<dyn scheduler::Host> = Arc::<SchedulerHost>::clone(&scheduler_host);
        let scheduler = Scheduler::new(
            store.clone(),
            options.core.binary.clone(),
            Arc::new(Processes::new(&options.directory)),
            READY,
            Arc::clone(&accounts),
            host,
            TIMING,
        );
        let features = features(FeatureContext {
            store: store.clone(),
            pool: scheduler.pool().clone(),
            accounts,
            directory: options.directory.clone(),
            fetch: options.fetch.clone(),
            host: FeatureHost(Arc::clone(&shared)),
        });
        // Set once, here, before anything is scheduled.
        let _ = scheduler_host.features.set(Arc::clone(&features));
        let catalog = Arc::new(ModelCatalog::new(options.fetch.clone(), options.clock));
        let prices = Arc::new(UsagePrices::new(options.fetch, options.clock));
        let gateway = Gateway::new(GatewayHost {
            shared: Arc::clone(&shared),
            auth: store.auth(),
            scheduler: scheduler.clone(),
            catalog: Arc::clone(&catalog),
            store: store.clone(),
        })
        .map_err(|error| StartError::Other(error.to_string()))?;
        let agents = AgentSetup::new(
            &options.directory,
            connection_source(&shared, &store, &scheduler, port),
            options.agents.clone(),
        )
        .map_err(|error| StartError::Other(error.to_string()))?;
        let token = SessionToken::new(URL_SAFE_NO_PAD.encode(random::<32>()?));
        let app = Arc::new(App {
            imports: Arc::new(Mutex::new(ImportCatalog::new(&options.agents.home))),
            shared,
            directory: options.directory,
            allow_hosts: options.allow_hosts,
            development: options.development,
            client: options.client,
            core: options.core,
            clock: options.clock,
            port,
            origin,
            base,
            token,
            asset_version: random_uuid()?,
            store,
            scheduler,
            gateway,
            agents,
            catalog,
            prices,
            features,
            core_version: Mutex::new(String::new()),
            phase: watch::Sender::new(Phase::Serving),
            lock: Mutex::new(Some(lock)),
            started_at: Mutex::new(None),
            closed: OnceCell::new(),
        });
        app.refresh_names().await?;
        crate::http::serve(&app, listener);
        if app.shared.desktop {
            follow_pool(&app);
        }
        Ok(Self(app))
    }

    /// Selects and checks the core, reaps cores an earlier run left, starts the profiles that
    /// are kept running (the others start on their first request), restores agent connections,
    /// and writes `server.json`. Dropping it part-way is safe: [`Daemon::close`] stops whatever
    /// it started.
    ///
    /// # Errors
    /// A core that is missing or fails its integrity check, a ledger or store that cannot be
    /// read, or shutdown beginning meanwhile.
    pub async fn restore(&self) -> Result<(), StartError> {
        let app = &self.0;
        let version = app.select_core().await?;
        *locked(&app.core_version) = version;
        app.scheduler.pool().reap_orphans().await?;
        app.features.restore().await?;
        app.scheduler.launch().await?;
        app.features.launched();
        let profiles = app.store.call(|db| db.profiles()).await?;
        match app.agents.restore(&profiles, app.port).await {
            Ok(failures) => {
                for failure in failures {
                    app.shared.errors.report(failure);
                }
            }
            Err(error) => {
                tracing::warn!("agent connections were not restored: {error}");
                app.shared.errors.report(
                    "Agent connections are unavailable. Open Connect agents and reconnect the agent."
                        .to_owned(),
                );
            }
        }
        app.update_tray();
        let session = Session {
            pid: std::process::id(),
            port: app.port,
            token: app.token.clone(),
            allow_hosts: app.allow_hosts.clone(),
            version: VERSION.to_owned(),
            started_at: iso_string((app.clock)()),
        };
        session.write(&app.directory)?;
        *locked(&app.started_at) = Some(session.started_at);
        Ok(())
    }

    /// The loopback port it accepts on.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.0.port
    }

    /// What `server.json` says once [`Daemon::restore`] wrote it.
    #[must_use]
    pub fn session(&self) -> Option<Session> {
        let started_at = locked(&self.0.started_at).clone()?;
        Some(Session {
            pid: std::process::id(),
            port: self.0.port,
            token: self.0.token.clone(),
            allow_hosts: self.0.allow_hosts.clone(),
            version: VERSION.to_owned(),
            started_at,
        })
    }

    /// Asks the daemon's owner to shut it down: [`Daemon::stopping`] resolves.
    pub fn stop(&self) {
        self.0.shared.stop.notify_one();
    }

    /// Resolves once Quit was chosen or [`Daemon::stop`] was called.
    pub async fn stopping(&self) {
        self.0.shared.stop.notified().await;
    }

    /// Stops accepting and lets open requests finish, closes the credential socket, lets the
    /// features and the scheduler settle, drains every core's usage and stops it, ends the
    /// connections still open, closes the store, removes `server.json` if it still names this
    /// process and releases the data directory. Runs once; later calls wait for it.
    ///
    /// # Errors
    /// The steps that failed; the others still ran.
    pub async fn close(&self) -> Result<(), CloseError> {
        let app = &self.0;
        app.closed
            .get_or_init(|| app.close_resources())
            .await
            .clone()
    }
}

impl<F: Fetch + Clone + 'static> App<F> {
    async fn select_core(&self) -> Result<String, StartError> {
        let pin = bundled_pin().map_err(|error| StartError::Other(error.to_string()))?;
        if self.core.updates {
            let cores = cores_directory(&self.directory);
            let target = CoreTarget::current()?;
            let selected = tokio::task::spawn_blocking(move || {
                select_installed_core(&cores, pin.version, target)
            })
            .await
            .map_err(|error| StartError::Other(error.to_string()))?
            .map_err(|error| StartError::Other(error.to_string()))?;
            if let Some(active) = selected {
                self.scheduler.pool().use_binary(active.binary).await?;
                return Ok(active.version.to_string());
            }
        }
        let binary = self.core.binary.clone();
        if !self.core.allow_missing || binary.exists() {
            let pinned = self.core.pinned.then_some(pin);
            tokio::task::spawn_blocking(move || verify_core(&binary, pinned))
                .await
                .map_err(|error| StartError::Other(error.to_string()))?
                .map_err(|error| StartError::Other(error.to_string()))?;
        }
        Ok(pin.version.to_string())
    }

    /// An installed build, which may run a core update.
    pub(crate) const fn updates(&self) -> bool {
        self.core.updates
    }

    /// Reads the profile names again after a profile was created or renamed.
    pub(crate) async fn refresh_names(&self) -> Result<(), StoreError> {
        if self.shared.desktop {
            let names = self.store.call(nsv_store::Db::profile_names).await?;
            *locked(&self.shared.tray.names) = names;
            self.update_tray();
        }
        Ok(())
    }

    pub(crate) fn update_tray(&self) {
        if self.shared.desktop {
            self.shared
                .tray
                .update(&self.scheduler.pool().state(), &self.scheduler);
        }
    }

    async fn close_resources(&self) -> Result<(), CloseError> {
        let mut failures = Vec::new();
        self.shared.closing.store(true, Ordering::Release);
        self.phase.send_replace(Phase::Draining);
        if let Err(error) = self.agents.close().await {
            failures.push(error.to_string());
        }
        self.scheduler.begin_shutdown().await;
        self.features.shutdown().await;
        self.scheduler.settled().await;
        if let Err(error) = self.scheduler.pool().stop_all().await {
            failures.push(error.message);
        }
        self.prices.close().await;
        self.phase.send_replace(Phase::Closed);
        self.store.close().await;
        if let Err(error) = Session::remove(&self.directory, std::process::id()) {
            failures.push(error.to_string());
        }
        if let Some(lock) = locked(&self.lock).take() {
            lock.release();
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(CloseError(failures.join(" ")))
        }
    }
}

fn bind(port: u16) -> Result<TcpListener, StartError> {
    let listener = std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        .map_err(|error| match error.kind() {
            io::ErrorKind::AddrInUse => StartError::PortInUse(port),
            _ => StartError::Io(error),
        })?;
    listener.set_nonblocking(true)?;
    Ok(TcpListener::from_std(listener)?)
}

/// The desktop tray follows each change the pool publishes.
fn follow_pool<F: Fetch + Clone + 'static>(app: &Arc<App<F>>) {
    let weak = Arc::downgrade(app);
    let mut changes = app.scheduler.pool().subscribe();
    tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            let Some(app) = weak.upgrade() else { return };
            app.update_tray();
        }
    });
}

/// What the credential socket answers: the profile's key and the port, while its core runs or
/// sleeps until the agent's first request.
fn connection_source(
    shared: &Arc<Shared>,
    store: &Store,
    scheduler: &Scheduler,
    port: u16,
) -> ConnectionSource {
    let shared = Arc::clone(shared);
    let auth: watch::Receiver<Arc<AuthSnapshot>> = store.auth();
    let scheduler = scheduler.clone();
    Arc::new(move |profile: &ProfileId| {
        let key = {
            let snapshot = auth.borrow();
            snapshot
                .profile(profile)
                .map_err(|error| AppError::new(error.to_string(), 500))?
                .map(|entry| entry.client_key.clone())
        }
        .ok_or_else(|| AppError::new("Profile not found.", 404))?;
        let serving = scheduler.pool().state().running(profile) || scheduler.is_dormant(profile);
        if shared.closing() || !serving {
            return Err(AppError::new(
                "Start this profile before connecting an agent.",
                409,
            ));
        }
        Ok(Connection { key, port })
    })
}

impl<F: Fetch + Clone + 'static> ShellHost for Daemon<F> {
    fn origin(&self) -> &Origin {
        &self.0.origin
    }

    fn session_token(&self) -> &SessionToken {
        &self.0.token
    }

    fn data_directory(&self) -> &Path {
        &self.0.directory
    }

    fn tray_status(&self) -> watch::Receiver<TrayStatus> {
        self.0.shared.tray.status.subscribe()
    }

    fn events(&self) -> broadcast::Receiver<HostEvent> {
        self.0.shared.events.subscribe()
    }

    fn has_profile(&self, profile: &ProfileId) -> bool {
        self.0.shared.tray.has(profile)
    }

    fn report(&self, message: &str) {
        self.0.shared.errors.report(message.to_owned());
    }

    fn shutdown(&self) {
        self.stop();
    }
}
