//! One `nonstopvibin-window` process: spawned with piped stdin and stdout, `Start` and the
//! token written first, and its messages read on a task of their own so the shell's loop never
//! loses half a line. Dropping a [`Helper`] closes its stdin, and the helper then exits.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use nsv_domain::helper::{Failure, FromHelper, MAX_LINE, Reason, ToHelper};
use nsv_domain::secret::SessionToken;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::log;

const NAME: &str = "nonstopvibin-window";

/// The helper beside the daemon's executable: the same folder in a checkout or an `AppImage`,
/// `Contents/MacOS` in the app bundle.
///
/// # Errors
/// When the daemon's own path is unknown.
pub fn beside_executable() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let directory = executable
        .parent()
        .ok_or_else(|| io::Error::other("the executable has no folder"))?;
    Ok(directory.join(NAME))
}

/// Shows why the desktop app failed in the helper's error dialog, as main.ts's error box did,
/// and returns once it is dismissed: call it off the async runtime. Without a helper that can show it, Linux posts a desktop
/// notification instead.
pub fn alert(failure: Failure, reason: &str) {
    let reason = Reason::new(reason);
    let shown = beside_executable().and_then(|program| {
        let mut child = std::process::Command::new(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let message = ToHelper::Alert {
            failure,
            reason: reason.clone(),
        };
        // Dropping stdin ends the pipe; the helper needs nothing after the one line.
        let written = match child.stdin.take() {
            Some(mut stdin) => line(&message).and_then(|line| stdin.write_all(&line)),
            None => Err(io::Error::other("the helper's stdin is missing")),
        };
        let status = child.wait()?;
        written?;
        Ok(status)
    });
    let failed = match shown {
        Ok(status) if status.success() => return,
        Ok(status) => format!("it exited with {status}"),
        Err(error) => error.to_string(),
    };
    log(format_args!(
        "the window helper could not show the error: {failed}"
    ));
    #[cfg(target_os = "linux")]
    if let Err(error) = notify(failure, &reason) {
        log(format_args!("no desktop notification either: {error}"));
    }
}

/// `org.freedesktop.Notifications`, which needs no GUI library in this process. The body may
/// be read as markup, so the reason is escaped.
#[cfg(target_os = "linux")]
fn notify(failure: Failure, reason: &Reason) -> zbus::Result<()> {
    let body = reason
        .as_str()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let bus = zbus::Connection::session().await?;
        bus.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "NonstopVibin",
                0_u32,
                "dialog-error",
                failure.title(),
                body.as_str(),
                Vec::<&str>::new(),
                std::collections::HashMap::<&str, zbus::zvariant::Value<'_>>::new(),
                -1_i32,
            ),
        )
        .await?;
        Ok(())
    })
}

#[derive(Debug)]
pub(crate) enum Event {
    Message(FromHelper),
    /// The helper has exited; `None` when waiting for it failed.
    Exited(Option<ExitStatus>),
}

pub(crate) struct Helper {
    stdin: ChildStdin,
    events: mpsc::Receiver<Event>,
    /// `Ready` arrived: the first view is on screen.
    pub(crate) ready: bool,
}

impl Helper {
    /// Starts a helper with `start`, a `ToHelper::Start`.
    pub(crate) async fn spawn(
        program: &Path,
        start: &ToHelper,
        token: &SessionToken,
    ) -> io::Result<Self> {
        let mut command = Command::new(program);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // WebKitGTK's DMA-BUF renderer shows a blank window on NVIDIA's driver.
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none()
            && Path::new("/proc/driver/nvidia/version").exists()
        {
            command.env("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
        let mut child = command.spawn()?;
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(io::Error::other("the helper's pipes are missing"));
        };
        let mut first = line(start)?;
        first.extend_from_slice(token.expose_secret().as_bytes());
        first.push(b'\n');
        stdin.write_all(&first).await?;
        let (sender, events) = mpsc::channel(16);
        tokio::spawn(read(child, stdout, sender));
        Ok(Self {
            stdin,
            events,
            ready: false,
        })
    }

    /// # Errors
    /// When the helper no longer reads its stdin: it has exited or is exiting.
    pub(crate) async fn send(&mut self, message: &ToHelper) -> io::Result<()> {
        self.stdin.write_all(&line(message)?).await
    }

    /// The next message, then `Exited` once. Cancel-safe.
    pub(crate) async fn next(&mut self) -> Event {
        self.events.recv().await.unwrap_or(Event::Exited(None))
    }
}

fn line(message: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    Ok(line)
}

/// Forwards the helper's messages until its stdout ends, then its exit. A line that is not a
/// message ends the helper: it is not the program this daemon speaks to.
async fn read(mut child: Child, stdout: ChildStdout, sender: mpsc::Sender<Event>) {
    let mut lines = BufReader::new(stdout);
    loop {
        match read_message(&mut lines).await {
            Ok(Some(message)) => {
                if sender.send(Event::Message(message)).await.is_err() {
                    // The shell let this helper go; it exits once it sees its stdin close.
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => {
                log(format_args!(
                    "the window helper broke the protocol: {error}"
                ));
                if let Err(error) = child.start_kill() {
                    log(format_args!("could not stop the window helper: {error}"));
                }
                break;
            }
        }
    }
    let status = child
        .wait()
        .await
        .inspect_err(|error| log(format_args!("lost the window helper: {error}")))
        .ok();
    // A shell that let this helper go no longer listens, and needs no answer.
    drop(sender.send(Event::Exited(status)).await);
}

async fn read_message(
    reader: &mut BufReader<impl AsyncRead + Unpin>,
) -> io::Result<Option<FromHelper>> {
    let mut line = Vec::new();
    let limit = u64::try_from(MAX_LINE + 1).unwrap_or(u64::MAX);
    (&mut *reader)
        .take(limit)
        .read_until(b'\n', &mut line)
        .await?;
    if line.is_empty() {
        return Ok(None);
    }
    if line.pop() != Some(b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "helper message too long or unterminated",
        ));
    }
    Ok(Some(serde_json::from_slice(&line)?))
}
