//! The daemon's single-instance lock: an owner-only `server.lock.sqlite` held in a
//! `BEGIN IMMEDIATE` transaction for the process lifetime.
//!
//! The file is never unlinked: SQLite's OS-backed writer lock is released on exit or crash,
//! with no stale-owner check-and-delete race. A separate database leaves the store's WAL
//! available to normal operations.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nsv_domain::sync::locked;
use rusqlite::{Connection, ErrorCode};

use crate::data_dir::DataDir;
use crate::files::{EntryKind, own_entry, owner_only_directory};

/// Directories this process holds. POSIX advisory locks are per process, so a second
/// descriptor for a locked inode must never be opened and closed here.
static OWNED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

fn owned() -> MutexGuard<'static, BTreeSet<PathBuf>> {
    locked(&OWNED)
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("The server data directory must be owner-only, owned by you, and not a symbolic link.")]
    UnsafeDirectory,
    /// Another process (or this one) holds the directory.
    #[error(
        "NonstopVibin is already starting, running or stopping for {}. Run `nonstopvibin url` for its link.",
        .0.display()
    )]
    Occupied(PathBuf),
    #[error("server.lock.sqlite must be an owner-only ordinary file you own, without links.")]
    UnsafeLockFile,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// Holds a data directory until released or dropped; [`DataDir::lock`] takes it.
#[derive(Debug)]
pub struct ServerLock {
    connection: Option<Connection>,
    directory: DataDir,
    canonical: PathBuf,
}

impl ServerLock {
    pub(crate) fn acquire(data: &DataDir) -> Result<Self, LockError> {
        let directory = data.path();
        owner_only_directory(directory, || {
            // Any mode: owner_only_directory tightens it next.
            if own_entry(&fs::symlink_metadata(directory)?, EntryKind::Directory, 0) {
                Ok(())
            } else {
                Err(LockError::UnsafeDirectory)
            }
        })?;
        let canonical = fs::canonicalize(directory)?;
        let mut owned = owned();
        if owned.contains(&canonical) {
            return Err(LockError::Occupied(canonical));
        }
        let path = DataDir::new(canonical.as_path()).server_lock();
        // O_CREAT|O_EXCL never follows a symbolic link: one at the path fails with EEXIST.
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let status = fs::symlink_metadata(&path)?;
        if !own_entry(&status, EntryKind::File, 0o077) || status.nlink() != 1 {
            return Err(LockError::UnsafeLockFile);
        }
        let connection = Connection::open(&path)?;
        connection.busy_timeout(Duration::ZERO)?;
        // Reserve the single writer without upgrading past concurrent SHARED readers:
        // EXCLUSIVE upgrades can make both initial contenders fail with SQLITE_BUSY.
        if let Err(error) = connection.execute_batch("BEGIN IMMEDIATE") {
            drop(connection);
            return Err(
                if error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) {
                    LockError::Occupied(canonical)
                } else {
                    error.into()
                },
            );
        }
        owned.insert(canonical.clone());
        Ok(Self {
            connection: Some(connection),
            directory: data.clone(),
            canonical,
        })
    }

    /// The directory held, as it was chosen.
    #[must_use]
    pub fn directory(&self) -> &DataDir {
        &self.directory
    }

    pub fn release(self) {
        drop(self);
    }
}

impl Drop for ServerLock {
    fn drop(&mut self) {
        drop(self.connection.take());
        owned().remove(&self.canonical);
    }
}
