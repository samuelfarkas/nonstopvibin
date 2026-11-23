//! Helpers shared by the core crate's integration tests.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use nsv_core::binary::{CorePin, CoreTarget, Sha256Digest};

/// A directory under the system temporary directory, removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "nsv-core-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // A crashed earlier run with the same process id may have left it.
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o777
}

/// sha256("core"), as tests/core-updates.test.ts states it.
pub(crate) const CORE_SHA256: &str =
    "0d45f5fd462b8c70bffb10021ac1bcff3f58f29b1faf7568595095427d42812c";

/// A pin for version `version` whose every target has `archive` and `binary` as hashes.
pub(crate) fn pin(version: &str, archive: &str, binary: &str) -> CorePin {
    let targets = |hash: &str| {
        serde_json::json!({
            "darwin_arm64": hash, "darwin_x64": hash, "linux_arm64": hash, "linux_x64": hash,
        })
    };
    serde_json::from_value(serde_json::json!({
        "version": version,
        "checksums": targets(archive),
        "binaries": targets(binary),
    }))
    .unwrap()
}

pub(crate) fn digest(path: &Path) -> Sha256Digest {
    Sha256Digest::of_file(path).unwrap()
}

pub(crate) const LINUX_X64: CoreTarget = CoreTarget::LinuxX64;
