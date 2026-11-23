//! Installing a reviewed core release: the reviewed pin feed, the download, the archive and
//! executable hash checks against the pin, and the `cores/` layout updates live in:
//! `cores/.staging-*` while a core installs, `cores/<version>/` once committed.

use std::fs::{self, Permissions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use nsv_domain::error::AppError;
use nsv_net::{Get, Https, NetError, Redirects, Response};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::CoreError;
use crate::binary::{BINARY, CoreManifest, CorePin, CoreTarget, Sha256Digest, Version};

/// Where upstream publishes each release's archives, under `v<version>/`.
pub const CORE_ARCHIVES: &str = "https://github.com/router-for-me/CLIProxyAPI/releases/download";
/// Hosts a release download may be redirected to: GitHub serves assets from its CDN.
pub const ARCHIVE_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];
/// The reviewed pin on `main`, which installed builds read; `NONSTOPVIBIN_CORE_FEED` replaces
/// it. Archives always come from [`CORE_ARCHIVES`].
pub const CORE_FEED: &str = "https://raw.githubusercontent.com/samuelfarkas/nonstopvibin/main/crates/core/core-release.json";
/// GitHub serves raw files from this host only.
const FEED_HOSTS: &[&str] = &["raw.githubusercontent.com"];
const FEED_TIMEOUT: Duration = Duration::from_secs(15);
const FEED_LIMIT: u64 = 64 * 1024;
/// Above any core archive or executable; executables are about 80 MB.
const SIZE_LIMIT: u64 = 256 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(10);
const MISMATCH: &str = "The downloaded core does not match the reviewed pin. Nothing was changed.";

fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

fn too_large(path: &Path) -> AppError {
    AppError::new(format!("{} is larger than any core.", file_name(path)), 502)
}

/// Streams `url` into a new owner-only file at `path`; answers its SHA-256.
///
/// # Errors
/// [`AppError`] 502 for any failure; the partial file is left for the caller to remove.
pub async fn download(https: &Https, url: &str, path: &Path) -> Result<Sha256Digest, AppError> {
    let name = url
        .split(['?', '#'])
        .next()
        .and_then(|address| address.rsplit('/').next())
        .unwrap_or_default();
    let request = Get {
        url,
        headers: &[],
        timeout: DOWNLOAD_TIMEOUT,
        limit: SIZE_LIMIT,
        redirects: Redirects::Follow(ARCHIVE_HOSTS),
    };
    match https.download(&request, path).await {
        Ok(download) => Ok(download.sha256.into()),
        Err(NetError::Status(status)) => Err(AppError::new(
            format!("Could not download {name} (HTTP {}).", status.as_u16()),
            502,
        )),
        Err(NetError::TooLarge { .. }) => Err(too_large(path)),
        Err(error) => Err(AppError::new(
            format!("Could not download {name}: {error}"),
            502,
        )),
    }
}

/// Where the reviewed pin and release archives come from: GitHub through [`Https`] in the
/// daemon; tests serve their own.
pub trait Releases: Send + Sync + 'static {
    /// The pin feed at `url`, whatever its status.
    fn feed(&self, url: &str) -> impl Future<Output = Result<Response, NetError>> + Send;

    /// See [`download`].
    fn download(
        &self,
        url: &str,
        path: &Path,
    ) -> impl Future<Output = Result<Sha256Digest, AppError>> + Send;
}

impl Releases for Https {
    async fn feed(&self, url: &str) -> Result<Response, NetError> {
        self.get(&Get {
            url,
            headers: &[],
            timeout: FEED_TIMEOUT,
            limit: FEED_LIMIT,
            redirects: Redirects::Follow(FEED_HOSTS),
        })
        .await
    }

    async fn download(&self, url: &str, path: &Path) -> Result<Sha256Digest, AppError> {
        download(self, url, path).await
    }
}

/// One source shared, as tests share theirs.
impl<T: Releases> Releases for std::sync::Arc<T> {
    fn feed(&self, url: &str) -> impl Future<Output = Result<Response, NetError>> + Send {
        T::feed(self, url)
    }

    fn download(
        &self,
        url: &str,
        path: &Path,
    ) -> impl Future<Output = Result<Sha256Digest, AppError>> + Send {
        T::download(self, url, path)
    }
}

/// The reviewed pin the feed at `url` serves.
///
/// # Errors
/// [`AppError`] 502 "Could not check for core updates: …" when the feed cannot be read or is
/// not a core pin.
pub async fn fetch_pin(releases: &impl Releases, url: &str) -> Result<CorePin, AppError> {
    let failed = |reason: &dyn std::fmt::Display| {
        AppError::new(format!("Could not check for core updates: {reason}"), 502)
    };
    let response = releases.feed(url).await.map_err(|error| match error {
        NetError::Status(status) => failed(&format_args!("HTTP {}.", status.as_u16())),
        other => failed(&other),
    })?;
    if !response.ok() {
        return Err(failed(&format_args!("HTTP {}.", response.status.as_u16())));
    }
    let body: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|error| failed(&error))?;
    serde_json::from_value(body).map_err(|_| failed(&"the feed is not a core pin."))
}

/// Streams `member` of the gzipped tar `archive` into a new file at `path` with exactly
/// `mode`; answers its SHA-256. Nothing else of the archive reaches the disk.
///
/// # Errors
/// [`AppError`] 502: the archive has no such member, it is larger than any core, or `tar`
/// or the file failed.
pub async fn extract(
    archive: &Path,
    member: &str,
    path: &Path,
    mode: u32,
) -> Result<Sha256Digest, AppError> {
    let failed =
        |error: io::Error| AppError::new(format!("Could not extract {member}: {error}"), 502);
    let mut tar = tokio::process::Command::new("tar")
        .arg("-xzOf")
        .arg(archive)
        .arg(member)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(failed)?;
    let mut output = tar
        .stdout
        .take()
        .ok_or_else(|| failed(io::Error::other("tar has no output")))?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .await
        .map_err(failed)?;
    // The mode is exact whatever the umask: packaging copies the executable's.
    file.set_permissions(Permissions::from_mode(mode))
        .await
        .map_err(failed)?;
    let mut hash = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0; 1 << 16];
    loop {
        let read = output.read(&mut buffer).await.map_err(failed)?;
        let Some(chunk) = buffer.get(..read).filter(|chunk| !chunk.is_empty()) else {
            break;
        };
        size = size.saturating_add(read as u64);
        if size > SIZE_LIMIT {
            return Err(too_large(path));
        }
        hash.update(chunk);
        file.write_all(chunk).await.map_err(failed)?;
    }
    file.flush().await.map_err(failed)?;
    let status = tar.wait().await.map_err(failed)?;
    if !status.success() {
        return Err(AppError::new(
            format!("The core archive has no {member}."),
            502,
        ));
    }
    Ok(Sha256Digest::from(<[u8; 32]>::from(hash.finalize())))
}

/// Installs `pin`'s core for `target` into `directory` as [`crate::verify_core`] reads it:
/// the executable and its manifest, plus the release's LICENSE with `license`. Fails unless
/// the archive and the executable both match the pin, leaving the directory for its caller to
/// discard; the archive is deleted either way.
///
/// # Errors
/// [`CoreError::App`] 502 for a failed download, extraction or pin check.
pub async fn install_core(
    releases: &impl Releases,
    archives: &str,
    pin: &CorePin,
    target: CoreTarget,
    directory: &Path,
    license: bool,
) -> Result<(), CoreError> {
    let name = target.archive(&pin.version);
    let archive = directory.join(&name);
    let installed = async {
        let url = format!("{archives}/v{}/{name}", pin.version);
        let sha256 = releases.download(&url, &archive).await?;
        install_archive(pin, target, &archive, sha256, directory, license).await
    }
    .await;
    match fs::remove_file(&archive) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => installed.and(Err(error.into())),
        _ => installed,
    }
}

/// The part of [`install_core`] after the download: `archive`, whose SHA-256 is
/// `archive_sha256`, is checked against the pin and extracted into `directory`.
///
/// # Errors
/// As [`install_core`].
pub async fn install_archive(
    pin: &CorePin,
    target: CoreTarget,
    archive: &Path,
    archive_sha256: Sha256Digest,
    directory: &Path,
    license: bool,
) -> Result<(), CoreError> {
    if archive_sha256 != pin.checksums.get(target) {
        return Err(AppError::new(MISMATCH, 502).into());
    }
    let binary_sha256 = extract(archive, BINARY, &directory.join(BINARY), 0o755).await?;
    if binary_sha256 != pin.binaries.get(target) {
        return Err(AppError::new(MISMATCH, 502).into());
    }
    if license {
        extract(archive, "LICENSE", &directory.join("LICENSE"), 0o644).await?;
    }
    CoreManifest::installed(pin, target, archive_sha256, binary_sha256).write(directory)?;
    Ok(())
}

/// A `cores/.staging-*` directory a core installs into. Dropping it uncommitted deletes it;
/// what a crash leaves is deleted by the next [`select_installed_core`]. Its methods block.
#[derive(Debug)]
pub struct Staging {
    path: PathBuf,
    cores: PathBuf,
    committed: bool,
}

impl Staging {
    /// Creates `cores` owner-only if needed, then a new staging directory inside it.
    ///
    /// # Errors
    /// A file-system error.
    pub fn create(cores: &Path) -> io::Result<Self> {
        nsv_store::files::owner_only_directory(cores, || Ok::<(), io::Error>(()))?;
        Ok(Self {
            path: nsv_store::files::unique_directory(cores, ".staging-")?,
            cores: cores.to_owned(),
            committed: false,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Replaces `cores/<version>` with this directory; answers the binary path inside it.
    ///
    /// # Errors
    /// A file-system error; the staging directory is deleted then.
    pub fn commit(mut self, version: Version) -> io::Result<PathBuf> {
        let target = self.cores.join(version.to_string());
        remove_entry(&target)?;
        fs::rename(&self.path, &target)?;
        self.committed = true;
        Ok(target.join(BINARY))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.committed {
            // Nothing can report from a drop; startup pruning deletes what remains.
            let _ = remove_entry(&self.path);
        }
    }
}

/// Deletes a file, a link or a directory tree; a missing entry is no error.
fn remove_entry(path: &Path) -> io::Result<()> {
    let removed = match fs::symlink_metadata(path) {
        Ok(status) if status.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match removed {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// A core profiles can start from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveCore {
    pub version: Version,
    pub binary: PathBuf,
}

/// Every entry of `cores`, or none when it is missing.
fn entries(cores: &Path) -> io::Result<Vec<String>> {
    match fs::read_dir(cores) {
        Ok(listing) => listing
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// Deletes every entry of `cores` except `keep`.
///
/// # Errors
/// A file-system error.
pub fn prune(cores: &Path, keep: Option<Version>) -> io::Result<()> {
    let keep = keep.map(|version| version.to_string());
    for name in entries(cores)? {
        if Some(&name) != keep.as_ref() {
            remove_entry(&cores.join(name))?;
        }
    }
    Ok(())
}

/// Whether `directory` holds an intact `version` core for `target`. A crash or a hand edit
/// can leave anything in `cores/`: an unreadable entry is no core.
fn intact(directory: &Path, version: Version, target: CoreTarget) -> Result<bool, CoreError> {
    let unreadable = |error: &io::Error| {
        matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory | io::ErrorKind::IsADirectory
        )
    };
    let manifest = match CoreManifest::read(directory) {
        Ok(manifest) => manifest,
        Err(CoreError::Json(_)) => return Ok(false),
        Err(CoreError::Io(error)) if unreadable(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    if manifest.version != Some(version.to_string())
        || manifest.platform.as_deref() != Some(target.platform())
        || manifest.arch.as_deref() != Some(target.arch())
    {
        return Ok(false);
    }
    match Sha256Digest::of_file(&directory.join(BINARY)) {
        Ok(hash) => Ok(manifest.binary_sha256 == Some(hash.to_string())),
        Err(error) if unreadable(&error) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// The newest intact core in `cores` above `bundled` for `target`. Every other entry is
/// deleted: older or equal versions, damaged ones, and staging a crash left behind. Blocks.
///
/// # Errors
/// A file-system error other than an unreadable entry.
pub fn select_installed_core(
    cores: &Path,
    bundled: Version,
    target: CoreTarget,
) -> Result<Option<ActiveCore>, CoreError> {
    let mut newer: Vec<Version> = entries(cores)?
        .iter()
        .filter_map(|name| Version::parse(name))
        .filter(|version| *version > bundled)
        .collect();
    newer.sort_unstable_by(|a, b| b.cmp(a));
    let mut selected = None;
    for version in newer {
        let directory = cores.join(version.to_string());
        if intact(&directory, version, target)? {
            selected = Some(ActiveCore {
                version,
                binary: directory.join(BINARY),
            });
            break;
        }
    }
    prune(cores, selected.as_ref().map(|core| core.version))?;
    Ok(selected)
}
