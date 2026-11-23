//! The core artifact: its layout, the reviewed release pin, the manifest beside the binary,
//! and the hash checks a core passes before it runs or ships. Every function that reads a
//! file blocks: run it on a blocking thread.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::{Arc, LazyLock};

use nsv_domain::error::AppError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::CoreError;

/// `crates/core/core-release.json`, the reviewed pin this build bundles. Installed builds read
/// the same file from `main` as their update feed, so it stays where it is.
const BUNDLED_PIN: &str = include_str!("../core-release.json");

/// Written by `bun run core:install`, relative to the checkout.
pub const CHECKOUT_DIRECTORY: &str = ".vendor/core";
/// Relative to the desktop app's resources and the server archive.
pub const PACKAGED_DIRECTORY: &str = "core";
pub const BINARY: &str = "cli-proxy-api";
pub const MANIFEST: &str = "manifest.json";
/// The release's licence, which an install beside a checkout keeps and packages ship.
pub const LICENSE: &str = "LICENSE";

/// A SHA-256, written as 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// `None` unless `text` is 64 lowercase hex digits.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let digits = text.as_bytes();
        if digits.len() != 64 {
            return None;
        }
        let mut bytes = [0; 32];
        for (byte, [high, low]) in bytes.iter_mut().zip(digits.as_chunks::<2>().0) {
            *byte = hex_value(*high)? << 4 | hex_value(*low)?;
        }
        Some(Self(bytes))
    }

    /// A file's SHA-256, streamed: a core is too large to read into one buffer.
    ///
    /// # Errors
    /// A file-system error.
    pub fn of_file(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 1 << 16];
        loop {
            let read = file.read(&mut buffer)?;
            let Some(chunk) = buffer.get(..read).filter(|chunk| !chunk.is_empty()) else {
                return Ok(Self(hash.finalize().into()));
            };
            hash.update(chunk);
        }
    }

    #[must_use]
    pub const fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

const fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

impl From<[u8; 32]> for Sha256Digest {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&nsv_domain::hex(&self.0))
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256Digest({self})")
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| serde::de::Error::custom("not a lowercase SHA-256"))
    }
}

/// A canonical `X.Y.Z` version (no leading zeros), as a pin, a manifest and a
/// `cores/<version>` directory write it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Version([u64; 3]);

impl Version {
    /// `None` unless `text` is `X.Y.Z` with decimal parts and no leading zeros.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = [0; 3];
        let mut pieces = text.split('.');
        for part in &mut parts {
            let piece = pieces.next()?;
            let canonical = piece == "0"
                || (!piece.starts_with('0')
                    && !piece.is_empty()
                    && piece.bytes().all(|b| b.is_ascii_digit()));
            if !canonical {
                return None;
            }
            *part = piece.parse().ok()?;
        }
        pieces.next().is_none().then_some(Self(parts))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch] = self.0;
        write!(f, "{major}.{minor}.{patch}")
    }
}

impl fmt::Debug for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Version({self})")
    }
}

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| serde::de::Error::custom("not an X.Y.Z version"))
    }
}

/// A platform and architecture a core is pinned for, by Node's names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoreTarget {
    DarwinArm64,
    DarwinX64,
    LinuxArm64,
    LinuxX64,
}

impl CoreTarget {
    pub const ALL: [Self; 4] = [
        Self::DarwinArm64,
        Self::DarwinX64,
        Self::LinuxArm64,
        Self::LinuxX64,
    ];

    /// The target `platform`/`arch` (Node's names) name, if a core is pinned for it.
    #[must_use]
    pub fn new(platform: &str, arch: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|target| target.platform() == platform && target.arch() == arch)
    }

    /// The target a pin's key (`<platform>_<arch>`) names.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        key.split_once('_')
            .and_then(|(platform, arch)| Self::new(platform, arch))
    }

    /// # Errors
    /// [`AppError`] 400 unless a core is pinned for `platform`/`arch`.
    pub fn parse(platform: &str, arch: &str) -> Result<Self, AppError> {
        Self::new(platform, arch).ok_or_else(|| {
            AppError::new("Supported core platforms: macOS/Linux arm64 and x64.", 400)
        })
    }

    /// The target this process runs on.
    ///
    /// # Errors
    /// As [`CoreTarget::parse`].
    pub fn current() -> Result<Self, AppError> {
        let platform = match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            other => other,
        };
        Self::parse(platform, arch)
    }

    #[must_use]
    pub const fn platform(self) -> &'static str {
        match self {
            Self::DarwinArm64 | Self::DarwinX64 => "darwin",
            Self::LinuxArm64 | Self::LinuxX64 => "linux",
        }
    }

    #[must_use]
    pub const fn arch(self) -> &'static str {
        match self {
            Self::DarwinArm64 | Self::LinuxArm64 => "arm64",
            Self::DarwinX64 | Self::LinuxX64 => "x64",
        }
    }

    /// The upstream release archive's name.
    #[must_use]
    pub fn archive(self, version: &Version) -> String {
        let suffix = match self.arch() {
            "x64" => "amd64",
            _ => "aarch64",
        };
        format!("CLIProxyAPI_{version}_{}_{suffix}.tar.gz", self.platform())
    }
}

/// One SHA-256 per target, keyed `<platform>_<arch>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Targets {
    darwin_arm64: Sha256Digest,
    darwin_x64: Sha256Digest,
    linux_arm64: Sha256Digest,
    linux_x64: Sha256Digest,
}

impl Targets {
    /// One digest per target, in [`CoreTarget::ALL`]'s order.
    #[must_use]
    pub const fn new(
        [darwin_arm64, darwin_x64, linux_arm64, linux_x64]: [Sha256Digest; 4],
    ) -> Self {
        Self {
            darwin_arm64,
            darwin_x64,
            linux_arm64,
            linux_x64,
        }
    }

    #[must_use]
    pub const fn get(&self, target: CoreTarget) -> Sha256Digest {
        match target {
            CoreTarget::DarwinArm64 => self.darwin_arm64,
            CoreTarget::DarwinX64 => self.darwin_x64,
            CoreTarget::LinuxArm64 => self.linux_arm64,
            CoreTarget::LinuxX64 => self.linux_x64,
        }
    }
}

/// A reviewed core release, `crates/core/core-release.json`: bundled into each build and read
/// from `main` by installed ones. Unknown keys are ignored, so a later field does not stop
/// older apps from reading it. Serialized in the file's key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorePin {
    pub version: Version,
    /// The oldest app version that may run this core.
    #[serde(rename = "minApp", default, skip_serializing_if = "Option::is_none")]
    pub min_app: Option<Version>,
    /// Each target's upstream archive SHA-256.
    pub checksums: Targets,
    /// Each target's extracted, unsigned executable SHA-256.
    pub binaries: Targets,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("the bundled core pin is invalid: {0}")]
pub struct PinError(Arc<serde_json::Error>);

/// The pin this build bundles.
///
/// # Errors
/// [`PinError`] if `core-release.json` does not parse, which its tests rule out.
pub fn bundled_pin() -> Result<&'static CorePin, PinError> {
    static PIN: LazyLock<Result<CorePin, PinError>> = LazyLock::new(|| {
        serde_json::from_str(BUNDLED_PIN).map_err(|error| PinError(Arc::new(error)))
    });
    PIN.as_ref().map_err(Clone::clone)
}

/// `manifest.json` beside the binary: `sha256` is the upstream archive's checksum and
/// `binary_sha256` the binary's hash, which desktop signing rewrites. Read leniently: a
/// missing field, or one that is not a string, is `None` and matches nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoreManifest {
    pub version: Option<String>,
    pub platform: Option<String>,
    pub arch: Option<String>,
    pub archive: Option<String>,
    pub sha256: Option<String>,
    pub binary_sha256: Option<String>,
}

impl CoreManifest {
    /// The manifest of a core installed from `pin` for `target`.
    #[must_use]
    pub fn installed(
        pin: &CorePin,
        target: CoreTarget,
        sha256: Sha256Digest,
        binary_sha256: Sha256Digest,
    ) -> Self {
        Self {
            version: Some(pin.version.to_string()),
            platform: Some(target.platform().to_owned()),
            arch: Some(target.arch().to_owned()),
            archive: Some(target.archive(&pin.version)),
            sha256: Some(sha256.to_string()),
            binary_sha256: Some(binary_sha256.to_string()),
        }
    }

    /// Reads `<directory>/manifest.json`.
    ///
    /// # Errors
    /// A file-system error, or [`CoreError::Json`] when it is not JSON.
    pub fn read(directory: &Path) -> Result<Self, CoreError> {
        let text = std::fs::read_to_string(directory.join(MANIFEST))?;
        let value: serde_json::Value = serde_json::from_str(&text)?;
        let field = |name: &str| {
            value
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        Ok(Self {
            version: field("version"),
            platform: field("platform"),
            arch: field("arch"),
            archive: field("archive"),
            sha256: field("sha256"),
            binary_sha256: field("binarySha256"),
        })
    }

    /// Writes `<directory>/manifest.json` as `JSON.stringify(manifest, null, 2)` did.
    ///
    /// # Errors
    /// A file-system error.
    pub fn write(&self, directory: &Path) -> io::Result<()> {
        // Built by hand: the manifest's key order is part of its shape, and a JSON map sorts.
        let entries: Vec<String> = [
            ("version", &self.version),
            ("platform", &self.platform),
            ("arch", &self.arch),
            ("archive", &self.archive),
            ("sha256", &self.sha256),
            ("binarySha256", &self.binary_sha256),
        ]
        .into_iter()
        .filter_map(|(name, value)| {
            let value = serde_json::Value::from(value.as_deref()?);
            Some(format!("  \"{name}\": {value}"))
        })
        .collect();
        let text = if entries.is_empty() {
            "{}".to_owned()
        } else {
            format!("{{\n{}\n}}", entries.join(",\n"))
        };
        std::fs::write(directory.join(MANIFEST), text)
    }
}

/// What [`check_core_directory`] holds a core's binary to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binary {
    /// The pin's hash for the target.
    Pinned,
    /// The hash in its manifest: macOS signing rewrote the binary and then its manifest.
    Signed,
}

/// Fails unless the core in `directory` is `pin`'s for `target`: the manifest's version and
/// archive checksum, its target, then its binary as `binary` says. Packaging and the server
/// archive build run this before shipping a core.
///
/// # Errors
/// [`AppError`] 500 with the reason, or a read error.
pub fn check_core_directory(
    pin: &CorePin,
    directory: &Path,
    target: CoreTarget,
    binary: Binary,
) -> Result<(), CoreError> {
    let manifest = CoreManifest::read(directory)?;
    let refuse = |message: String| Err(AppError::internal(message).into());
    let (platform, arch) = (
        manifest.platform.as_deref().unwrap_or_default(),
        manifest.arch.as_deref().unwrap_or_default(),
    );
    let archive_pinned = CoreTarget::new(platform, arch)
        .is_some_and(|named| manifest.sha256 == Some(pin.checksums.get(named).to_string()));
    if manifest.version != Some(pin.version.to_string()) || !archive_pinned {
        return refuse("The proxy core does not match the reviewed release pin.".to_owned());
    }
    if platform != target.platform() || arch != target.arch() {
        return refuse(format!(
            "The proxy core is {platform}/{arch}, but the target is {}/{}. Run core:install for the target first.",
            target.platform(),
            target.arch()
        ));
    }
    let hash = || Sha256Digest::of_file(&directory.join(BINARY));
    match binary {
        Binary::Signed => {
            if manifest.binary_sha256 != Some(hash()?.to_string()) {
                return refuse("The signed core differs from its manifest.".to_owned());
            }
        }
        Binary::Pinned => {
            if manifest.binary_sha256 != Some(pin.binaries.get(target).to_string()) {
                return refuse(
                    "The proxy core manifest does not match the reviewed binary pin. Run bun run core:install again."
                        .to_owned(),
                );
            }
            if hash()? != pin.binaries.get(target) {
                return refuse(
                    "The proxy core binary does not match the reviewed release pin. Run bun run core:install again."
                        .to_owned(),
                );
            }
        }
    }
    Ok(())
}

/// Checks the core binary against the `manifest.json` beside it and, with `pinned`, against
/// the binary hash that pin names for this platform. Desktop signing changes the binary and
/// refreshes its manifest, so only headless releases require the pin.
///
/// # Errors
/// [`AppError`] 503 when the binary is missing or fails either check; a read or JSON error
/// for the manifest.
pub fn verify_core(binary: &Path, pinned: Option<&CorePin>) -> Result<(), CoreError> {
    if !binary.exists() {
        return Err(AppError::new(
            format!(
                "The proxy core is missing at {}. Reinstall NonstopVibin.",
                binary.display()
            ),
            503,
        )
        .into());
    }
    let manifest = CoreManifest::read(binary.parent().unwrap_or(Path::new("")))?;
    let hash = Sha256Digest::of_file(binary)?;
    let pinned_ok = pinned.is_none_or(|pin| {
        CoreTarget::current().is_ok_and(|target| pin.binaries.get(target) == hash)
    });
    if manifest.binary_sha256 != Some(hash.to_string()) || !pinned_ok {
        return Err(AppError::new(
            "The proxy core failed its integrity check. Reinstall NonstopVibin.",
            503,
        )
        .into());
    }
    Ok(())
}
