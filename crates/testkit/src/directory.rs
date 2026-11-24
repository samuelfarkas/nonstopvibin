#![expect(
    clippy::missing_panics_doc,
    clippy::unwrap_used,
    reason = "test support: a panic fails the test that called it, and is its report"
)]

use std::fs::{self, Permissions};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A directory under the system temporary directory, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    /// An empty `nsv-<name>-<process id>-<count>`.
    #[must_use]
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "nsv-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // A crashed earlier run with the same process id may have left it.
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The permission bits of `path`, not following a symbolic link.
#[must_use]
pub fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o777
}

pub fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
}

/// Writes an executable a test runs, or hands a child to run.
///
/// A test thread that forks while this process holds a file open for writing passes the
/// descriptor to its child until the child calls exec, and running the file meanwhile fails
/// with "Text file busy". The bytes go to a sibling file, and a `cp` process writes the
/// executable, so this process never holds it open for writing.
pub fn write_executable(path: &Path, contents: &[u8]) {
    let source = path.with_extension("source");
    fs::write(&source, contents).unwrap();
    copy_executable(&source, path);
}

/// Copies `from` to an executable `to` in a `cp` process; see [`write_executable`].
pub fn copy_executable(from: &Path, to: &Path) {
    let status = Command::new("cp").arg(from).arg(to).status().unwrap();
    assert!(status.success(), "cp {} {}", from.display(), to.display());
    set_mode(to, 0o755);
}
