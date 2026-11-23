//! The daemon's two lives, `serve` and the desktop app, and the commands that ask a running
//! one: `url` and `status`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::SystemTime;

use nsv_agents::AgentDirectories;
use nsv_core::binary::{BINARY, CHECKOUT_DIRECTORY, PACKAGED_DIRECTORY};
use nsv_domain::endpoint::{LOOPBACK_HOST, session_link};
use nsv_domain::helper::Failure;
use nsv_domain::time::{epoch_ms, iso_string};
use nsv_net::Https;
use nsv_server::{CoreOptions, DEV_HOST, Daemon, Options, Session, TIMING, panic, processes};
use nsv_store::{LockError, ServerLock};
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::service::{UNIT, systemctl};
use crate::{CliError, NOT_RUNNING, complain, say, url_text};

/// Where the desktop app listens unless `NONSTOPVIBIN_PORT` says otherwise.
const DESKTOP_PORT: u16 = 4318;

/// The reviewed core pin installed builds offer; `NONSTOPVIBIN_CORE_FEED` tries another, such
/// as a pull request's.
fn core_feed() -> String {
    std::env::var("NONSTOPVIBIN_CORE_FEED")
        .ok()
        .filter(|feed| !feed.is_empty())
        .unwrap_or_else(|| nsv_core::install::CORE_FEED.to_owned())
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
    epoch_ms(SystemTime::now())
}

/// Starts the daemon, cancelling on a signal. `None` when a signal came first: everything
/// started is stopped again.
async fn start(
    signals: &mut Signals,
    options: Options<Https>,
    lock: ServerLock,
) -> Result<Option<Daemon<Https>>, CliError> {
    let daemon = tokio::select! {
        started = Daemon::start(options, lock) => {
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
/// after such a panic or a failed close; the desktop app (`desktop`) first shows a failed
/// close in main.ts's error box until it is dismissed.
async fn run_until_stopped(
    daemon: &Daemon<Https>,
    signals: &mut Signals,
    shell: impl Future<Output = Result<(), nsv_shell::UpdateFailed>>,
    desktop: bool,
) -> u8 {
    let mut update_failed = false;
    tokio::select! {
        () = signals.received() => {}
        () = daemon.stopping() => {}
        () = panic::fatal() => {}
        ended = shell => update_failed = ended.is_err(),
    }
    let closed = daemon.close().await;
    if let Err(error) = &closed {
        let reason = error.to_string();
        complain(&reason);
        if desktop {
            let shown =
                tokio::task::spawn_blocking(move || nsv_shell::alert(Failure::Shutdown, &reason));
            if let Err(error) = shown.await {
                complain(&format!("Could not show the error: {error}"));
            }
        }
    }
    u8::from(update_failed || closed.is_err() || panic::panicked())
}

fn agents() -> Result<AgentDirectories, CliError> {
    AgentDirectories::from_environment().map_err(CliError::from_display)
}

fn https() -> Result<Https, CliError> {
    Https::new().map_err(CliError::from_display)
}

/// `serve`: the headless server in the foreground. With `NONSTOPVIBIN_DEVELOPMENT=1`, a build
/// run from its source checkout (`bun run dev`) also accepts the Vite development server's
/// Host and Origin and starts without its core; packaged builds ignore the variable.
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
            "NonstopVibin is already running on {LOOPBACK_HOST}:{} for {}. Run `nonstopvibin url` for its link.",
            existing.port,
            directory.display()
        )));
    }
    let lock = ServerLock::acquire(&directory).map_err(CliError::from_display)?;
    let layout = Layout::detect();
    let development = !layout.packaged
        && std::env::var_os("NONSTOPVIBIN_DEVELOPMENT").is_some_and(|value| value == "1");
    if !development && !layout.core.exists() {
        return Err(CliError::new(format!(
            "The proxy core is missing at {}. Reinstall NonstopVibin from its release archive.",
            layout.core.display()
        )));
    }
    let options = Options {
        directory,
        port,
        allow_hosts,
        development,
        desktop: false,
        client: layout.client,
        core: CoreOptions {
            binary: layout.core,
            // `core:install` writes the pinned core, which development may replace by hand.
            pinned: !development,
            // A source checkout shares the data folder: an installed core would shadow its pin.
            feed: layout.packaged.then(core_feed),
            allow_missing: development,
        },
        agents: agents()?,
        backends: processes(),
        fetch: https()?,
        clock: now,
        timing: TIMING,
    };
    let Some(daemon) = start(&mut signals, options, lock).await? else {
        return Ok(0);
    };
    // journald must never receive the session token.
    match daemon.session() {
        Some(session) if development => say(&format!(
            "Open the local interface: {}",
            session_link(&format!("http://{DEV_HOST}"), &session.token)
        )),
        Some(session) if std::io::IsTerminal::is_terminal(&std::io::stdout()) => say(&url_text(
            &session,
            std::env::var("SSH_CONNECTION").ok().as_deref(),
        )),
        _ => say(&format!(
            "NonstopVibin {} is listening on {LOOPBACK_HOST}:{}. Run `nonstopvibin url` for the session link.",
            nsv_server::VERSION,
            daemon.port()
        )),
    }
    Ok(run_until_stopped(&daemon, &mut signals, std::future::pending(), false).await)
}

/// The desktop app: the daemon, its tray and, unless `background`, the main window. A second
/// launch finds the first through the data directory's lock and asks it to show its window.
pub(crate) fn desktop(background: bool) -> ExitCode {
    let directory = match crate::service::data_directory(
        std::env::var_os("NONSTOPVIBIN_DATA_DIR").as_deref(),
    ) {
        Ok(directory) => directory,
        Err(error) => {
            could_not_start(&error.message);
            return ExitCode::FAILURE;
        }
    };
    nsv_shell::run_desktop(move |link| {
        let result = crate::runtime()
            .and_then(|runtime| runtime.block_on(desktop_daemon(directory, background, link)));
        result.unwrap_or_else(|error| {
            could_not_start(&error.message);
            1
        })
    })
}

/// main.ts's error box, which held the exit until it was dismissed. A desktop user who
/// started the app from a launcher or at login never sees standard error.
fn could_not_start(message: &str) {
    complain(&format!("NonstopVibin could not start: {message}"));
    nsv_shell::alert(Failure::Start, message);
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
        backends: processes(),
        fetch: https()?,
        clock: now,
        timing: TIMING,
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
    Ok(run_until_stopped(&daemon, &mut signals, shell, true).await)
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
        "NonstopVibin {} is running on {LOOPBACK_HOST}:{} (pid {}, since {}).\nCore: CLIProxyAPI {}.{unit}",
        state.version,
        session.port,
        session.pid,
        iso_string(session.started_at),
        state.core_version
    ));
    Ok(0)
}
