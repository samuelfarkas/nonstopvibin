//! Cores must not outlive the process that started them. Layer 1: a shell watchdog per core
//! blocks reading a pipe only this process can write, so the kernel wakes it when this
//! process ends in any way (crash, SIGKILL). Layer 2: a ledger lets the next instance on the
//! same data directory reap cores whose owner is provably dead. Both identify a process by
//! PID plus start time and full command line, as `ps` reports them.

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use nsv_domain::id::ProfileId;
use nsv_domain::js::utf16_len;
use nsv_files::{DataDir, read_own_file};
use rustix::process::{Pid, Signal};
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio::sync::OnceCell;

/// How long a core gets to exit after SIGTERM before SIGKILL, wherever it is stopped.
pub const KILL_GRACE: Duration = Duration::from_secs(5);
const PS_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
/// What a core inherits, each only when set and not empty, as core.ts listed it: no API keys,
/// proxy settings, storage backends or provider variables.
const CORE_ENVIRONMENT: [&str; 6] = ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "SystemRoot"];

/// `$1` core PID, `$2` its exact command line. Captures the core's identity, then sleeps in
/// `read` until EOF: the write end lives only in the owning process (pipes are close-on-exec).
/// The owner kills this watchdog as soon as the core exits, so EOF means the owner died. Then:
/// SIGTERM, [`KILL_GRACE`] for a graceful exit (checked only during this shutdown), SIGKILL.
/// Byte for byte the script of core-processes.ts.
const WATCHDOG_SCRIPT: &str = r#"trap '' HUP INT
export PATH=/usr/bin:/bin:/usr/sbin:/sbin LC_ALL=C TZ=UTC
identity() { ps -ww -o lstart= -o command= -p "$1" 2>/dev/null; }
held=$(identity "$1")
command=${held#????????????????????????}
command=${command#"${command%%[! ]*}"}
[ -n "$held" ] && [ "$command" = "$2" ] || exit 0
while read -r _; do :; done
[ "$(identity "$1")" = "$held" ] || exit 0
kill -TERM "$1" 2>/dev/null
i=0
while [ $i -lt 50 ]; do
  sleep 0.1
  [ "$(identity "$1")" = "$held" ] || exit 0
  i=$((i + 1))
done
kill -KILL "$1" 2>/dev/null
exit 0"#;

/// Looks up a process's identity: `ps` start time and full command line, or `None` when no
/// such process runs. Tests inject failing lookups.
pub trait Identify {
    fn identify(&self, pid: u32) -> impl Future<Output = io::Result<Option<String>>> + Send;
}

/// The real lookup through `ps`.
#[derive(Debug, Clone)]
pub struct Ps {
    program: OsString,
}

impl Default for Ps {
    fn default() -> Self {
        Self::new("ps")
    }
}

impl Ps {
    /// A lookup through `program` instead of `ps`, for tests.
    #[must_use]
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl Identify for Ps {
    fn identify(&self, pid: u32) -> impl Future<Output = io::Result<Option<String>>> + Send {
        process_identity(self.program.clone(), pid)
    }
}

impl<F, Lookup> Identify for F
where
    F: Fn(u32) -> Lookup,
    Lookup: Future<Output = io::Result<Option<String>>> + Send,
{
    fn identify(&self, pid: u32) -> impl Future<Output = io::Result<Option<String>>> + Send {
        self(pid)
    }
}

/// `ps` start time and full command line, or `None` if `ps` reports no such process. Every
/// other failure is an error, `ps` not running at all included, so nothing unprovable reads
/// as gone.
async fn process_identity(ps: OsString, pid: u32) -> io::Result<Option<String>> {
    let output = Command::new(&ps)
        .args(["-ww", "-o", "lstart=", "-o", "command=", "-p"])
        .arg(pid.to_string())
        .env_clear()
        .env("PATH", PS_PATH)
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await?;
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(io::Error::other(format!("ps failed: {}", output.status)));
    }
    let identity = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!identity.is_empty()).then_some(identity))
}

/// Whether `identity` is a core started with a profile config inside `directory`: its command
/// ends in ` --config <directory>/profiles/<uuid>/config.yaml`. `lstart` is fixed-width in the
/// C locale, so the command starts after 24 characters.
fn core_of(directory: &DataDir, identity: &str) -> bool {
    let Some(command) = identity.get(24..).map(str::trim_start) else {
        return false;
    };
    let Some(profiles) = directory.profiles().to_str().map(str::to_owned) else {
        return false;
    };
    let prefix = format!(" --config {profiles}/");
    let suffix = "/config.yaml";
    let Some(at) = command.rfind(&prefix) else {
        return false;
    };
    let middle = command
        .get(at + prefix.len()..)
        .and_then(|rest| rest.strip_suffix(suffix));
    at > 0
        && middle.is_some_and(|id| {
            id.len() == 36
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
        })
}

fn signal(pid: u32, signal: Signal) {
    if let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) {
        // It may have exited between the identity check and the signal.
        let _ = rustix::process::kill_process(pid, signal);
    }
}

/// A running core and its watchdog. One task owns it and selects between [`CoreProcess::wait`]
/// and a request to [`CoreProcess::terminate`]: signals go only to a child not yet reaped,
/// so a reused PID is never hit. Dropping it closes the watchdog's pipe, and the watchdog
/// stops the core.
#[derive(Debug)]
pub struct CoreProcess {
    child: Child,
    pid: u32,
    watchdog: Result<Child, io::Error>,
}

/// The core's arguments; reaping orphaned cores recognises them by this command line.
#[must_use]
pub fn core_arguments(data: &DataDir, profile: &ProfileId) -> [OsString; 2] {
    [
        OsString::from("--config"),
        data.core_config(profile).into_os_string(),
    ]
}

/// Starts `binary` as `profile`'s core in its profile directory with the config arguments,
/// a minimal environment and no standard streams, then its watchdog. Without `/bin/sh` the
/// core still runs and only the ledger guards it; [`CoreProcess::watchdog`] says why.
///
/// # Errors
/// The core could not be started.
pub fn spawn_core(binary: &Path, data: &DataDir, profile: &ProfileId) -> io::Result<CoreProcess> {
    let arguments = core_arguments(data, profile);
    let mut command = Command::new(binary);
    command
        .args(&arguments)
        .current_dir(data.profile(profile))
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for name in CORE_ENVIRONMENT {
        if let Some(value) = std::env::var_os(name).filter(|value| !value.is_empty()) {
            command.env(name, value);
        }
    }
    let child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("the core exited before it had a PID"))?;
    let mut line = OsString::from(binary);
    for argument in &arguments {
        line.push(" ");
        line.push(argument);
    }
    // Right after spawn: the core cannot have been reaped and its PID reused yet.
    let watchdog = watch(pid, &line);
    Ok(CoreProcess {
        child,
        pid,
        watchdog,
    })
}

fn watch(pid: u32, command_line: &OsStr) -> io::Result<Child> {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(WATCHDOG_SCRIPT)
        .arg("nonstopvibin-core-watchdog")
        .arg(pid.to_string())
        .arg(command_line)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own process group: terminal signals aimed at the app's group must not take it first.
        .process_group(0)
        .spawn()
}

impl CoreProcess {
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// The watchdog's PID, or why it could not start.
    ///
    /// # Errors
    /// The watchdog's spawn error.
    pub fn watchdog(&self) -> Result<Option<u32>, &io::Error> {
        self.watchdog.as_ref().map(Child::id)
    }

    /// Waits for the core to exit, then stops its watchdog, which has nothing left to guard.
    /// Cancel-safe.
    ///
    /// # Errors
    /// Waiting failed.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait().await?;
        self.stop_watchdog().await;
        Ok(status)
    }

    /// SIGTERM, then SIGKILL after [`KILL_GRACE`]; answers the exit status. Cancel-safe.
    ///
    /// # Errors
    /// Waiting or signalling failed.
    pub async fn terminate(&mut self) -> io::Result<ExitStatus> {
        if self.child.try_wait()?.is_none() {
            signal(self.pid, Signal::TERM);
            if tokio::time::timeout(KILL_GRACE, self.child.wait())
                .await
                .is_err()
            {
                self.child.start_kill()?;
            }
        }
        self.wait().await
    }

    async fn stop_watchdog(&mut self) {
        if let Ok(watchdog) = &mut self.watchdog {
            // It may already have exited on its own; either way it is reaped here.
            let _ = watchdog.start_kill();
            let _ = watchdog.wait().await;
        }
    }

    fn exited(&mut self) -> io::Result<bool> {
        Ok(self.child.try_wait()?.is_some())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    pid: u32,
    identity: String,
    owner: Owner,
}

#[derive(Debug, Serialize, Deserialize)]
struct Owner {
    pid: u32,
    identity: String,
}

impl Entry {
    /// The checks of core-processes.ts's schema: positive PIDs and identities of at least 25
    /// UTF-16 units (a 24-character start time and a command).
    fn valid(&self) -> bool {
        let long = |identity: &str| utf16_len(identity) >= 25;
        self.pid > 0 && self.owner.pid > 0 && long(&self.identity) && long(&self.owner.identity)
    }
}

enum OwnerLookup {
    Empty,
    Failed(io::Error),
}

/// The ledger of running cores, `core-processes/<pid>.json` in the data directory, for the
/// next instance to reap after a crash.
#[derive(Debug)]
pub struct Ledger<I: Identify = Ps> {
    directory: DataDir,
    identify: I,
    /// This process's identity, once determined; a failed or empty lookup is retried.
    owner: OnceCell<String>,
}

impl<I: Identify> Ledger<I> {
    #[must_use]
    pub fn new(data: &DataDir, identify: I) -> Self {
        Self {
            directory: data.clone(),
            identify,
            owner: OnceCell::new(),
        }
    }

    /// Records a running core so a later instance can reap it if this process dies. Answers
    /// the entry's path, or `None` when the core already exited, this process's identity is
    /// unknown, or `ps` does not yet show the core's own command line (a read before its exec
    /// settles could never be reaped).
    ///
    /// # Errors
    /// An identity lookup failed, or the entry could not be written.
    pub async fn record(&self, core: &mut CoreProcess) -> io::Result<Option<PathBuf>> {
        let owner = self.owner.get_or_try_init(|| async {
            match self.identify.identify(std::process::id()).await {
                Ok(Some(identity)) => Ok(identity),
                Ok(None) => Err(OwnerLookup::Empty),
                Err(error) => Err(OwnerLookup::Failed(error)),
            }
        });
        let (identity, owner) = tokio::join!(self.identify.identify(core.pid), owner);
        let owner = match owner {
            Ok(owner) => owner.clone(),
            Err(OwnerLookup::Empty) => return Ok(None),
            Err(OwnerLookup::Failed(error)) => return Err(error),
        };
        let Some(identity) = identity? else {
            return Ok(None);
        };
        if !core_of(&self.directory, &identity) || core.exited()? {
            return Ok(None);
        }
        let ledger = self.directory.core_ledger();
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&ledger)?;
        let path = ledger.join(format!("{}.json", core.pid));
        let entry = Entry {
            pid: core.pid,
            identity,
            owner: Owner {
                pid: std::process::id(),
                identity: owner,
            },
        };
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?
            .write_all(serde_json::to_string(&entry)?.as_bytes())?;
        Ok(Some(path))
    }

    /// Stops cores recorded under this data directory whose owner has exited. Live owners,
    /// other directories and reused PIDs are never signalled; stale entries go. An entry
    /// whose owner or core cannot be identified this time (no `ps`) stays for the next start.
    ///
    /// # Errors
    /// A malformed entry could not be removed.
    pub async fn reap_orphans(&self) -> io::Result<()> {
        let ledger = self.directory.core_ledger();
        let Ok(listing) = fs::read_dir(&ledger) else {
            return Ok(());
        };
        let names: Vec<OsString> = listing
            .filter_map(|entry| entry.ok().map(|e| e.file_name()))
            .collect();
        for name in names {
            let path = ledger.join(&name);
            let entry = read_own_file(&path, 0, 64 * 1024, || {
                io::Error::other("not a ledger entry")
            })
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str::<Entry>(&text).ok())
            .filter(Entry::valid);
            let Some(entry) = entry else {
                forget(&path)?;
                continue;
            };
            // Unprovable this time (e.g. ps could not run): the entry stays for the next start.
            if let Err(error) = self.reap(&name, &path, &entry).await {
                tracing::warn!(%error, ?name, "a recorded core was not reaped; its entry stays");
            }
        }
        Ok(())
    }

    async fn reap(&self, name: &OsStr, path: &Path, entry: &Entry) -> io::Result<()> {
        if self.identify.identify(entry.owner.pid).await?.as_deref()
            == Some(entry.owner.identity.as_str())
        {
            return Ok(());
        }
        let ours = name.to_str() == Some(&format!("{}.json", entry.pid))
            && core_of(&self.directory, &entry.identity);
        if ours
            && self.identify.identify(entry.pid).await?.as_deref() == Some(entry.identity.as_str())
        {
            self.stop_orphan(entry).await?;
        }
        forget(path)
    }

    /// Not our child, so its exit cannot be awaited: this bounded poll runs only while reaping.
    async fn stop_orphan(&self, entry: &Entry) -> io::Result<()> {
        let same =
            async || -> io::Result<bool> {
                Ok(self.identify.identify(entry.pid).await?.as_deref()
                    == Some(entry.identity.as_str()))
            };
        signal(entry.pid, Signal::TERM);
        let step = Duration::from_millis(100);
        let mut waited = Duration::ZERO;
        while waited < KILL_GRACE {
            tokio::time::sleep(step).await;
            waited += step;
            if !same().await? {
                return Ok(());
            }
        }
        if same().await? {
            signal(entry.pid, Signal::KILL);
        }
        Ok(())
    }
}

/// Removes a ledger entry; a missing one is no error.
///
/// # Errors
/// A file-system error other than a missing file.
pub fn forget(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_core_of_a_directory_is_recognised_by_its_command_line_suffix() {
        let data = &DataDir::new("/home/u/.nonstopvibin");
        let id = "0b5a4f3e-1c2d-4e5f-8a9b-0c1d2e3f4a5b";
        let start = "Thu Jan  1 00:00:00 1970";
        let identity = |command: &str| format!("{start}     {command}");
        assert!(core_of(
            data,
            &identity(&format!(
                "/app/cli-proxy-api --config {}/profiles/{id}/config.yaml",
                data.path().display()
            ))
        ));
        for refused in [
            // Another data directory, a non-UUID profile, extra arguments, no binary before it.
            format!("/app/cli-proxy-api --config /other/profiles/{id}/config.yaml"),
            format!(
                "/app/cli-proxy-api --config {}/profiles/not-a-uuid/config.yaml",
                data.path().display()
            ),
            format!(
                "/app/cli-proxy-api --config {}/profiles/{id}/config.yaml --x",
                data.path().display()
            ),
            format!(
                "--config {}/profiles/{id}/config.yaml",
                data.path().display()
            ),
            format!(
                "/app/cli-proxy-api --config {}/profiles/{}/config.yaml",
                data.path().display(),
                id.to_uppercase()
            ),
        ] {
            assert!(!core_of(data, &identity(&refused)), "{refused}");
        }
        assert!(!core_of(data, "short"));
    }
}
