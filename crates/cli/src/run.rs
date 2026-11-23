//! The daemon's two lives, `serve` and the desktop app, and the commands that ask a running
//! one: `url` and `status`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use nsv_agents::AgentDirectories;
use nsv_core::binary::{BINARY, CHECKOUT_DIRECTORY, PACKAGED_DIRECTORY};
use nsv_net::Https;
use nsv_server::{CoreOptions, Daemon, Options, Session, Unwired, panic};
use nsv_store::{LockError, ServerLock};
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::service::{UNIT, systemctl};
use crate::{CliError, NOT_RUNNING, complain, epoch_ms, say, url_text};

/// Where the desktop app listens unless `NONSTOPVIBIN_PORT` says otherwise.
const DESKTOP_PORT: u16 = 4318;

/// The reviewed core pin installed builds offer; `NONSTOPVIBIN_CORE_FEED` tries another, such
/// as a pull request's.
fn core_feed() -> String {
    std::env::var("NONSTOPVIBIN_CORE_FEED")
        .ok()
        .filter(|feed| !feed.is_empty())
        .unwrap_or_else(|| {
            "https://raw.githubusercontent.com/samuelfarkas/nonstopvibin/main/src/server/core-release.json"
                .to_owned()
        })
}

/// Where the core and the interface are: beside the real executable in a package (in
/// `Contents/Resources` of a macOS bundle), or in the source checkout this was built from.
pub(crate) struct Layout {
    pub(crate) packaged: bool,
    pub(crate) core: PathBuf,
    pub(crate) client: PathBuf,
}

impl Layout {
    pub(crate) fn detect() -> Self {
        // The packaged binary may be reached through a ~/.local/bin symlink.
        let resources = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .ok()
            .and_then(|executable| executable.parent().map(Path::to_path_buf))
            .map(|directory| {
                if cfg!(target_os = "macos") {
                    directory.join("../Resources")
                } else {
                    directory
                }
            });
        if let Some(resources) = resources
            && resources.join(PACKAGED_DIRECTORY).join(BINARY).is_file()
        {
            return Self {
                packaged: true,
                core: resources.join(PACKAGED_DIRECTORY).join(BINARY),
                client: resources.join("client"),
            };
        }
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let checkout = std::fs::canonicalize(&checkout).unwrap_or(checkout);
        Self {
            packaged: false,
            core: checkout.join(CHECKOUT_DIRECTORY).join(BINARY),
            client: checkout.join("dist/client"),
        }
    }
}

/// SIGINT, SIGTERM and SIGHUP, received from before any initialization.
struct Signals([Signal; 3]);

impl Signals {
    fn new() -> Result<Self, CliError> {
        let listen = |kind| signal(kind).map_err(CliError::from_display);
        Ok(Self([
            listen(SignalKind::interrupt())?,
            listen(SignalKind::terminate())?,
            listen(SignalKind::hangup())?,
        ]))
    }

    async fn received(&mut self) {
        let [interrupt, terminate, hangup] = &mut self.0;
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
            _ = hangup.recv() => {}
        }
    }
}

fn now() -> i64 {
    epoch_ms()
}

/// Starts the daemon, cancelling on a signal. `None` when a signal came first: everything
/// started is stopped again.
async fn start(
    signals: &mut Signals,
    options: Options<Https>,
    lock: ServerLock,
) -> Result<Option<Daemon<Https>>, CliError> {
    let daemon = tokio::select! {
        started = Daemon::start(options, lock, |_| Arc::new(Unwired)) => {
            started.map_err(CliError::from_display)?
        }
        () = signals.received() => return Ok(None),
    };
    let restored = tokio::select! {
        restored = daemon.restore() => Some(restored),
        () = signals.received() => None,
    };
    match restored {
        Some(Ok(())) => Ok(Some(daemon)),
        Some(Err(error)) => Err(match daemon.close().await {
            Ok(()) => CliError::from_display(error),
            Err(cleanup) => CliError::new(format!("{error} Cleanup failed: {cleanup}")),
        }),
        None => {
            daemon.close().await.map_err(CliError::from_display)?;
            Ok(None)
        }
    }
}

/// Waits for a signal, Quit, or a panic outside any request, then closes the daemon. Exits 1
/// after such a panic or a failed close.
async fn run_until_stopped(
    daemon: &Daemon<Https>,
    signals: &mut Signals,
    shell: impl Future<Output = ()>,
) -> u8 {
    tokio::select! {
        () = signals.received() => {}
        () = daemon.stopping() => {}
        () = panic::fatal() => {}
        () = shell => {}
    }
    let closed = daemon.close().await;
    if let Err(error) = &closed {
        complain(&error.to_string());
    }
    u8::from(closed.is_err() || panic::panicked())
}

fn agents() -> Result<AgentDirectories, CliError> {
    AgentDirectories::from_environment().map_err(CliError::from_display)
}

fn https() -> Result<Https, CliError> {
    Https::new().map_err(CliError::from_display)
}

/// `serve`: the headless server in the foreground.
pub(crate) async fn serve(
    directory: PathBuf,
    port: u16,
    allow_hosts: Vec<String>,
) -> Result<u8, CliError> {
    let mut signals = Signals::new()?;
    if let Some(existing) = Session::read(&directory).map_err(CliError::from_display)?
        && existing.probe().await.is_some()
    {
        return Err(CliError::new(format!(
            "NonstopVibin is already running on 127.0.0.1:{} for {}. Run `nonstopvibin url` for its link.",
            existing.port,
            directory.display()
        )));
    }
    let lock = ServerLock::acquire(&directory).map_err(CliError::from_display)?;
    let layout = Layout::detect();
    if !layout.core.exists() {
        return Err(CliError::new(format!(
            "The proxy core is missing at {}. Reinstall NonstopVibin from its release archive.",
            layout.core.display()
        )));
    }
    let options = Options {
        directory,
        port,
        allow_hosts,
        development: false,
        desktop: false,
        client: layout.client,
        core: CoreOptions {
            binary: layout.core,
            pinned: true,
            // A source checkout shares the data folder: an installed core would shadow its pin.
            feed: layout.packaged.then(core_feed),
            allow_missing: false,
        },
        agents: agents()?,
        fetch: https()?,
        clock: now,
    };
    let Some(daemon) = start(&mut signals, options, lock).await? else {
        return Ok(0);
    };
    // journald must never receive the session token.
    match daemon.session() {
        Some(session) if std::io::IsTerminal::is_terminal(&std::io::stdout()) => say(&url_text(
            &session,
            std::env::var("SSH_CONNECTION").ok().as_deref(),
        )),
        _ => say(&format!(
            "NonstopVibin {} is listening on 127.0.0.1:{}. Run `nonstopvibin url` for the session link.",
            nsv_server::VERSION,
            daemon.port()
        )),
    }
    Ok(run_until_stopped(&daemon, &mut signals, std::future::pending()).await)
}

/// The desktop app: the daemon, its tray and, unless `background`, the main window. A second
/// launch finds the first through the data directory's lock and asks it to show its window.
pub(crate) fn desktop(background: bool) -> ExitCode {
    let directory = match crate::service::data_directory(
        std::env::var_os("NONSTOPVIBIN_DATA_DIR").as_deref(),
    ) {
        Ok(directory) => directory,
        Err(error) => {
            complain(&format!("NonstopVibin could not start: {}", error.message));
            return ExitCode::FAILURE;
        }
    };
    nsv_shell::run_desktop(move |link| {
        let result = crate::runtime()
            .and_then(|runtime| runtime.block_on(desktop_daemon(directory, background, link)));
        result.unwrap_or_else(|error| {
            complain(&format!("NonstopVibin could not start: {}", error.message));
            1
        })
    })
}

async fn desktop_daemon(
    directory: PathBuf,
    background: bool,
    link: nsv_shell::TrayLink,
) -> Result<u8, CliError> {
    let mut signals = Signals::new()?;
    let lock = match ServerLock::acquire(&directory) {
        Ok(lock) => lock,
        Err(LockError::Occupied(_)) => {
            let shown = tokio::task::spawn_blocking(move || nsv_shell::show_running(&directory))
                .await
                .map_err(CliError::from_display)?;
            shown.map_err(CliError::from_display)?;
            return Ok(0);
        }
        Err(error) => return Err(CliError::from_display(error)),
    };
    let port = match std::env::var("NONSTOPVIBIN_PORT") {
        Ok(value) if !value.is_empty() => value.parse().map_err(|_| {
            CliError::new(format!(
                "NONSTOPVIBIN_PORT must be a port number; got {value:?}."
            ))
        })?,
        _ => DESKTOP_PORT,
    };
    let layout = Layout::detect();
    let options = Options {
        directory,
        port,
        allow_hosts: Vec::new(),
        development: false,
        desktop: true,
        client: layout.client,
        core: CoreOptions {
            binary: layout.core,
            // Desktop signing rewrites the core and refreshes its manifest.
            pinned: false,
            feed: layout.packaged.then(core_feed),
            // A development run without its core still opens.
            allow_missing: !layout.packaged,
        },
        agents: agents()?,
        fetch: https()?,
        clock: now,
    };
    let Some(daemon) = start(&mut signals, options, lock).await? else {
        return Ok(0);
    };
    let helper = nsv_shell::helper_beside_executable().map_err(CliError::from_display)?;
    #[cfg(target_os = "linux")]
    let login = nsv_shell::Login::from_environment().map_err(CliError::from_display)?;
    #[cfg(not(target_os = "linux"))]
    let login = nsv_shell::Login::new();
    let shell_options = nsv_shell::Options {
        helper,
        open_window: !background,
        packaged: layout.packaged,
        login,
    };
    let shell = nsv_shell::run(&daemon, shell_options, link);
    Ok(run_until_stopped(&daemon, &mut signals, shell).await)
}

/// A running daemon's `server.json` and state, if one answers.
async fn running(directory: &Path) -> Result<Option<(Session, nsv_server::ServerState)>, CliError> {
    let Some(session) = Session::read(directory).map_err(CliError::from_display)? else {
        return Ok(None);
    };
    Ok(session.probe().await.map(|state| (session, state)))
}

/// `url [--json]`.
pub(crate) async fn url(directory: &Path, json: bool) -> Result<u8, CliError> {
    let Some((session, state)) = running(directory).await? else {
        return Err(CliError {
            message: format!(
                "NonstopVibin is not running for {}.\nCheck `systemctl --user status nonstopvibin`, or start it with `nonstopvibin serve`.",
                directory.display()
            ),
            code: NOT_RUNNING,
        });
    };
    if json {
        say(&serde_json::json!({
            "running": true,
            "pid": session.pid,
            "port": session.port,
            "version": state.version,
            "coreVersion": state.core_version,
            "url": session.url(),
            "httpsUrls": session.https_urls(),
        })
        .to_string());
    } else {
        say(&url_text(
            &session,
            std::env::var("SSH_CONNECTION").ok().as_deref(),
        ));
    }
    Ok(0)
}

/// `status`: exit 0 with the running daemon's details, or 3.
pub(crate) async fn status(directory: &Path) -> Result<u8, CliError> {
    let unit = if cfg!(target_os = "linux") {
        systemctl(&["is-active", UNIT])
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|state| !state.is_empty())
    } else {
        None
    };
    let unit = unit.map_or_else(String::new, |state| format!(" Unit {UNIT}: {state}."));
    let Some((session, state)) = running(directory).await? else {
        say(&format!(
            "NonstopVibin is not running for {}.{unit}",
            directory.display()
        ));
        return Ok(NOT_RUNNING);
    };
    say(&format!(
        "NonstopVibin {} is running on 127.0.0.1:{} (pid {}, since {}).\nCore: CLIProxyAPI {}.{unit}",
        state.version, session.port, session.pid, session.started_at, state.core_version
    ));
    Ok(0)
}
