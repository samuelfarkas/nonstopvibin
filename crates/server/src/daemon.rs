//! The daemon's parts and their order: what starts before the port accepts, what restores once
//! it does, and how it shuts down.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use nsv_accounts::catalog::ModelCatalog;
use nsv_accounts::cores::{Cores, CoresOptions};
use nsv_accounts::fetch::Outside;
use nsv_accounts::imports::ImportCatalog;
use nsv_accounts::pricing::UsagePrices;
use nsv_accounts::scheduler::Timing;
use nsv_accounts::sign_in::Backends;
use nsv_agents::{AgentDirectories, AgentSetup, Connection, ConnectionSource};
use nsv_core::pool::{Backend, Processes, READY};
use nsv_desktop::{HostEvent, TrayStatus};
use nsv_domain::endpoint::{self, Origin};
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::random;
use nsv_domain::secret::SessionToken;
use nsv_domain::sync::locked;
use nsv_files::{DataDir, ServerLock};
use nsv_gateway::Gateway;
use nsv_store::{AuthSnapshot, ProfileFacts, Store, StoreError, Vault, VaultError};
use tokio::net::TcpListener;
use tokio::sync::{Notify, OnceCell, broadcast, watch};

use crate::http::{GatewayHost, Phase, valid_allow_host};
use crate::session::Session;

/// This build's version, as `package.json` states it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How the daemon runs.
pub struct Options {
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
    /// Launches a data directory's cores: [`processes`] outside tests.
    pub backends: Backends,
    /// The outside requests: models.dev, the euro rate, provider APIs, the core feed and
    /// release downloads.
    pub outside: Arc<dyn Outside>,
    /// Milliseconds since the epoch, for usage periods, pricing and retention.
    pub clock: fn() -> i64,
    /// The scheduler's periods: [`nsv_accounts::scheduler::TIMING`] outside tests.
    pub timing: Timing,
}

/// Each data directory's cores run as processes of the core binary.
#[must_use]
pub fn processes() -> Backends {
    Arc::new(|directory: &DataDir| -> Arc<dyn Backend> { Arc::new(Processes::new(directory)) })
}

/// Which core profiles start from.
#[derive(Debug, Clone)]
pub struct CoreOptions {
    /// The core this build bundles.
    pub binary: PathBuf,
    /// Its hash must also match the pin compiled into this build: headless releases.
    pub pinned: bool,
    /// Where an installed build reads the reviewed core pin (`NONSTOPVIBIN_CORE_FEED`). With
    /// one, a core update installed over the bundled one runs instead; a development run has
    /// none, and never selects or deletes installed cores.
    pub feed: Option<String>,
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

/// The tray's view: serving profiles by name, and whether any request streams. Only the
/// desktop keeps it current.
pub(crate) struct Tray {
    status: watch::Sender<TrayStatus>,
    facts: watch::Receiver<ProfileFacts>,
}

impl Tray {
    fn new(facts: watch::Receiver<ProfileFacts>) -> Self {
        Self {
            status: watch::Sender::new(TrayStatus::default()),
            facts,
        }
    }

    pub(crate) fn has(&self, profile: &ProfileId) -> bool {
        self.facts.borrow().iter().any(|fact| fact.id == *profile)
    }

    /// `Application.status()`: a dormant profile still serves, since its core starts on the next
    /// request. Tells the tray only when something it shows changed.
    pub(crate) fn update(&self, cores: &Cores) {
        let facts = Arc::clone(&self.facts.borrow());
        let status = cores.statuses(|status| {
            let mut tray = TrayStatus::default();
            for fact in facts.iter() {
                let status = status(&fact.id);
                if status.serving {
                    tray.running.push(fact.name.clone());
                    tray.streaming |= status.streaming;
                }
            }
            tray
        });
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

/// Everything a request may reach.
pub(crate) struct App {
    pub(crate) shared: Arc<Shared>,
    pub(crate) directory: DataDir,
    pub(crate) allow_hosts: Vec<String>,
    pub(crate) development: bool,
    pub(crate) client: PathBuf,
    pub(crate) clock: fn() -> i64,
    pub(crate) port: u16,
    pub(crate) origin: Origin,
    pub(crate) base: url::Url,
    pub(crate) token: SessionToken,
    /// Starts every static `ETag`: file times in a package are not trustworthy, so unhashed
    /// files revalidate after each launch.
    pub(crate) asset_version: String,
    pub(crate) store: Store,
    /// The scheduler, its pool and the account features: sign-in, credentials, quota, resets
    /// and core updates.
    pub(crate) cores: Arc<Cores>,
    pub(crate) gateway: Gateway<GatewayHost>,
    pub(crate) agents: AgentSetup,
    pub(crate) catalog: Arc<ModelCatalog>,
    pub(crate) prices: Arc<UsagePrices>,
    pub(crate) imports: Arc<Mutex<ImportCatalog>>,
    pub(crate) phase: watch::Sender<Phase>,
    lock: Mutex<Option<ServerLock>>,
    started_at: Mutex<Option<i64>>,
    closed: OnceCell<Result<(), CloseError>>,
}

/// The running daemon. Clones share it.
pub struct Daemon(Arc<App>);

impl Clone for Daemon {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl Daemon {
    /// Opens the store, prunes old usage and starts accepting on the loopback port; requests
    /// are served from here on. Then [`Daemon::restore`] starts the profiles.
    ///
    /// # Errors
    /// An invalid allowed host, a port in use, or a store that does not open.
    pub async fn start(options: Options, lock: ServerLock) -> Result<Self, StartError> {
        if let Some(host) = options
            .allow_hosts
            .iter()
            .find(|host| !valid_allow_host(host))
        {
            return Err(StartError::InvalidHost(
                serde_json::Value::String(host.clone()).to_string(),
            ));
        }
        let directory = lock.directory().clone();
        let vault = tokio::task::spawn_blocking(move || Vault::open(&directory))
            .await
            .map_err(|error| StartError::Other(error.to_string()))??;
        let store = Store::open(lock.directory(), vault).await?;
        match Self::assemble(options, lock, store.clone()).await {
            Ok(daemon) => Ok(daemon),
            Err(error) => {
                store.close().await;
                Err(error)
            }
        }
    }

    async fn assemble(
        options: Options,
        lock: ServerLock,
        store: Store,
    ) -> Result<Self, StartError> {
        let now = UNIX_EPOCH + Duration::from_millis(u64::try_from((options.clock)()).unwrap_or(0));
        store.call(move |db| db.prune_usage(now)).await?;
        let listener = bind(options.port)?;
        let port = listener.local_addr()?.port();
        let origin_text = endpoint::loopback_origin(port);
        let origin =
            Origin::parse(&origin_text).map_err(|error| StartError::Other(error.to_string()))?;
        let base =
            url::Url::parse(&origin_text).map_err(|error| StartError::Other(error.to_string()))?;
        let shared = Arc::new(Shared {
            errors: Errors::default(),
            tray: Tray::new(store.facts()),
            desktop: options.desktop,
            events: broadcast::Sender::new(8),
            closing: AtomicBool::new(false),
            stop: Notify::new(),
        });
        let (reported, accepted) = (Arc::clone(&shared), Arc::clone(&shared));
        let cores = Arc::new(Cores::new(CoresOptions {
            store: store.clone(),
            binary: options.core.binary,
            backends: options.backends,
            ready: READY,
            timing: options.timing,
            outside: Arc::clone(&options.outside),
            feed: options.core.feed,
            require_pinned_core: options.core.pinned,
            allow_missing_core: options.core.allow_missing,
            clock: options.clock,
            report: Arc::new(move |message| reported.errors.report(message)),
            sign_in_accepted: Arc::new(move || accepted.sign_in_accepted()),
        })?);
        let catalog = Arc::new(ModelCatalog::new(
            Arc::clone(&options.outside),
            options.clock,
        ));
        let prices = Arc::new(UsagePrices::new(options.outside, options.clock));
        let gateway = Gateway::new(GatewayHost {
            shared: Arc::clone(&shared),
            auth: store.auth(),
            profiles: Mutex::default(),
            cores: Arc::clone(&cores),
            catalog: Arc::clone(&catalog),
            store: store.clone(),
        });
        let agents = AgentSetup::new(
            lock.directory(),
            connection_source(&shared, &store, &cores, port),
            options.agents.clone(),
        );
        let token = SessionToken::generate()?;
        let app = Arc::new(App {
            imports: Arc::new(Mutex::new(ImportCatalog::new(&options.agents.home))),
            shared,
            directory: lock.directory().clone(),
            allow_hosts: options.allow_hosts,
            development: options.development,
            client: options.client,
            clock: options.clock,
            port,
            origin,
            base,
            token,
            asset_version: random::uuid()?,
            store,
            cores,
            gateway,
            agents,
            catalog,
            prices,
            phase: watch::Sender::new(Phase::Serving),
            lock: Mutex::new(Some(lock)),
            started_at: Mutex::new(None),
            closed: OnceCell::new(),
        });
        crate::http::serve(&app, listener);
        if app.shared.desktop {
            follow_tray(&app);
        }
        Ok(Self(app))
    }

    /// Selects and checks the core, reaps cores and pending sign-ins an earlier run left,
    /// starts the profiles that are kept running (the others start on their first request),
    /// checks for a core update in the background, restores agent connections, and writes
    /// `server.json`. Dropping it part-way is safe: [`Daemon::close`] stops whatever
    /// it started.
    ///
    /// # Errors
    /// A core that is missing or fails its integrity check, a ledger or store that cannot be
    /// read, or shutdown beginning meanwhile.
    pub async fn restore(&self) -> Result<(), StartError> {
        let app = &self.0;
        app.cores.launch().await?;
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
            started_at: (app.clock)(),
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
        let started_at = (*locked(&self.0.started_at))?;
        Some(Session {
            pid: std::process::id(),
            port: self.0.port,
            token: self.0.token.clone(),
            allow_hosts: self.0.allow_hosts.clone(),
            version: VERSION.to_owned(),
            started_at,
        })
    }

    /// `http://127.0.0.1:<port>`: the only origin the desktop's windows show.
    #[must_use]
    pub fn origin(&self) -> &Origin {
        &self.0.origin
    }

    /// The session token the desktop's window helper hands the UI.
    #[must_use]
    pub fn session_token(&self) -> &SessionToken {
        &self.0.token
    }

    #[must_use]
    pub fn data_directory(&self) -> &DataDir {
        &self.0.directory
    }

    /// The tray's view of the running profiles; only the desktop keeps it current.
    #[must_use]
    pub fn tray_status(&self) -> watch::Receiver<TrayStatus> {
        self.0.shared.tray.status.subscribe()
    }

    /// Sign-ins and second launches, for the desktop shell.
    #[must_use]
    pub fn events(&self) -> broadcast::Receiver<HostEvent> {
        self.0.shared.events.subscribe()
    }

    /// Whether the tray knows `profile`.
    #[must_use]
    pub fn has_profile(&self, profile: &ProfileId) -> bool {
        self.0.shared.tray.has(profile)
    }

    /// Shows an error in the UI's error list.
    pub fn report(&self, message: &str) {
        self.0.shared.errors.report(message.to_owned());
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

impl App {
    pub(crate) fn update_tray(&self) {
        if self.shared.desktop {
            self.shared.tray.update(&self.cores);
        }
    }

    async fn close_resources(&self) -> Result<(), CloseError> {
        let mut failures = Vec::new();
        self.shared.closing.store(true, Ordering::Release);
        self.phase.send_replace(Phase::Draining);
        if let Err(error) = self.agents.close().await {
            failures.push(error.to_string());
        }
        if let Err(error) = self.cores.shutdown().await {
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

/// The desktop tray follows each change the pool or the store's profile facts publish.
fn follow_tray(app: &Arc<App>) {
    let weak = Arc::downgrade(app);
    let mut pool = app.cores.changes();
    let mut facts = app.store.facts();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                published = pool.changed() => if published.is_err() { return },
                // A closed store publishes nothing more: its branch is skipped.
                Ok(()) = facts.changed() => {}
            }
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
    cores: &Arc<Cores>,
    port: u16,
) -> ConnectionSource {
    let shared = Arc::clone(shared);
    let auth: watch::Receiver<Arc<AuthSnapshot>> = store.auth();
    let cores = Arc::clone(cores);
    Arc::new(move |profile: &ProfileId| {
        let key = {
            let snapshot = auth.borrow();
            snapshot
                .profile(profile)
                .map_err(AppError::internal)?
                .map(|entry| entry.client_key.clone())
        }
        .ok_or_else(|| AppError::new("Profile not found.", 404))?;
        if shared.closing() || !cores.status(profile).serving {
            return Err(AppError::new(
                "Start this profile before connecting an agent.",
                409,
            ));
        }
        Ok(Connection { key, port })
    })
}
