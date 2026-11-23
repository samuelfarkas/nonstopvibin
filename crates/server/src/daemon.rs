//! The daemon's parts and their order: what starts before the port accepts, what restores once
//! it does, and how it shuts down.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hyper::body::Incoming;
use hyper::{Request, Response};
use nsv_accounts::Backends;
use nsv_accounts::catalog::ModelCatalog;
use nsv_accounts::cores::{Cores, CoresOptions};
use nsv_accounts::fetch::Outside;
use nsv_accounts::imports::ImportCatalog;
use nsv_accounts::pricing::UsagePrices;
use nsv_accounts::scheduler::{TIMING, Timing};
use nsv_agents::{AgentDirectories, AgentSetup, Connection, ConnectionSource};
use nsv_core::install::CoreSource;
use nsv_core::pool::{Backend, Processes, READY};
use nsv_domain::endpoint::Origin;
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::secret::SessionToken;
use nsv_domain::sync::locked;
use nsv_domain::time::epoch_ms;
use nsv_domain::version::AppVersion;
use nsv_files::{DataDir, ServerLock};
use nsv_gateway::{Body, Gateway, Refusal};
use nsv_net::{Https, NetError};
use nsv_store::{AuthSnapshot, Store, StoreError, Vault, VaultError};
use tokio::sync::{OnceCell, watch};
use tokio::task::JoinError;

use crate::api::Api;
use crate::assets::Assets;
use crate::checks::{Admission, AllowedHost, Checks};
use crate::desktop::Desktop;
use crate::failure::{self, Errors};
use crate::gateway_host::GatewayHost;
use crate::listener::{self, Phase, advance};
use crate::session::Session;
use crate::shared::{Shared, VERSION};

/// How the daemon runs.
pub struct Options {
    /// The loopback port; 0 takes any free one.
    pub port: u16,
    /// The names an HTTPS reverse proxy (such as Tailscale Serve) uses for the interface and the
    /// management API. The agent gateway stays loopback-only.
    pub allow_hosts: Vec<AllowedHost>,
    /// Accepts the Vite development server's Host and Origin.
    pub development: bool,
    /// The desktop app's part: `POST /api/desktop/show`, the tray's status and the shell's
    /// events.
    pub desktop: Option<Desktop>,
    /// The built interface, an absolute path.
    pub client: PathBuf,
    pub core: CoreOptions,
    pub agents: AgentDirectories,
}

/// What the daemon reaches outside itself; tests replace each.
pub struct Boundaries {
    /// Launches a data directory's cores.
    pub backends: Backends,
    /// The outside requests: models.dev, the euro rate, provider APIs, the core feed and
    /// release downloads.
    pub outside: Arc<dyn Outside>,
    /// Milliseconds since the epoch, for usage periods, pricing and retention.
    pub clock: fn() -> i64,
    /// The scheduler's periods.
    pub timing: Timing,
}

impl Boundaries {
    /// Cores as processes of the core binary, HTTPS, the system clock and the scheduler's
    /// periods.
    ///
    /// # Errors
    /// When the TLS client cannot be configured.
    pub fn production() -> Result<Self, NetError> {
        Ok(Self {
            backends: Arc::new(|directory: &DataDir| -> Arc<dyn Backend> {
                Arc::new(Processes::new(directory))
            }),
            outside: Arc::new(Https::new()?),
            clock: || epoch_ms(SystemTime::now()),
            timing: *TIMING,
        })
    }
}

/// Which core profiles start from.
#[derive(Debug, Clone)]
pub struct CoreOptions {
    /// The core this build bundles.
    pub binary: PathBuf,
    pub source: CoreSource,
}

/// What kept the daemon from starting or restoring.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(
        "Port {0} on 127.0.0.1 is already in use. Stop the other program, or choose another port with --port."
    )]
    PortInUse(u16),
    /// The loopback port could not be bound or read back.
    #[error(transparent)]
    Listen(io::Error),
    /// The vault key could not be read or created.
    #[error(transparent)]
    Vault(#[from] VaultError),
    /// The task that opens the vault ended without an answer.
    #[error(transparent)]
    VaultTask(#[from] JoinError),
    /// The store did not open, prune old usage or list the profiles.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// No entropy for the session token or the interface's version.
    #[error(transparent)]
    Entropy(io::Error),
    /// The cores could not be set up, or the core could not be selected and checked.
    #[error(transparent)]
    Cores(AppError),
    /// `server.json` could not be written.
    #[error(transparent)]
    Session(io::Error),
}

/// What closing failed to do; every later step still ran.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct CloseError(String);

/// The running daemon's parts.
pub(crate) struct App {
    shared: Arc<Shared>,
    checks: Checks,
    api: Api,
    assets: Assets,
    gateway: Gateway<GatewayHost>,
    store: Store,
    /// The scheduler, its pool and the account features: sign-in, credentials, quota, resets
    /// and core updates.
    cores: Arc<Cores>,
    agents: Arc<AgentSetup>,
    prices: Arc<UsagePrices>,
    phase: watch::Sender<Phase>,
    directory: DataDir,
    lock: Mutex<Option<ServerLock>>,
    /// What `server.json` says, once [`Daemon::restore`] wrote it.
    session: OnceLock<Session>,
    /// The close's outcome: it runs once, and a caller dropped part-way lets the next one run it.
    closed: OnceCell<Result<(), CloseError>>,
}

impl App {
    /// Answers a request once it passed the checks, and the refusal otherwise.
    pub(crate) async fn dispatch(&self, request: Request<Incoming>) -> Response<Body> {
        let answered = match self.checks.admit(&request) {
            Admission::Closed(status) => return failure::closed(status),
            Admission::WebSocket => return self.gateway.upgrade(request).await,
            Admission::Gateway => {
                return self
                    .gateway
                    .proxy(request)
                    .await
                    .unwrap_or_else(Refusal::into_response);
            }
            Admission::Refused(error) => Err(error),
            Admission::Api(url) => self.api.answer(request, &url).await,
            Admission::Asset { path, head } => self.assets.answer(&request, &path, head).await,
        };
        answered
            .unwrap_or_else(|error| failure::refusal(error, &self.shared.errors).into_response())
    }

    fn update_tray(&self) {
        if let Some(desktop) = &self.shared.desktop {
            desktop.update(&self.cores, &self.store.facts());
        }
    }

    async fn close_resources(&self) -> Result<(), CloseError> {
        let mut failures = Vec::new();
        advance(&self.phase, Phase::Draining);
        if let Err(error) = self.agents.close().await {
            failures.push(error.to_string());
        }
        if let Err(error) = self.cores.shutdown().await {
            failures.push(error.message);
        }
        self.prices.close().await;
        advance(&self.phase, Phase::Closed);
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
    /// A port in use, or a store that does not open.
    pub async fn start(
        options: Options,
        boundaries: Boundaries,
        lock: ServerLock,
    ) -> Result<Self, StartError> {
        let directory = lock.directory().clone();
        let vault = tokio::task::spawn_blocking(move || Vault::open(&directory)).await??;
        let store = Store::open(lock.directory(), vault).await?;
        match Self::assemble(options, boundaries, lock, store.clone()).await {
            Ok(daemon) => Ok(daemon),
            Err(error) => {
                store.close().await;
                Err(error)
            }
        }
    }

    async fn assemble(
        options: Options,
        boundaries: Boundaries,
        lock: ServerLock,
        store: Store,
    ) -> Result<Self, StartError> {
        let Boundaries {
            backends,
            outside,
            clock,
            timing,
        } = boundaries;
        let now = UNIX_EPOCH + Duration::from_millis(u64::try_from(clock()).unwrap_or(0));
        store.call(move |db| db.prune_usage(now)).await?;
        let listener = listener::bind(options.port)?;
        let port = listener.local_addr().map_err(StartError::Listen)?.port();
        let origin = Origin::loopback(port);
        let phase = watch::Sender::new(Phase::Serving);
        let checks = Checks::new(
            port,
            origin.clone(),
            options.allow_hosts,
            options.development,
            SessionToken::generate().map_err(StartError::Entropy)?,
            phase.subscribe(),
        );
        let shared = Arc::new(Shared {
            port,
            origin,
            clock,
            errors: Errors::default(),
            desktop: options.desktop,
        });
        let (reported, accepted) = (Arc::clone(&shared), Arc::clone(&shared));
        let cores = Arc::new(
            Cores::new(CoresOptions {
                store: store.clone(),
                binary: options.core.binary,
                backends,
                ready: READY,
                timing,
                outside: Arc::clone(&outside),
                source: options.core.source,
                app: AppVersion::current(),
                clock,
                report: Arc::new(move |message| reported.errors.report(message)),
                sign_in_accepted: Arc::new(move || {
                    if let Some(desktop) = &accepted.desktop {
                        desktop.sign_in_accepted();
                    }
                }),
            })
            .map_err(StartError::Cores)?,
        );
        let catalog = Arc::new(ModelCatalog::new(Arc::clone(&outside), clock));
        let prices = Arc::new(UsagePrices::new(outside, clock));
        let imports = ImportCatalog::new(&options.agents.home);
        let agents = Arc::new(AgentSetup::new(
            lock.directory(),
            connection_source(phase.subscribe(), &store, &cores, port),
            options.agents,
        ));
        let app = Arc::new(App {
            api: Api::new(
                Arc::clone(&shared),
                store.clone(),
                Arc::clone(&cores),
                Arc::clone(&agents),
                Arc::clone(&catalog),
                Arc::clone(&prices),
                imports,
            ),
            assets: Assets::new(options.client).map_err(StartError::Entropy)?,
            gateway: Gateway::new(GatewayHost::new(
                Arc::clone(&shared),
                store.clone(),
                Arc::clone(&cores),
                catalog,
            )),
            shared,
            checks,
            store,
            cores,
            agents,
            prices,
            phase,
            directory: lock.directory().clone(),
            lock: Mutex::new(Some(lock)),
            session: OnceLock::new(),
            closed: OnceCell::new(),
        });
        listener::serve(&app, listener, &app.phase);
        if let Some(desktop) = &app.shared.desktop {
            desktop.follow(&app.cores, app.store.facts());
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
        app.cores.launch().await.map_err(StartError::Cores)?;
        let profiles = app.store.call(|db| db.profiles()).await?;
        match app.agents.restore(&profiles, app.shared.port).await {
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
            port: app.shared.port,
            token: app.checks.token().clone(),
            allow_hosts: app.checks.allow_hosts().to_vec(),
            version: VERSION.to_owned(),
            started_at: (app.shared.clock)(),
        };
        session.write(&app.directory).map_err(StartError::Session)?;
        app.session.get_or_init(|| session);
        Ok(())
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.0.shared.port
    }

    /// What `server.json` says once [`Daemon::restore`] wrote it.
    #[must_use]
    pub fn session(&self) -> Option<Session> {
        self.0.session.get().cloned()
    }

    /// `http://127.0.0.1:<port>`: the only origin the desktop's windows show.
    #[must_use]
    pub fn origin(&self) -> &Origin {
        &self.0.shared.origin
    }

    /// The session token the desktop's window helper hands the UI.
    #[must_use]
    pub fn session_token(&self) -> &SessionToken {
        self.0.checks.token()
    }

    #[must_use]
    pub fn data_directory(&self) -> &DataDir {
        &self.0.directory
    }

    #[must_use]
    pub fn has_profile(&self, profile: &ProfileId) -> bool {
        self.0
            .store
            .facts()
            .borrow()
            .iter()
            .any(|fact| fact.id == *profile)
    }

    /// Shows an error in the UI's error list.
    pub fn report(&self, message: &str) {
        self.0.shared.errors.report(message.to_owned());
    }

    pub fn stop(&self) {
        advance(&self.0.phase, Phase::StopAsked);
    }

    /// Resolves once Quit was chosen or [`Daemon::stop`] was called, and not for a close that
    /// began without either.
    pub async fn stopping(&self) {
        let mut phase = self.0.phase.subscribe();
        // The sender lives as long as the daemon this borrows.
        let _ = phase.wait_for(|phase| *phase == Phase::StopAsked).await;
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

/// What the credential socket answers: the profile's key and the port, while its core runs or
/// sleeps until the agent's first request.
fn connection_source(
    phase: watch::Receiver<Phase>,
    store: &Store,
    cores: &Arc<Cores>,
    port: u16,
) -> ConnectionSource {
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
        .ok_or_else(AppError::profile_not_found)?;
        if *phase.borrow() >= Phase::Draining || !cores.status(profile).serving {
            return Err(AppError::new(
                "Start this profile before connecting an agent.",
                409,
            ));
        }
        Ok(Connection { key, port })
    })
}
