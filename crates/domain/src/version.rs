//! The two version grammars: a core's `X.Y.Z`, and this app's release versions
//! (`scripts/release-channel.ts`), `X.Y.Z` or `X.Y.Z-beta.N` for a prerelease. Neither has
//! leading zeros.

use std::cmp::Ordering;
use std::fmt;
use std::sync::LazyLock;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A canonical `X.Y.Z` version, as a core pin, a manifest and a `cores/<version>` directory
/// write it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Version([u64; 3]);

impl Version {
    /// `None` unless `text` is `X.Y.Z` with decimal parts and no leading zeros.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = [0; 3];
        let mut pieces = text.split('.');
        for part in &mut parts {
            *part = number(pieces.next()?)?;
        }
        pieces.next().is_none().then_some(Self(parts))
    }
}

/// A decimal with no leading zero.
fn number(text: &str) -> Option<u64> {
    let canonical = text == "0"
        || (!text.starts_with('0') && !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()));
    canonical.then(|| text.parse().ok()).flatten()
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

#[expect(
    clippy::expect_used,
    reason = "the workspace version is a build-time fact that scripts/verify-release.mjs holds to the release grammar and the_package_version_parses checks, never input"
)]
static CURRENT: LazyLock<AppVersion> =
    LazyLock::new(|| AppVersion::parse(env!("CARGO_PKG_VERSION")).expect("a release version"));

/// A version of this app: a release, or a prerelease that sorts below its release.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AppVersion {
    release: Version,
    beta: Option<u64>,
}

impl AppVersion {
    /// This build's version, the workspace's.
    #[must_use]
    pub fn current() -> Self {
        *CURRENT
    }

    /// `None` for anything but `X.Y.Z` or `X.Y.Z-beta.N`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (release, beta) = match text.split_once("-beta.") {
            Some((release, beta)) => (release, Some(number(beta)?)),
            None => (text, None),
        };
        Some(Self {
            release: Version::parse(release)?,
            beta,
        })
    }

    #[must_use]
    pub const fn prerelease(self) -> bool {
        self.beta.is_some()
    }

    /// Whether a build at `self` installs `candidate`: never a downgrade or the same version,
    /// and a prerelease only from a prerelease, as electron-updater's `allowPrerelease`.
    #[must_use]
    pub fn takes(self, candidate: Self) -> bool {
        candidate > self && (self.prerelease() || !candidate.prerelease())
    }
}

impl Ord for AppVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let key = |v: &Self| (v.release, v.beta.is_none(), v.beta);
        key(self).cmp(&key(other))
    }
}

impl PartialOrd for AppVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for AppVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.release)?;
        if let Some(beta) = self.beta {
            write!(f, "-beta.{beta}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for AppVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AppVersion({self})")
    }
}

impl Serialize for AppVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for AppVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| serde::de::Error::custom("not an app release version"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> AppVersion {
        AppVersion::parse(text).unwrap()
    }

    #[test]
    fn the_package_version_parses() {
        assert_eq!(AppVersion::current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn core_versions_are_canonical_x_y_z() {
        assert_eq!(Version::parse("8.10.0").unwrap().to_string(), "8.10.0");
        assert_eq!(Version::parse("0.0.0").unwrap().to_string(), "0.0.0");
        for refused in [
            "8.1",
            "8.1.0.1",
            "v8.1.0",
            "08.1.0",
            "8.1.x",
            "8..0",
            "",
            "8.1.0-beta.1",
        ] {
            assert_eq!(Version::parse(refused), None, "{refused}");
        }
        assert!(Version::parse("8.10.0") > Version::parse("8.9.99"));
    }

    #[test]
    fn app_versions_are_releases_and_betas() {
        for valid in [
            "0.0.0",
            "0.2.0",
            "10.20.30",
            "1.2.3-beta.0",
            "1.2.3-beta.12",
        ] {
            assert_eq!(v(valid).to_string(), valid);
        }
        for invalid in [
            "",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "1.2.3-beta",
            "1.2.3-beta.01",
            "1.2.3-rc.1",
            "1.2.3-beta.1.2",
            "v1.2.3",
            "1.2.3 ",
            "+1.2.3",
            "1.2.x",
        ] {
            assert_eq!(AppVersion::parse(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn a_prerelease_sorts_below_its_release_and_numbers_compare_numerically() {
        let ordered = [
            "0.2.0-beta.1",
            "0.2.0-beta.2",
            "0.2.0-beta.10",
            "0.2.0",
            "0.2.1-beta.0",
            "0.2.1",
            "0.10.0",
            "1.0.0",
        ];
        for pair in ordered.windows(2) {
            if let [lower, higher] = pair {
                assert!(v(lower) < v(higher), "{lower} < {higher}");
            }
        }
    }

    #[test]
    fn releases_take_only_newer_releases_and_betas_take_newer_anything() {
        let cases = [
            ("0.2.0", "0.2.1", true),
            ("0.2.0", "0.2.0", false),
            ("0.2.0", "0.1.9", false),
            ("0.2.0", "0.3.0-beta.1", false),
            ("0.3.0-beta.1", "0.3.0-beta.2", true),
            ("0.3.0-beta.1", "0.3.0", true),
            ("0.3.0-beta.2", "0.3.0-beta.1", false),
            ("0.3.0-beta.1", "0.2.9", false),
        ];
        for (running, offered, expected) in cases {
            assert_eq!(
                v(running).takes(v(offered)),
                expected,
                "{running} → {offered}"
            );
        }
    }
}
