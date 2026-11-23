//! Helpers shared by the core crate's integration tests.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use nsv_core::binary::{CorePin, CoreTarget, Sha256Digest};

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
