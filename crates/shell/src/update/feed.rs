//! What a release publishes for the updater, parsed where it arrives: the newest release's tag,
//! `latest-<platform>.yml` (scripts/package-common.ts `writeFeed`), and `SHA256SUMS` with its
//! minisign signature (scripts/sign-release.ts). Every check fails with a fixed message: a
//! remote body never reaches a log.

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use minisign_verify::{PublicKey, Signature};
use nsv_domain::version::AppVersion;

/// Why a release was refused; the text is what the daemon logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub(crate) struct Rejected(pub(crate) &'static str);

/// The repository's `releases.atom`: its newest entries, prereleases included. Each entry links
/// to its tag with this exact element; release notes are escaped, so they cannot forge one.
const TAG_LINK: &str = concat!(
    "<link rel=\"alternate\" type=\"text/html\" href=\"",
    env!("CARGO_PKG_REPOSITORY"),
    "/releases/tag/v"
);

/// The newest release in `atom` that a build at `current` installs ([`AppVersion::takes`]).
pub(crate) fn newest(atom: &str, current: AppVersion) -> Option<AppVersion> {
    atom.split(TAG_LINK)
        .skip(1)
        .filter_map(|rest| AppVersion::parse(rest.split('"').next().unwrap_or_default()))
        .filter(|&release| current.takes(release))
        .max()
}

/// The update payload a feed lists, by file extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Payload {
    /// A bare file name in the release.
    pub(crate) name: String,
    pub(crate) sha512: [u8; 64],
    pub(crate) size: u64,
}

/// The payload ending in `extension` from a `latest-<platform>.yml` whose version is `version`.
pub(crate) fn payload(
    feed: &str,
    version: AppVersion,
    extension: &str,
) -> Result<Payload, Rejected> {
    let malformed = Rejected("the update feed is malformed");
    let mut feed_version = None;
    let mut files: Vec<BTreeMap<&str, &str>> = Vec::new();
    let mut in_files = false;
    for line in feed.lines() {
        let content = line.trim_start();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        if content.len() == line.len() {
            let (key, value) = entry(content).ok_or(malformed)?;
            in_files = key == "files" && value.is_empty();
            if key == "version" {
                feed_version = Some(value);
            }
        } else if !in_files {
            return Err(malformed);
        } else if let Some(item) = content.strip_prefix("- ") {
            let (key, value) = entry(item).ok_or(malformed)?;
            files.push(BTreeMap::from([(key, value)]));
        } else {
            let (key, value) = entry(content).ok_or(malformed)?;
            files.last_mut().ok_or(malformed)?.insert(key, value);
        }
    }
    if feed_version.and_then(AppVersion::parse) != Some(version) {
        return Err(Rejected("the update feed is for another version"));
    }
    let file = files
        .iter()
        .find(|file| file.get("url").is_some_and(|url| url.ends_with(extension)))
        .ok_or(Rejected(
            "the update feed lists no payload for this platform",
        ))?;
    let name = file.get("url").copied().unwrap_or_default();
    if !bare_name(name) {
        return Err(Rejected("the update payload is not a release file"));
    }
    let sha512 = file
        .get("sha512")
        .and_then(|text| STANDARD.decode(text).ok())
        .and_then(|bytes| <[u8; 64]>::try_from(bytes).ok())
        .ok_or(malformed)?;
    let size = file
        .get("size")
        .and_then(|text| text.parse().ok())
        .ok_or(malformed)?;
    Ok(Payload {
        name: name.to_owned(),
        sha512,
        size,
    })
}

/// `key: value` with the value unquoted; YAML's single quotes double an inner quote. Values
/// that need escapes do not occur in a feed.
fn entry(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once(':')?;
    let value = value.trim();
    let value = match value.as_bytes() {
        [b'\'', .., b'\''] | [b'"', .., b'"'] => {
            let inner = value.get(1..value.len() - 1)?;
            if inner.contains(['\'', '"', '\\']) {
                return None;
            }
            inner
        }
        _ => value,
    };
    Some((key.trim(), value))
}

/// A file name a release can hold: no folder, no `..`, nothing to escape.
fn bare_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// `SHA256SUMS`: `sha256sum`'s `<hex>  <name>` lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sums(BTreeMap<String, [u8; 32]>);

impl Sums {
    pub(crate) fn parse(text: &str) -> Result<Self, Rejected> {
        let malformed = Rejected("SHA256SUMS is malformed");
        let mut sums = BTreeMap::new();
        for line in text.lines().filter(|line| !line.is_empty()) {
            let (hex, name) = line.split_once("  ").ok_or(malformed)?;
            let hash = hex_digest(hex).ok_or(malformed)?;
            if !bare_name(name) || sums.insert(name.to_owned(), hash).is_some() {
                return Err(malformed);
            }
        }
        Ok(Self(sums))
    }

    pub(crate) fn of(&self, name: &str) -> Result<&[u8; 32], Rejected> {
        self.0
            .get(name)
            .ok_or(Rejected("SHA256SUMS does not list a file the update needs"))
    }
}

fn hex_digest(hex: &str) -> Option<[u8; 32]> {
    let mut digest = [0; 32];
    let (pairs, []) = hex.as_bytes().as_chunks::<2>() else {
        return None;
    };
    if pairs.len() != digest.len() {
        return None;
    }
    for (byte, pair) in digest.iter_mut().zip(pairs) {
        let text = std::str::from_utf8(pair).ok()?;
        if text.bytes().any(|b| b.is_ascii_uppercase()) {
            return None;
        }
        *byte = u8::from_str_radix(text, 16).ok()?;
    }
    Some(digest)
}

/// Checks `SHA256SUMS.minisig` over `sums` with the release key. The trusted comment, which
/// the signature covers, must name `version` as scripts/sign-release.ts writes it, so a
/// signature from another release does not carry over.
pub(crate) fn verify_signature(
    key: &PublicKey,
    sums: &[u8],
    signature: &str,
    version: AppVersion,
) -> Result<(), Rejected> {
    let bad = Rejected("SHA256SUMS is not signed by the release key");
    let signature = Signature::decode(signature).map_err(|_| bad)?;
    key.verify(sums, &signature, false).map_err(|_| bad)?;
    if signature.trusted_comment() != format!("NonstopVibin {version} SHA256SUMS") {
        return Err(Rejected("SHA256SUMS is signed for another version"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> AppVersion {
        AppVersion::parse(text).unwrap()
    }

    // writeFeed's output for a mac release, as YAML.stringify writes it.
    const FEED: &str = "version: 0.3.0
files:
  - url: NonstopVibin-0.3.0-mac-arm64.zip
    sha512: AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+Pw==
    size: 123456
  - url: NonstopVibin-0.3.0-mac-arm64.dmg
    sha512: Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pw==
    size: 999
path: NonstopVibin-0.3.0-mac-arm64.zip
sha512: AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+Pw==
releaseDate: '2026-11-23T10:00:00.000Z'
";

    #[test]
    fn the_feed_names_its_payload_with_hash_and_size() {
        let payload = payload(FEED, v("0.3.0"), ".zip").unwrap();
        assert_eq!(payload.name, "NonstopVibin-0.3.0-mac-arm64.zip");
        assert_eq!(payload.size, 123_456);
        assert_eq!(payload.sha512[..4], [0, 1, 2, 3]);
        assert_eq!(payload.sha512[63], 63);
        assert_eq!(super::payload(FEED, v("0.3.0"), ".dmg").unwrap().size, 999);
    }

    #[test]
    fn the_feed_must_match_its_release_and_list_a_safe_payload() {
        assert_eq!(
            payload(FEED, v("0.3.1"), ".zip"),
            Err(Rejected("the update feed is for another version"))
        );
        assert_eq!(
            payload(FEED, v("0.3.0"), ".AppImage"),
            Err(Rejected(
                "the update feed lists no payload for this platform"
            ))
        );
        let escaping = FEED.replace("- url: NonstopVibin", "- url: ../NonstopVibin");
        assert_eq!(
            payload(&escaping, v("0.3.0"), ".zip"),
            Err(Rejected("the update payload is not a release file"))
        );
        for broken in [
            FEED.replace("    size: 123456", "    size: lots"),
            FEED.replace("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+Pw==\n    size", "AAEC\n    size"),
            FEED.replace("files:", "files: []"),
            FEED.replace("version: 0.3.0", "  version: 0.3.0"),
            "version: '0.3.0\\u0000'\n".to_owned(),
        ] {
            assert!(payload(&broken, v("0.3.0"), ".zip").is_err(), "{broken}");
        }
    }

    #[test]
    fn the_newest_release_this_build_takes_comes_from_the_atom_feed() {
        let link = |tag: &str| {
            format!(
                "<link rel=\"alternate\" type=\"text/html\" href=\"https://github.com/samuelfarkas/nonstopvibin/releases/tag/{tag}\"/>"
            )
        };
        let atom = format!(
            "<feed><entry>{}<content type=\"html\">&lt;a href=&quot;https://github.com/samuelfarkas/nonstopvibin/releases/tag/v9.0.0&quot;&gt;</content></entry>\
             <entry>{}</entry><entry>{}</entry><entry>{}</entry>\
             <entry><link rel=\"alternate\" type=\"text/html\" href=\"https://github.com/someone/else/releases/tag/v8.0.0\"/></entry></feed>",
            link("v0.3.0-beta.2"),
            link("core-pin"),
            link("v0.2.1"),
            link("v0.2.0"),
        );
        assert_eq!(newest(&atom, v("0.2.0")), Some(v("0.2.1")));
        assert_eq!(newest(&atom, v("0.3.0-beta.1")), Some(v("0.3.0-beta.2")));
        assert_eq!(newest(&atom, v("0.2.1")), None);
        assert_eq!(newest("<html>", v("0.2.0")), None);
    }

    const SUMS: &str = "1111111111111111111111111111111111111111111111111111111111111111  latest-linux.yml
2222222222222222222222222222222222222222222222222222222222222222  NonstopVibin-0.3.0-linux-x86_64.AppImage
";

    #[test]
    fn sums_list_each_release_file_once() {
        let sums = Sums::parse(SUMS).unwrap();
        assert_eq!(sums.of("latest-linux.yml"), Ok(&[0x11; 32]));
        assert_eq!(
            sums.of("NonstopVibin-0.3.0-linux-x86_64.AppImage"),
            Ok(&[0x22; 32])
        );
        assert!(sums.of("other").is_err());
        for broken in [
            SUMS.replace("1111", "111"),
            SUMS.replace("1111", "ABCD"),
            SUMS.replace("  latest", " latest"),
            SUMS.replace("latest-linux.yml", "../latest-linux.yml"),
            format!("{SUMS}{}", SUMS.lines().next().unwrap_or_default()),
        ] {
            assert!(Sums::parse(&broken).is_err(), "{broken}");
        }
    }

    // Made with minisign 0.12 (`-G -W`, then `-S -t "NonstopVibin 0.3.0 SHA256SUMS"` over SUMS).
    const KEY: &str = "RWSVb2QEOrWXkgoJ9yLmtYuB5qIfemv0AB15EzPXE6lhKgq+fil8q9i7";
    const SIGNED: &str = "untrusted comment: signature from minisign secret key
RUSVb2QEOrWXksYsZiRL/WKOBpzD4l3w0wQnHMnuzYoz55nL2sKSoi+Tr1H/dzAgVC804mAKcD/Hbvs3qw4Knkr8o7uqkW8ZsQQ=
trusted comment: NonstopVibin 0.3.0 SHA256SUMS
2v56J2p+vMaXbn0lYdkOCuIbVGm0wzXOyinD2x29t1a0D96Rc8MvItX5y5bO9+TYkKzNj6t6gdG7OzXyMCs+AA==
";
    // The same file signed by another key.
    const OTHER_KEY: &str = "untrusted comment: signature from minisign secret key
RUTikmySp9ZJIIgO6AHAz3x32A9UgT/U0vJFc4NaGCtjw+n/NHMeAaVUIyMq1wwyiDjk/veKeTWCLhR41whVbyQfSrXlwO7sYAM=
trusted comment: NonstopVibin 0.3.0 SHA256SUMS
BYni2CFYhviA9VJR9wzBrpDETXnmZWNwxHquOZ0bWuWdNyYytJ2X9aG/cmc7A149R2zBf3synXH8SrCe3O+iAA==
";
    // The release key's signature of the same file for 0.2.9.
    const OLD_RELEASE: &str = "untrusted comment: signature from minisign secret key
RUSVb2QEOrWXksYsZiRL/WKOBpzD4l3w0wQnHMnuzYoz55nL2sKSoi+Tr1H/dzAgVC804mAKcD/Hbvs3qw4Knkr8o7uqkW8ZsQQ=
trusted comment: NonstopVibin 0.2.9 SHA256SUMS
1+a1MNb1DJtK1Jr/5gqi5OZi1XsH4eZ5Y/exlyHTn0Z4jD+7L/68kQUcbF7fyZIO2ciqFdkERLeXO99w3yZtBA==
";

    #[test]
    fn only_the_release_key_signing_this_version_verifies() {
        let key = PublicKey::from_base64(KEY).unwrap();
        let sums = SUMS.as_bytes();
        assert_eq!(verify_signature(&key, sums, SIGNED, v("0.3.0")), Ok(()));
        let unsigned = Err(Rejected("SHA256SUMS is not signed by the release key"));
        let tampered = SUMS.replace('1', "3");
        assert_eq!(
            verify_signature(&key, tampered.as_bytes(), SIGNED, v("0.3.0")),
            unsigned
        );
        assert_eq!(
            verify_signature(&key, sums, OTHER_KEY, v("0.3.0")),
            unsigned
        );
        assert_eq!(verify_signature(&key, sums, "", v("0.3.0")), unsigned);
        let edited = SIGNED.replace("0.3.0 SHA256SUMS", "0.4.0 SHA256SUMS");
        assert_eq!(verify_signature(&key, sums, &edited, v("0.4.0")), unsigned);
        assert_eq!(
            verify_signature(&key, sums, OLD_RELEASE, v("0.3.0")),
            Err(Rejected("SHA256SUMS is signed for another version"))
        );
    }
}
