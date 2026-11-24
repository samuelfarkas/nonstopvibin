//! The daemon's two lives, `serve` and the desktop app, and the commands that ask a running
//! one: `url` and `status`.

use std::net::Ipv4Addr;
use std::path::Path;
use std::process::ExitCode;

use nsv_agents::AgentDirectories;
use nsv_core::binary::{BINARY, CHECKOUT_DIRECTORY, PACKAGED_DIRECTORY};
use nsv_core::install::CoreSource;
use nsv_files::{DataDir, LockError, ServerLock};
use nsv_server::{
    AllowedHost, Boundaries, CoreOptions, Daemon, Options, Session, StartError, panic,
};
use nsv_shell::{Failure, Install};
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::desktop::Desktop;
use crate::service::{UNIT, systemctl};
use crate::{CliError, NOT_RUNNING, complain, say, url_text};

/// Where the desktop app listens unless `NONSTOPVIBIN_PORT` says otherwise.
const DESKTOP_PORT: u16 = 4318;
/// A package's built interface, relative to its resources.
const PACKAGED_CLIENT: &str = "client";

/// Where this build is installed; a package keeps its interface and its core in its resources.
pub(crate) fn install() -> Install {
    Install::detect(
        Path::new(PACKAGED_CLIENT),
        &Path::new(PACKAGED_DIRECTORY).join(BINARY),
    )
}

/// The daemon's two lives.
enum Mode {
    /// `serve`, with the command line's port and allowed hosts.
    Serve {
        port: u16,
        allow_hosts: Vec<AllowedHost>,
    },
    /// The desktop app, with the part its shell follows.
    Desktop(nsv_server::Desktop),
}

/// The daemon's options in `mode` on `install`; the match is where the two lives differ.
fn options(mode: Mode, install: &Install) -> Result<Options, CliError> {
    // `bun run dev`: a source checkout's `serve` also accepts the Vite development server's
    // Host and Origin and starts without its core; packaged builds ignore the variable.
    let development = matches!(mode, Mode::Serve { .. })
        && !install.packaged
        && std::env::var_os("NONSTOPVIBIN_DEVELOPMENT").is_some_and(|value| value == "1");
    let source = match (&mode, install.packaged) {
        (Mode::Serve { .. }, true) => CoreSource::Package {
            feed: core_feed(),
            pinned: true,
        },
        // Desktop signing rewrites the core and refreshes its manifest.
        (Mode::Desktop(_), true) => CoreSource::Package {
            feed: core_feed(),
            pinned: false,
        },
        (Mode::Serve { .. }, false) if !development => CoreSource::Checkout,
        // A development run without its core still opens.
        (_, false) => CoreSource::Development,
    };
    let (port, allow_hosts, desktop) = match mode {
        Mode::Serve { port, allow_hosts } => (port, allow_hosts, None),
        Mode::Desktop(desktop) => (desktop_port()?, Vec::new(), Some(desktop)),
    };
    let (core, client) = if install.packaged {
        (
            install.resources.join(PACKAGED_DIRECTORY),
            install.resources.join(PACKAGED_CLIENT),
        )
    } else {
        (
            install.resources.join(CHECKOUT_DIRECTORY),
            install.resources.join("dist/client"),
        )
    };
    Ok(Options {
        port,
        allow_hosts,
        development,
        desktop,
        client,
        core: CoreOptions {
            binary: core.join(BINARY),
            source,
        },
        agents: AgentDirectories::from_environment().map_err(CliError::from_display)?,
    })
}

fn desktop_port() -> Result<u16, CliError> {
    match std::env::var("NONSTOPVIBIN_PORT") {
        Ok(value) if !value.is_empty() => value.parse().map_err(|_| {
            CliError::new(format!(
                "NONSTOPVIBIN_PORT must be a port number; got {value:?}."
            ))
        }),
        _ => Ok(DESKTOP_PORT),
    }
}

/// The reviewed core pin installed builds offer; `NONSTOPVIBIN_CORE_FEED` tries another, such
/// as a pull request's.
fn core_feed() -> String {
    std::env::var("NONSTOPVIBIN_CORE_FEED")
        .ok()
        .filter(|feed| !feed.is_empty())
        .unwrap_or_else(|| nsv_core::install::CORE_FEED.to_owned())
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

/// Why the daemon did not start. The desktop app takes its port from `NONSTOPVIBIN_PORT`, not
/// from `--port`.
fn start_failure(error: &StartError, desktop: bool) -> CliError {
    match error {
        StartError::PortInUse(port) if desktop => CliError::new(format!(
            "Port {port} on 127.0.0.1 is already in use. Stop the other program, or set NONSTOPVIBIN_PORT to another port before starting NonstopVibin."
        )),
        _ => CliError::from_display(error),
    }
}

/// Starts the daemon, cancelling on a signal. `None` when a signal came first: everything
/// started is stopped again.
async fn start(
    signals: &mut Signals,
    options: Options,
    lock: ServerLock,
) -> Result<Option<Daemon>, CliError> {
    let desktop = options.desktop.is_some();
    let boundaries = Boundaries::production().map_err(CliError::from_display)?;
    let daemon = tokio::select! {
        started = Daemon::start(options, boundaries, lock) => {
            started.map_err(|error| start_failure(&error, desktop))?
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
/// after such a panic or a failed close; the desktop app (`desktop`, its installation) first
/// shows a failed close in main.ts's error box until it is dismissed.
async fn run_until_stopped(
    daemon: &Daemon,
    signals: &mut Signals,
    shell: impl Future<Output = Result<(), nsv_shell::UpdateFailed>>,
    desktop: Option<&Install>,
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
        if let Some(install) = desktop.cloned() {
            let shown = tokio::task::spawn_blocking(move || {
                nsv_shell::alert(&install, Failure::Shutdown, &reason);
            });
            if let Err(error) = shown.await {
                complain(&format!("Could not show the error: {error}"));
            }
        }
    }
    u8::from(update_failed || closed.is_err() || panic::panicked())
}

/// `serve`: the headless server in the foreground.
pub(crate) async fn serve(
    directory: DataDir,
    port: u16,
    allow_hosts: Vec<AllowedHost>,
) -> Result<u8, CliError> {
    let mut signals = Signals::new()?;
    if let Some(existing) = Session::read(&directory).map_err(CliError::from_display)?
        && existing.probe().await.is_some()
    {
        return Err(CliError::new(format!(
            "NonstopVibin is already running on {}:{} for {}. Run `nonstopvibin url` for its link.",
            Ipv4Addr::LOCALHOST,
            existing.port,
            directory.path().display()
        )));
    }
    let lock = directory.lock().map_err(CliError::from_display)?;
    let options = options(Mode::Serve { port, allow_hosts }, &install())?;
    let development = options.development;
    if !development && !options.core.binary.exists() {
        return Err(CliError::new(format!(
            "The proxy core is missing at {}. Reinstall NonstopVibin from its release archive.",
            options.core.binary.display()
        )));
    }
    let Some(daemon) = start(&mut signals, options, lock).await? else {
        return Ok(0);
    };
    // journald must never receive the session token.
    match daemon.session() {
        Some(session) if development => say(&format!(
            "Open the local interface: {}",
            session.development_url()
        )),
        Some(session) if std::io::IsTerminal::is_terminal(&std::io::stdout()) => say(&url_text(
            &session,
            std::env::var("SSH_CONNECTION").ok().as_deref(),
        )),
        _ => say(&format!(
            "NonstopVibin {} is listening on {}:{}. Run `nonstopvibin url` for the session link.",
            nsv_server::VERSION,
            Ipv4Addr::LOCALHOST,
            daemon.port()
        )),
    }
    Ok(run_until_stopped(&daemon, &mut signals, std::future::pending(), None).await)
}

/// The desktop app: the daemon, its tray and, unless `background`, the main window. A second
/// launch finds the first through the data directory's lock and asks it to show its window.
pub(crate) fn desktop(background: bool) -> ExitCode {
    let install = install();
    let directory = match crate::service::data_directory() {
        Ok(directory) => directory,
        Err(error) => {
            could_not_start(&install, &error.message);
            return ExitCode::FAILURE;
        }
    };
    nsv_shell::run_desktop(move |link| {
        let result = nsv_server::runtime()
            .map_err(CliError::from_display)
            .and_then(|runtime| {
                runtime.block_on(desktop_daemon(&install, directory, background, link))
            });
        result.unwrap_or_else(|error| {
            could_not_start(&install, &error.message);
            1
        })
    })
}

/// main.ts's error box, which held the exit until it was dismissed. A desktop user who
/// started the app from a launcher or at login never sees standard error.
fn could_not_start(install: &Install, message: &str) {
    complain(&format!("NonstopVibin could not start: {message}"));
    nsv_shell::alert(install, Failure::Start, message);
}

async fn desktop_daemon(
    install: &Install,
    directory: DataDir,
    background: bool,
    link: nsv_shell::TrayLink,
) -> Result<u8, CliError> {
    let mut signals = Signals::new()?;
    let lock = match directory.lock() {
        Ok(lock) => lock,
        Err(LockError::Occupied(_)) => {
            Session::show_main(&directory)
                .await
                .map_err(CliError::from_display)?;
            return Ok(0);
        }
        Err(error) => return Err(CliError::from_display(error)),
    };
    let desktop = nsv_server::Desktop::default();
    let options = options(Mode::Desktop(desktop.clone()), install)?;
    let Some(daemon) = start(&mut signals, options, lock).await? else {
        return Ok(0);
    };
    let shell_options =
        nsv_shell::Options::new(install.clone(), !background).map_err(CliError::from_display)?;
    let host = Desktop {
        daemon: daemon.clone(),
        desktop,
    };
    let shell = nsv_shell::run(&host, shell_options, link);
    Ok(run_until_stopped(&daemon, &mut signals, shell, Some(install)).await)
}

/// A running daemon's `server.json` and state, if one answers.
async fn running(
    directory: &DataDir,
) -> Result<Option<(Session, nsv_server::ServerState)>, CliError> {
    let Some(session) = Session::read(directory).map_err(CliError::from_display)? else {
        return Ok(None);
    };
    Ok(session.probe().await.map(|state| (session, state)))
}

/// `url [--json]`.
pub(crate) async fn url(directory: &DataDir, json: bool) -> Result<u8, CliError> {
    let Some((session, state)) = running(directory).await? else {
        return Err(CliError {
            message: format!(
                "NonstopVibin is not running for {}.\nCheck `systemctl --user status nonstopvibin`, or start it with `nonstopvibin serve`.",
                directory.path().display()
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
pub(crate) async fn status(directory: &DataDir) -> Result<u8, CliError> {
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
            directory.path().display()
        ));
        return Ok(NOT_RUNNING);
    };
    say(&format!(
        "NonstopVibin {} is running on {}:{} (pid {}, since {}).\nCore: CLIProxyAPI {}.{unit}",
        state.version,
        Ipv4Addr::LOCALHOST,
        session.port,
        session.pid,
        session.started(),
        state.core_version
    ));
    Ok(0)
}
