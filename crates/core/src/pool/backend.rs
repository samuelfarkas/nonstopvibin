//! What the pool does outside its own memory: launching cores and talking to them over
//! loopback. [`Processes`] is the daemon's; tests put in-process fake cores behind the same
//! traits, so the pool and scheduler run under tokio's paused clock.

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use nsv_domain::id::ProfileId;
use nsv_net::{Loopback, LoopbackRequest, Method, NetError, Response, loopback};

use super::BoxFuture;
use crate::processes::{CoreProcess, Ledger, Ps, forget, spawn_core};

/// Launches cores and carries the pool's loopback requests.
pub trait Backend: Send + Sync + 'static {
    /// Starts `binary` as `profile`'s core of the data directory `data`.
    ///
    /// # Errors
    /// The core could not be started.
    fn launch(
        &self,
        binary: &Path,
        data: &Path,
        profile: &ProfileId,
    ) -> io::Result<Box<dyn CoreHandle>>;

    /// One request to a core: management, readiness and the model list.
    fn send(&self, call: Call) -> BoxFuture<'static, Result<Response, NetError>>;

    /// Stops cores of a dead earlier instance; see [`Ledger::reap_orphans`].
    fn reap_orphans(&self) -> BoxFuture<'_, io::Result<()>>;
}

/// One request to a core on 127.0.0.1. It carries a key, so it has no `Debug`.
pub struct Call {
    pub port: u16,
    pub method: Method,
    pub target: String,
    /// The `Authorization` header: the management key, or the core key for the model list.
    pub authorization: String,
    /// A JSON body.
    pub body: Option<Vec<u8>>,
    pub timeout: Duration,
    /// The most of the answer read.
    pub limit: u64,
}

/// One launched core, owned by the pool task that serves it.
pub trait CoreHandle: Send + 'static {
    /// Records the core for the next instance to reap, unless it already is. A core that
    /// cannot be recorded yet (its exec has not settled, or there is no `ps`) is still guarded
    /// by its watchdog; the pool records again once it answers.
    fn record(&mut self) -> BoxFuture<'_, ()>;

    /// Waits for the core to exit. Cancel-safe.
    fn wait(&mut self) -> BoxFuture<'_, io::Result<ExitStatus>>;

    /// SIGTERM, then SIGKILL after five seconds; answers the exit status. Cancel-safe.
    fn terminate(&mut self) -> BoxFuture<'_, io::Result<ExitStatus>>;
}

/// Real cores: [`spawn_core`], the watchdog and the ledger of `core-processes/`.
#[derive(Debug)]
pub struct Processes {
    ledger: Arc<Ledger<Ps>>,
}

impl Processes {
    #[must_use]
    pub fn new(data: &Path) -> Self {
        Self {
            ledger: Arc::new(Ledger::new(data, Ps::default())),
        }
    }
}

impl Backend for Processes {
    fn launch(
        &self,
        binary: &Path,
        data: &Path,
        profile: &ProfileId,
    ) -> io::Result<Box<dyn CoreHandle>> {
        Ok(Box::new(Process {
            core: spawn_core(binary, data, profile)?,
            ledger: Arc::clone(&self.ledger),
            entry: None,
        }))
    }

    fn send(&self, call: Call) -> BoxFuture<'static, Result<Response, NetError>> {
        Box::pin(async move {
            let mut headers = vec![("Authorization", call.authorization.as_str())];
            if call.body.is_some() {
                headers.push(("Content-Type", "application/json"));
            }
            loopback(LoopbackRequest {
                to: Loopback::v4(call.port),
                method: &call.method,
                target: &call.target,
                headers: &headers,
                body: call.body.as_deref(),
                timeout: call.timeout,
                limit: call.limit,
            })
            .await
        })
    }

    fn reap_orphans(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(self.ledger.reap_orphans())
    }
}

struct Process {
    core: CoreProcess,
    ledger: Arc<Ledger<Ps>>,
    entry: Option<PathBuf>,
}

impl Process {
    /// Removes the exited core's ledger entry. An entry left by a failed removal names a
    /// process that is gone, which the next instance's reaping removes.
    fn forget(&mut self) {
        if let Some(entry) = self.entry.take() {
            let _ = forget(&entry);
        }
    }
}

impl CoreHandle for Process {
    fn record(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async {
            if self.entry.is_none() {
                // The watchdog still guards this core; only the restart backstop is missing.
                if let Ok(entry) = self.ledger.record(&mut self.core).await {
                    self.entry = entry;
                }
            }
        })
    }

    fn wait(&mut self) -> BoxFuture<'_, io::Result<ExitStatus>> {
        Box::pin(async {
            let status = self.core.wait().await;
            self.forget();
            status
        })
    }

    fn terminate(&mut self) -> BoxFuture<'_, io::Result<ExitStatus>> {
        Box::pin(async {
            let status = self.core.terminate().await;
            self.forget();
            status
        })
    }
}

/// How Node named an exit: the signal's name, else the exit code.
pub(super) fn exit_text(status: &io::Result<ExitStatus>) -> String {
    let status = match status {
        Ok(status) => status,
        Err(error) => return error.to_string(),
    };
    if let Some(signal) = status.signal() {
        let name = match signal {
            1 => "SIGHUP",
            2 => "SIGINT",
            3 => "SIGQUIT",
            4 => "SIGILL",
            5 => "SIGTRAP",
            6 => "SIGABRT",
            8 => "SIGFPE",
            9 => "SIGKILL",
            11 => "SIGSEGV",
            13 => "SIGPIPE",
            14 => "SIGALRM",
            15 => "SIGTERM",
            other => return format!("signal {other}"),
        };
        return name.to_owned();
    }
    status
        .code()
        .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
}
