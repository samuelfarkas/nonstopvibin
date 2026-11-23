//! One `nonstopvibin-window` process: spawned with piped stdin and stdout, the handshake
//! written first, and its messages read on a task of their own so the shell's loop never
//! loses half a line. Dropping a [`Helper`] closes its stdin, and the helper then exits.

use std::io::{self, Write as _};
use std::path::Path;
use std::process::{self, ExitStatus, Stdio};

use nsv_desktop::pipe::{self, Failure, FromHelper, Reason, Start, ToHelper};
use nsv_domain::secret::SessionToken;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::install::Install;
use crate::log;

/// The helper at `program`, reading the pipe on its stdin and logging to the daemon's stderr.
fn command(program: &Path, stdout: Stdio) -> process::Command {
    let mut command = process::Command::new(program);
    command
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(Stdio::inherit());
    // WebKitGTK's DMA-BUF renderer shows a blank window on NVIDIA's driver.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none()
        && Path::new("/proc/driver/nvidia/version").exists()
    {
        command.env("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    command
}

/// Shows why the desktop app failed in the helper's error dialog, as main.ts's error box did,
/// and returns once it is dismissed: call it off the async runtime. Without a helper that can show it, Linux posts a desktop
/// notification instead.
pub fn alert(install: &Install, failure: Failure, reason: &str) {
    let reason = Reason::new(reason);
    let shown = install.helper().and_then(|program| {
        let mut child = command(&program, Stdio::null()).spawn()?;
        let message = ToHelper::Alert {
            failure,
            reason: reason.clone(),
        };
        // Dropping stdin ends the pipe; the helper needs nothing after the one line.
        let written = match child.stdin.take() {
            Some(mut stdin) => pipe::encode(&message).and_then(|line| stdin.write_all(&line)),
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
    ready: bool,
}

impl Helper {
    pub(crate) async fn spawn(
        program: &Path,
        start: Start,
        token: &SessionToken,
    ) -> io::Result<Self> {
        let mut child = Command::from(command(program, Stdio::piped())).spawn()?;
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(io::Error::other("the helper's pipes are missing"));
        };
        stdin.write_all(&pipe::handshake(start, token)?).await?;
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
        self.stdin.write_all(&pipe::encode(message)?).await
    }

    /// The next message, then `Exited` once. Cancel-safe.
    pub(crate) async fn next(&mut self) -> Event {
        let event = self.events.recv().await.unwrap_or(Event::Exited(None));
        if matches!(event, Event::Message(FromHelper::Ready)) {
            self.ready = true;
        }
        event
    }

    /// Whether its first view has been on screen.
    pub(crate) fn ready(&self) -> bool {
        self.ready
    }
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
    (&mut *reader)
        .take(pipe::line_limit())
        .read_until(b'\n', &mut line)
        .await?;
    pipe::frame(line)?
        .map(|line| pipe::decode(&line))
        .transpose()
}
