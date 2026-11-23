//! App updates from GitHub Releases, as electron-updater ran them (desktop/updates.ts and the
//! update parts of desktop/main.ts): packaged builds with `updatesEnabled` update on macOS and
//! from an `AppImage`; a deb or a development run never does. The first check runs 30 s after
//! start, then every 6 h. A found update is downloaded, verified and staged beside the app
//! before "An update is ready" asks; "Restart to update" stops the daemon and swaps it in.
//!
//! A release is trusted when `SHA256SUMS` carries the release key's minisign signature (when
//! this build embeds `build/update-signing.pub`), the feed and the payload match their lines in
//! it, and the payload matches the feed's size and SHA-512. SECURITY.md has the trust model.

mod feed;
#[cfg(target_os = "linux")]
mod linux;
mod machine;
#[cfg(target_os = "macos")]
mod macos;
mod source;
mod version;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use minisign_verify::PublicKey;
use nsv_desktop::pipe::DialogKind;
use sha2::{Digest, Sha256, Sha512};
use tokio::time::{Instant, Interval, MissedTickBehavior};

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;

use self::feed::{Rejected, Sums};
use self::machine::{Checked, Command, Machine};
use self::source::Source;
use self::version::Version;
use crate::log;

const FIRST_CHECK: Duration = Duration::from_secs(30);
const EVERY: Duration = Duration::from_hours(6);
const SMALL_FILE: u64 = 64 * 1024;
const ATOM: u64 = 4 * 1024 * 1024;

/// This build's release key, `build/update-signing.pub` when it was committed at build time.
fn release_key() -> Option<PublicKey> {
    option_env!("NONSTOPVIBIN_UPDATE_KEY").map(|key| {
        #[expect(clippy::expect_used, reason = "build.rs parsed the same key")]
        PublicKey::from_base64(key).expect("a minisign public key")
    })
}

pub(crate) struct Updates(Mode);

enum Mode {
    /// A development run, a deb, or updates switched off: "Check for updates…" stays disabled.
    Off,
    /// macOS cannot replace the bundle where it is: the menu item says to move it.
    MoveFirst,
    On(Box<Updater>),
}

/// What [`platform::Target::detect`] found.
enum Detected<T> {
    None,
    #[cfg_attr(
        not(target_os = "macos"),
        expect(
            dead_code,
            reason = "an AppImage is not checked, as electron-updater did not"
        )
    )]
    NotReplaceable,
    Target(T),
}

struct Updater {
    target: platform::Target,
    current: Version,
    machine: Machine<Staged>,
    timer: Interval,
    check: Option<Pin<Box<dyn Future<Output = Checked<Staged>>>>>,
}

impl Updates {
    /// What this installation allows. Leftovers of an earlier update are removed.
    pub(crate) fn detect(packaged: bool) -> Self {
        #[expect(
            clippy::expect_used,
            reason = "the workspace version is X.Y.Z, which a version test parses"
        )]
        let current = Version::parse(env!("CARGO_PKG_VERSION")).expect("an X.Y.Z version");
        if !packaged || !cfg!(updates_enabled) {
            return Self(Mode::Off);
        }
        match platform::Target::detect() {
            Detected::None => Self(Mode::Off),
            Detected::NotReplaceable => Self(Mode::MoveFirst),
            Detected::Target(target) => {
                // What an earlier update left: a download, or the version it replaced.
                remove(&staging(&target));
                let mut timer = tokio::time::interval_at(Instant::now() + FIRST_CHECK, EVERY);
                timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
                Self(Mode::On(Box::new(Updater {
                    target,
                    current,
                    machine: Machine::new(),
                    timer,
                    check: None,
                })))
            }
        }
    }

    /// Whether "Check for updates…" is enabled.
    pub(crate) fn possible(&self) -> bool {
        !matches!(self.0, Mode::Off)
    }

    /// "Check for updates…" (`manual`) or the timer: the dialog to show, if any.
    pub(crate) fn check(&mut self, manual: bool) -> Option<DialogKind> {
        match &mut self.0 {
            Mode::Off => None,
            Mode::MoveFirst => manual.then_some(DialogKind::MoveToApplications),
            Mode::On(updater) => updater.command(|machine| machine.check(manual)),
        }
    }

    /// The next timer tick or finished check: the dialog it shows, if any. Pending for good
    /// when updates are off, so an idle daemon wakes only for the timer.
    pub(crate) async fn next(&mut self) -> Option<DialogKind> {
        let Mode::On(updater) = &mut self.0 else {
            return std::future::pending().await;
        };
        let checked = if let Some(check) = &mut updater.check {
            tokio::select! {
                _ = updater.timer.tick() => None,
                checked = check => Some(checked),
            }
        } else {
            updater.timer.tick().await;
            None
        };
        match checked {
            None => updater.command(|machine| machine.check(false)),
            Some(checked) => {
                updater.check = None;
                updater.command(|machine| machine.checked(checked))
            }
        }
    }

    /// "Restart to update": the staged update, while its dialog is the one open.
    pub(crate) fn restart(&mut self) -> Option<Staged> {
        match &mut self.0 {
            Mode::On(updater) => updater.machine.restart(),
            _ => None,
        }
    }

    /// An update dialog closed; `None` when the helper exited with its windows.
    pub(crate) fn closed(&mut self, kind: Option<DialogKind>) {
        if let Mode::On(updater) = &mut self.0 {
            updater.machine.closed(kind);
        }
    }
}

impl Updater {
    fn command(
        &mut self,
        step: impl FnOnce(&mut Machine<Staged>) -> Option<Command>,
    ) -> Option<DialogKind> {
        match step(&mut self.machine)? {
            Command::Show(kind) => Some(kind),
            Command::Check => {
                let (target, current) = (self.target.clone(), self.current);
                self.check = Some(Box::pin(async move {
                    match find(&target, current).await {
                        Ok(Some(staged)) => Checked::Ready(staged),
                        Ok(None) => Checked::UpToDate,
                        Err(error) => {
                            // Generic: remote bodies never reach the log.
                            log(format_args!("app update failed: {error}"));
                            remove(&staging(&target));
                            Checked::Failed
                        }
                    }
                }));
                None
            }
        }
    }
}

/// Why a check failed; logged without any remote body.
#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error(transparent)]
    Rejected(#[from] Rejected),
    #[error("could not fetch the release: {0}")]
    Net(#[from] nsv_net::NetError),
    #[error("{0}")]
    Local(String),
}

/// Finds the newest release this build takes, then downloads, verifies and stages it.
async fn find(target: &platform::Target, current: Version) -> Result<Option<Staged>, Failure> {
    let key = release_key();
    let source = Source::new().map_err(Failure::Local)?;
    let atom = source
        .get("releases.atom", "application/atom+xml", ATOM)
        .await?;
    let Some(version) = feed::newest(&String::from_utf8_lossy(&atom), current) else {
        return Ok(None);
    };
    let release = format!("releases/download/v{version}/");
    let sums = source
        .get(&format!("{release}SHA256SUMS"), "*/*", SMALL_FILE)
        .await?;
    if let Some(key) = &key {
        let signature = source
            .get(&format!("{release}SHA256SUMS.minisig"), "*/*", SMALL_FILE)
            .await?;
        let signature = text(&signature, "SHA256SUMS.minisig is not text")?;
        feed::verify_signature(key, &sums, signature, version)?;
    }
    let sums = Sums::parse(text(&sums, "SHA256SUMS is not text")?)?;
    let feed = source
        .get(&format!("{release}{}", platform::FEED), "*/*", SMALL_FILE)
        .await?;
    if Sha256::digest(&feed)[..] != sums.of(platform::FEED)?[..] {
        return Err(Rejected("the update feed does not match SHA256SUMS").into());
    }
    let payload = feed::payload(
        text(&feed, "the update feed is not text")?,
        version,
        platform::PAYLOAD,
    )?;
    let sha256 = *sums.of(&payload.name)?;
    let staging = staging(target);
    remove(&staging);
    tokio::fs::create_dir(&staging)
        .await
        .map_err(|error| Failure::Local(format!("could not stage the update: {error}")))?;
    let file = staging.join("payload");
    let download = source
        .download(&format!("{release}{}", payload.name), &file, payload.size)
        .await?;
    if download.size != payload.size || download.sha256 != sha256 {
        return Err(Rejected("the update does not match SHA256SUMS").into());
    }
    let path = file.clone();
    let sha512 = tokio::task::spawn_blocking(move || sha512_of(&path))
        .await
        .map_err(|error| Failure::Local(error.to_string()))?
        .map_err(|error| Failure::Local(format!("could not read the update: {error}")))?;
    if sha512 != payload.sha512 {
        return Err(Rejected("the update does not match its feed").into());
    }
    let new = target
        .prepare(&staging, &file)
        .await
        .map_err(Failure::Local)?;
    Ok(Some(Staged {
        new,
        current: target.path().to_owned(),
        executable: target.executable().to_owned(),
        staging,
    }))
}

fn text<'a>(bytes: &'a [u8], invalid: &'static str) -> Result<&'a str, Rejected> {
    std::str::from_utf8(bytes).map_err(|_| Rejected(invalid))
}

/// A verified update beside the app, ready to swap in.
#[derive(Debug)]
pub(crate) struct Staged {
    new: PathBuf,
    current: PathBuf,
    executable: PathBuf,
    staging: PathBuf,
}

impl Staged {
    /// Swaps the update in and starts it; on failure the running version is put back. The
    /// version it replaced stays in the staging folder, which the updated app removes.
    /// Synchronous: no cancellation can stop it half way.
    pub(crate) fn install(self) -> Result<(), String> {
        let swapped = Swapped::swap(&self.new, &self.current, &self.staging)
            .map_err(|error| format!("could not replace the app: {error}"))?;
        if let Err(error) = relaunch(&self.executable) {
            if let Err(undo) = swapped.undo() {
                log(format_args!(
                    "could not put the running version back: {undo}"
                ));
            }
            return Err(format!("could not start the updated app: {error}"));
        }
        Ok(())
    }
}

/// A swap of the update and the app it replaces.
struct Swapped<'a> {
    current: &'a Path,
    /// Where the replaced version is now.
    old: PathBuf,
    staging: &'a Path,
    exchanged: bool,
}

impl<'a> Swapped<'a> {
    /// One atomic exchange (`renameat2` `RENAME_EXCHANGE`, `renamex_np` `RENAME_SWAP`), so the
    /// app's path always holds a whole version. File systems without it get two renames.
    fn swap(new: &Path, current: &'a Path, staging: &'a Path) -> std::io::Result<Self> {
        match exchange(new, current) {
            Ok(()) => {
                return Ok(Self {
                    current,
                    old: new.to_owned(),
                    staging,
                    exchanged: true,
                });
            }
            Err(error) if !unsupported(&error) => return Err(error),
            Err(_) => {}
        }
        let old = staging.join("previous");
        std::fs::rename(current, &old)?;
        if let Err(error) = std::fs::rename(new, current) {
            std::fs::rename(&old, current)?;
            return Err(error);
        }
        Ok(Self {
            current,
            old,
            staging,
            exchanged: false,
        })
    }

    fn undo(self) -> std::io::Result<()> {
        if self.exchanged {
            return exchange(&self.old, self.current);
        }
        std::fs::rename(self.current, self.staging.join("failed"))?;
        std::fs::rename(&self.old, self.current)
    }
}

fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, a, CWD, b, RenameFlags::EXCHANGE).map_err(std::io::Error::from)
}

/// The file system or kernel cannot exchange: NFS, FAT, or an old kernel.
fn unsupported(error: &std::io::Error) -> bool {
    use rustix::io::Errno;
    error.raw_os_error().is_some_and(|code| {
        [Errno::INVAL, Errno::NOSYS, Errno::NOTSUP, Errno::OPNOTSUPP]
            .iter()
            .any(|errno| errno.raw_os_error() == code)
    })
}

/// `<folder>/.<app name>.update`: beside the app, so the swap stays on one file system.
fn staging(target: &platform::Target) -> PathBuf {
    let path = target.path();
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.update"))
}

fn sha512_of(path: &Path) -> std::io::Result<[u8; 64]> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha512::new();
    let mut buffer = vec![0; 256 * 1024];
    loop {
        match file.read(&mut buffer)? {
            0 => return Ok(hash.finalize().into()),
            read => hash.update(buffer.get(..read).unwrap_or_default()),
        }
    }
}

/// Removes a staged update or its leftovers, whatever state a run left it in.
fn remove(path: &Path) {
    let removed = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };
    if let Err(error) = removed {
        log(format_args!("could not remove {}: {error}", path.display()));
    }
}

/// Starts the updated app as a process of its own, with this one's environment and log.
fn relaunch(executable: &Path) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(executable)
        .stdin(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(drop)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// The app at `path`: a file, or a bundle-like folder holding one.
    fn app(path: &Path, folder: bool, version: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        if folder {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("version"), version).unwrap();
        } else {
            fs::write(path, version).unwrap();
        }
    }

    fn version(path: &Path) -> String {
        let file = if path.is_dir() {
            path.join("version")
        } else {
            path.to_owned()
        };
        fs::read_to_string(file).unwrap()
    }

    #[test]
    fn install_swaps_the_update_in_or_puts_the_running_version_back_when_it_cannot_start() {
        let root = std::env::temp_dir().join(format!("nsv-update-install-{}", std::process::id()));
        for folder in [false, true] {
            for (executable, installed) in [("/usr/bin/true", "new"), ("/nonexistent/app", "old")] {
                drop(fs::remove_dir_all(&root));
                let current = root.join("Applications/NonstopVibin.app");
                let staging = root.join("Applications/.NonstopVibin.app.update");
                let new = staging.join("unpacked/NonstopVibin.app");
                app(&current, folder, "old");
                app(&new, folder, "new");
                let result = Staged {
                    new,
                    current: current.clone(),
                    executable: PathBuf::from(executable),
                    staging: staging.clone(),
                }
                .install();
                assert_eq!(result.is_ok(), installed == "new", "{result:?}");
                assert_eq!(version(&current), installed);
                // The next start removes whatever the install left beside the app.
                remove(&staging);
                assert!(!staging.exists());
                assert_eq!(version(&current), installed);
            }
        }
        fs::remove_dir_all(&root).unwrap();
    }
}
