//! The versions this project releases (`scripts/release-channel.ts`): `X.Y.Z`, or
//! `X.Y.Z-beta.N` for a prerelease, with no leading zeros. A prerelease sorts below its release.

use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Version {
    core: [u64; 3],
    beta: Option<u64>,
}

impl Version {
    /// `None` for anything but `X.Y.Z` or `X.Y.Z-beta.N`.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let (core_text, beta) = match text.split_once("-beta.") {
            Some((core, beta)) => (core, Some(number(beta)?)),
            None => (text, None),
        };
        let mut core = [0; 3];
        let mut pieces = core_text.split('.');
        for part in &mut core {
            *part = number(pieces.next()?)?;
        }
        pieces.next().is_none().then_some(Self { core, beta })
    }

    pub(crate) const fn prerelease(self) -> bool {
        self.beta.is_some()
    }

    /// Whether a build at `self` installs `candidate`: never a downgrade or the same version,
    /// and a prerelease only from a prerelease, as electron-updater's `allowPrerelease`.
    pub(crate) fn takes(self, candidate: Self) -> bool {
        candidate > self && (self.prerelease() || !candidate.prerelease())
    }
}

/// A decimal with no leading zero.
fn number(text: &str) -> Option<u64> {
    let canonical = text == "0"
        || (!text.starts_with('0') && !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()));
    canonical.then(|| text.parse().ok()).flatten()
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        // A release (no beta) sorts above every prerelease of the same X.Y.Z.
        let key = |v: &Self| (v.core, v.beta.is_none(), v.beta);
        key(self).cmp(&key(other))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch] = self.core;
        write!(f, "{major}.{minor}.{patch}")?;
        if let Some(beta) = self.beta {
            write!(f, "-beta.{beta}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn the_package_version_parses() {
        assert!(Version::parse(env!("CARGO_PKG_VERSION")).is_some());
    }

    #[test]
    fn parses_only_release_and_beta_versions() {
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
            assert_eq!(Version::parse(invalid), None, "{invalid}");
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
