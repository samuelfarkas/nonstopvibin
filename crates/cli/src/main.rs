//! `nonstopvibin`: the desktop app with no command, and the headless server's commands: `serve`,
//! `url`, `status` and `service`. Messages and exit codes are the TypeScript CLI's.
#![forbid(unsafe_code)]

mod args;
mod desktop;
mod run;
mod service;

use std::fmt::Display;
use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use nsv_core::upstream::SIGN_IN_PORTS;
use nsv_domain::endpoint::LOOPBACK_HOST;
use nsv_files::DataDir;
use nsv_server::{Session, VERSION, valid_allow_host};

use crate::args::Args;

/// The headless server's default port.
pub(crate) const SERVER_PORT: u16 = 4320;
/// `status` and `url` exit with this when no server runs.
const NOT_RUNNING: u8 = 3;

/// A message for the user and the exit code it ends with.
#[derive(Debug)]
pub(crate) struct CliError {
    message: String,
    code: u8,
}

impl CliError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 1,
        }
    }

    pub(crate) fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 2,
        }
    }

    pub(crate) fn from_display(error: impl Display) -> Self {
        Self::new(error.to_string())
    }
}

/// One line on standard output. A closed pipe loses it: there is no one left to tell.
pub(crate) fn say(text: &str) {
    let _ = writeln!(std::io::stdout().lock(), "{text}");
}

/// One line on standard error.
pub(crate) fn complain(text: &str) {
    let _ = writeln!(std::io::stderr().lock(), "{text}");
}

/// The user name a tunnel command logs in as.
pub(crate) fn user() -> String {
    ["USER", "LOGNAME"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn help() -> Result<String, CliError> {
    let data = service::data_directory()?;
    Ok(format!(
        "NonstopVibin {VERSION} — headless server

Usage:
  nonstopvibin serve [--port {SERVER_PORT}] [--allow-host FQDN]
      Run in the foreground (Ctrl+C stops it).
  nonstopvibin url [--json]
      Print the session link and the SSH tunnel command.
  nonstopvibin status
      Show whether the server is running (exit 0) or not (exit {NOT_RUNNING}).
  nonstopvibin service install [--port {SERVER_PORT}] [--allow-host FQDN]
      Install and start a systemd user service.
  nonstopvibin service uninstall
      Stop and remove the user service. Your data is kept.
  nonstopvibin --version

Options:
  --port N           Loopback port (default {SERVER_PORT}). Changing it later
                     requires restarting running agents.
  --allow-host FQDN  Also accept this exact HTTPS host name for the web UI,
                     e.g. from `tailscale serve --bg {SERVER_PORT}`.

Manage the service with systemd:
  systemctl --user status nonstopvibin
  systemctl --user restart nonstopvibin    (issues a new session link)
  journalctl --user -u nonstopvibin -f

Data: {} (override with {}).
Profiles, subscriptions and agent connections are managed in the web UI.",
        data.path().display(),
        DataDir::VARIABLE
    ))
}

fn parse_host(value: &str) -> Result<String, CliError> {
    let host = value.trim_matches(char::is_whitespace).to_lowercase();
    if valid_allow_host(&host) {
        Ok(host)
    } else {
        Err(CliError::usage(format!(
            "--allow-host needs an exact DNS name such as myserver.tailnet.ts.net (no scheme, port, wildcard or IP address); got {}.",
            serde_json::Value::String(value.to_owned())
        )))
    }
}

fn parse_port(value: Option<&str>) -> Result<u16, CliError> {
    let Some(value) = value else {
        return Ok(SERVER_PORT);
    };
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0 && value.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| CliError::usage("--port must be between 1 and 65535."))
}

/// OpenSSH sets `SSH_CONNECTION` to "client-ip client-port server-ip server-port".
fn tunnel_command(ssh_connection: &str, user: &str, port: u16) -> Option<String> {
    let fields: Vec<&str> = ssh_connection.split_whitespace().collect();
    let (address, ssh_port) = (*fields.get(2)?, *fields.get(3)?);
    let address = address
        .strip_prefix("::ffff:")
        .filter(|rest| {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            digits > 0 && rest.as_bytes().get(digits) == Some(&b'.')
        })
        .unwrap_or(address);
    let mut parts = vec![
        "ssh -N -o ExitOnForwardFailure=yes".to_owned(),
        format!("-L 127.0.0.1:{port}:127.0.0.1:{port}"),
    ];
    if ssh_port != "22" {
        parts.push(format!("-p {ssh_port}"));
    }
    parts.push(format!("{user}@{address}"));
    Some(parts.join(" "))
}

/// What `url` and a `serve` on a terminal print.
fn url_text(session: &Session, ssh_connection: Option<&str>) -> String {
    let url = session.url();
    let tunnel = ssh_connection
        .filter(|value| !value.is_empty())
        .and_then(|connection| tunnel_command(connection, &user(), session.port));
    let mut lines = vec![
        format!(
            "NonstopVibin is running on {LOOPBACK_HOST}:{} (pid {}).",
            session.port, session.pid
        ),
        String::new(),
    ];
    if let Some(tunnel) = tunnel {
        let forwards: Vec<String> = SIGN_IN_PORTS
            .iter()
            .map(|port| format!("-L 127.0.0.1:{port}:127.0.0.1:{port}"))
            .collect();
        lines.extend([
            "1. On your laptop, start a tunnel (use your usual SSH host name if you have one):"
                .to_owned(),
            format!("   {tunnel}"),
            String::new(),
            "2. Then open:".to_owned(),
            format!("   {url}"),
            String::new(),
            "3. Optional, for automatic Codex and Claude sign-in, add to the tunnel:".to_owned(),
            format!("   {}", forwards.join(" ")),
            "   Otherwise, paste the callback URL into the app when asked.".to_owned(),
        ]);
    } else {
        lines.extend([
            "Open this link on this machine:".to_owned(),
            format!("   {url}"),
        ]);
    }
    let https = session.https_urls();
    if !https.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "Tailscale (HTTPS only), after `tailscale serve --bg {}`:",
            session.port
        ));
        lines.extend(https.iter().map(|link| format!("   {link}")));
    }
    lines.extend([
        String::new(),
        "Keep this link private: it grants full control of NonstopVibin until the server restarts."
            .to_owned(),
    ]);
    lines.join("\n")
}

/// The runtime every command runs on: one thread, and at most four blocking threads that exit
/// after two idle seconds.
fn runtime() -> Result<tokio::runtime::Runtime, CliError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .max_blocking_threads(4)
        .thread_keep_alive(Duration::from_secs(2))
        .thread_name(nsv_server::panic::BLOCKING_THREAD)
        .build()
        .map_err(CliError::from_display)
}

/// Whether this invocation is the desktop app: bare in a graphical session, or with the
/// `--background` that Electron's autostart entries and login items pass, wherever they run.
fn opens_desktop(args: &Args, graphical: bool) -> bool {
    args.positionals.is_empty() && !args.help && (args.background || graphical)
}

/// A graphical session the desktop app can show its tray in.
fn desktop_session() -> bool {
    cfg!(target_os = "macos")
        || ["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

async fn command(args: &Args) -> Result<u8, CliError> {
    let allow_hosts = args
        .allow_hosts
        .as_ref()
        .map(|hosts| {
            hosts
                .iter()
                .map(|host| parse_host(host))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    let port = parse_port(args.port.as_deref())?;
    let positionals: Vec<&str> = args.positionals.iter().map(String::as_str).collect();
    let directory = match positionals.first() {
        Some(&("url" | "status")) => service::discovery_directory()?,
        _ => service::data_directory()?,
    };
    match positionals.as_slice() {
        ["serve"] => run::serve(directory, port, allow_hosts.unwrap_or_default()).await,
        ["url"] => run::url(&directory, args.json).await,
        ["status"] => run::status(&directory).await,
        ["service", "unit", executable] => {
            if let Some(path) = service::unit(executable)? {
                say(&path.display().to_string());
            }
            Ok(0)
        }
        ["service", "install"] => {
            let given_port = args.port.is_some().then_some(port);
            service::install(
                run::Layout::detect().packaged,
                given_port,
                allow_hosts.as_deref(),
            )
            .await?;
            Ok(0)
        }
        ["service", "uninstall"] => {
            service::uninstall()?;
            Ok(0)
        }
        _ => Err(CliError::usage(format!(
            "Unknown command: {}. Run `nonstopvibin --help` for usage.",
            positionals.join(" ")
        ))),
    }
}

fn main() -> ExitCode {
    nsv_server::panic::install_hook();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .init();
    let arguments: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    let args = match Args::parse(&arguments) {
        Ok(args) => args,
        Err(message) => {
            complain(&format!("{message}\nRun `nonstopvibin --help` for usage."));
            return ExitCode::from(2);
        }
    };
    if args.version {
        let core = nsv_core::binary::bundled_pin()
            .map_or_else(|_| String::new(), |pin| pin.version.to_string());
        say(&format!(
            "nonstopvibin {VERSION} (bundled CLIProxyAPI {core})"
        ));
        return ExitCode::SUCCESS;
    }
    if opens_desktop(&args, desktop_session()) {
        return run::desktop(args.background);
    }
    let result = if args.help || args.positionals.is_empty() {
        help().map(|text| {
            say(&text);
            0
        })
    } else {
        runtime().and_then(|runtime| runtime.block_on(command(&args)))
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            complain(&error.message);
            ExitCode::from(error.code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsv_domain::secret::SessionToken;

    #[test]
    fn the_legacy_background_flag_starts_the_desktop_app_without_a_display() {
        let opens = |line: &str, graphical| {
            let arguments: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
            opens_desktop(&Args::parse(&arguments).unwrap(), graphical)
        };
        assert!(opens("--background", false));
        assert!(opens("--background", true));
        assert!(opens("", true));
        assert!(
            !opens("", false),
            "a bare command without a display prints help"
        );
        assert!(!opens("--background --help", true));
        assert!(!opens("serve", true));
    }

    #[test]
    fn urls_and_tunnels_follow_the_session() {
        let session = Session {
            pid: 1,
            port: 4320,
            token: SessionToken::new("t"),
            allow_hosts: vec![],
            version: VERSION.to_owned(),
            started_at: 0,
        };
        assert!(url_text(&session, Some("")).contains("Open this link on this machine:"));
        assert_eq!(tunnel_command("1 2 3", "u", 4320), None);
    }
}

/// `tests/golden/cli.json`'s function-level cases: unit text, tunnels, links, host names and
/// data directories.
#[cfg(test)]
mod golden {
    use std::path::Path;

    use nsv_domain::secret::SessionToken;
    use nsv_domain::time::parse_iso;
    use nsv_files::DataDir;
    use serde_json::Value;

    use super::{tunnel_command, url_text, user};
    use crate::service::{render_unit, update_unit};
    use nsv_server::{Session, valid_allow_host};

    fn cases(prefix: &str) -> Vec<Value> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/cli.json");
        let all: Vec<Value> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let chosen: Vec<Value> = all
            .into_iter()
            .filter(|case| case["name"].as_str().unwrap().starts_with(prefix))
            .collect();
        assert!(!chosen.is_empty(), "no {prefix} cases");
        chosen
    }

    fn strings(value: &Value) -> Vec<String> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_owned())
            .collect()
    }

    fn environment(value: &Value) -> Vec<(String, String)> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| {
                (
                    pair[0].as_str().unwrap().to_owned(),
                    pair[1].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    /// The outcome as the recorder wrote it: the output, or the error's message.
    fn outcome(result: Result<String, crate::CliError>) -> (Option<String>, Option<String>) {
        match result {
            Ok(text) => (Some(text), None),
            Err(error) => (None, Some(error.message)),
        }
    }

    fn expected(case: &Value) -> (Option<String>, Option<String>) {
        (
            case["output"].as_str().map(str::to_owned),
            case["error"]["message"].as_str().map(str::to_owned),
        )
    }

    #[test]
    fn units_render_and_update_as_the_typescript_cli_wrote_them() {
        for case in cases("renderUnit ") {
            let input = &case["input"];
            let rendered = render_unit(
                input["executable"].as_str().unwrap(),
                &strings(&input["args"]),
                &environment(&input["environment"]),
            );
            assert_eq!(outcome(rendered), expected(&case), "{}", case["name"]);
        }
        for case in cases("updateUnit ") {
            let input = &case["input"];
            let hosts = input["allowHosts"]
                .as_array()
                .map(|_| strings(&input["allowHosts"]));
            let port = input["port"]
                .as_u64()
                .map(|port| u16::try_from(port).unwrap());
            let updated = update_unit(
                input["previous"].as_str().unwrap(),
                port,
                hosts.as_deref(),
                &environment(&input["environment"]),
            );
            assert_eq!(outcome(updated), expected(&case), "{}", case["name"]);
        }
    }

    #[test]
    fn tunnels_and_links_read_as_before() {
        for case in cases("tunnelCommand ") {
            let input = &case["input"];
            let port = u16::try_from(input["port"].as_u64().unwrap()).unwrap();
            let command = tunnel_command(
                input["sshConnection"].as_str().unwrap(),
                input["user"].as_str().unwrap(),
                port,
            );
            assert_eq!(
                command.as_deref(),
                case["output"].as_str(),
                "{}",
                case["name"]
            );
        }
        for case in cases("urlText ") {
            let stored = &case["input"]["session"];
            let session = Session {
                pid: u32::try_from(stored["pid"].as_u64().unwrap()).unwrap(),
                port: u16::try_from(stored["port"].as_u64().unwrap()).unwrap(),
                token: SessionToken::new(stored["token"].as_str().unwrap()),
                allow_hosts: strings(&stored["allowHosts"]),
                version: stored["version"].as_str().unwrap().to_owned(),
                started_at: parse_iso(stored["startedAt"].as_str().unwrap()).unwrap(),
            };
            let text = url_text(&session, case["input"]["sshConnection"].as_str())
                .replace(&format!("{}@", user()), "<user>@");
            assert_eq!(text, case["output"].as_str().unwrap(), "{}", case["name"]);
        }
    }

    #[test]
    fn allowed_hosts_and_data_directories_resolve_as_before() {
        for case in cases("validAllowHost ") {
            let host = case["input"].as_str().unwrap();
            assert_eq!(
                Value::Bool(valid_allow_host(host)),
                case["output"],
                "{}",
                case["name"]
            );
        }
        let home = std::env::home_dir().unwrap();
        let cwd = std::env::current_dir().unwrap();
        for case in cases("dataDirectory ") {
            let chosen = case["input"]["override"].as_str().unwrap_or_default();
            let resolved = DataDir::chosen(std::ffi::OsStr::new(chosen))
                .unwrap()
                .unwrap_or_else(|| DataDir::home().unwrap());
            let text = resolved
                .path()
                .to_string_lossy()
                .replacen(&*cwd.to_string_lossy(), "<repository>", 1)
                .replacen(&*home.to_string_lossy(), "<home>", 1);
            assert_eq!(text, case["output"].as_str().unwrap(), "{}", case["name"]);
        }
    }
}
