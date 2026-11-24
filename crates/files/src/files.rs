//! Owner-only directories, checked reads and atomic writes. Every function here blocks on the
//! file system: call it from the store thread, a blocking task or before the runtime starts.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nsv_domain::random;
use rustix::fs::{Mode, OFlags};

/// Creates the directory (and missing parents) owner-only and tightens an existing one.
/// `vet` checks it after creation and before the mode changes, since chmod follows a symbolic
/// link.
///
/// # Errors
/// What `vet` returns, or a file-system error.
pub fn owner_only_directory<E: From<io::Error>>(
    directory: &Path,
    vet: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    vet()?;
    fs::set_permissions(directory, Permissions::from_mode(0o700))?;
    Ok(())
}

/// `mkdtemp`'s suffix characters.
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Creates a new owner-only directory `<prefix><six random characters>` in `parent`, as
/// `mkdtemp` does; `parent` must exist.
///
/// # Errors
/// A file-system error.
pub fn unique_directory(parent: &Path, prefix: &str) -> io::Result<PathBuf> {
    loop {
        let random = random::bytes::<6>()?;
        let suffix: String = random
            .iter()
            .filter_map(|byte| ALPHABET.get(usize::from(*byte) % ALPHABET.len()))
            .map(|&byte| char::from(byte))
            .collect();
        let path = parent.join(format!("{prefix}{suffix}"));
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

/// Whether an entry is a `kind`, not a symbolic link, owned by this user, and has none of the
/// `forbidden` mode bits: 0o077 for owner-only, 0o022 for no shared write access, 0 for any
/// mode. Pass the metadata of an open descriptor or of `symlink_metadata`.
#[must_use]
pub fn own_entry(status: &Metadata, kind: EntryKind, forbidden: u32) -> bool {
    let kind_matches = match kind {
        EntryKind::File => status.is_file(),
        EntryKind::Directory => status.is_dir(),
    };
    kind_matches
        && !status.is_symlink()
        && status.uid() == rustix::process::getuid().as_raw()
        && status.mode() & forbidden == 0
}

/// Reads a file without following a symbolic link (that fails with `ELOOP`) or blocking on a
/// FIFO. `None` when it is missing; `refused()` unless it is an ordinary file that passes
/// [`own_entry`] with `forbidden` and is at most `limit` bytes. Invalid UTF-8 becomes U+FFFD,
/// as Node's `readFile(..., "utf8")` did.
///
/// # Errors
/// `refused()`, or a file-system error other than a missing file.
pub fn read_own_file<E: From<io::Error>>(
    path: &Path,
    forbidden: u32,
    limit: u64,
    refused: impl FnOnce() -> E,
) -> Result<Option<String>, E> {
    let descriptor = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(io::Error::from(error).into()),
    };
    let mut file = File::from(descriptor);
    let status = file.metadata()?;
    if !own_entry(&status, EntryKind::File, forbidden) || status.len() > limit {
        return Err(refused());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }))
}

/// Writes `content` to a new `<path>.<uuid>.tmp` with `mode` and renames it over `path`, so a
/// reader sees the old or the new file, never a partial one. The file is synced before the
/// rename and the directory after it, so a power loss leaves the old or the new content rather
/// than an empty file: these are credentials and the user's own settings. That costs about
/// 7 ms per write on an ext4 disk. A directory that cannot be synced is logged, not returned:
/// the file was replaced by then, and callers act on whether it was.
///
/// # Errors
/// A file-system error before the rename; the temporary file is removed either way.
pub fn write_atomic(path: &Path, content: &[u8], mode: u32) -> io::Result<()> {
    replace(path, content, mode, true)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // Plain fsync: macOS's F_FULLFSYNC, which `sync_all` uses there, is for files.
    let synced = File::open(parent).and_then(|directory| Ok(rustix::fs::fsync(directory)?));
    if let Err(error) = synced {
        tracing::warn!(%error, "a replaced file's directory could not be synced");
    }
    Ok(())
}

/// [`write_atomic`] without syncing, for a file whose reader treats an empty or malformed copy
/// as absent and that a power loss makes stale anyway (`server.json`).
///
/// # Errors
/// A file-system error; the temporary file is removed either way.
pub fn write_atomic_unsynced(path: &Path, content: &[u8], mode: u32) -> io::Result<()> {
    replace(path, content, mode, false)
}

fn replace(path: &Path, content: &[u8], mode: u32, sync: bool) -> io::Result<()> {
    let mut name = OsString::from(path);
    name.push(format!(".{}.tmp", random::uuid()?));
    let temporary = PathBuf::from(name);
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(content)?;
            if sync { file.sync_all() } else { Ok(()) }
        })
        .and_then(|()| fs::rename(&temporary, path));
    let removed = match fs::remove_file(&temporary) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    };
    written.and(removed)
}
