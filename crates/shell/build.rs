//! The shell's build-time configuration, read and checked here so that a bad value fails the
//! build instead of a check in an installed app:
//! - `build/update-signing.pub`, the release key, once the owner has committed it
//!   (docs/release-signing.md): from then on the updater accepts only releases whose
//!   `SHA256SUMS` that key signed. It becomes `NONSTOPVIBIN_UPDATE_KEY`.
//! - package.json's `updatesEnabled`, which the release preflight requires to be on. It becomes
//!   `cfg(updates_enabled)`.
//! - `NONSTOPVIBIN_TEST_UPDATE_FEED=http://127.0.0.1:<port>/`, which tests/linux-update.sh and
//!   tests/mac-update-rust.mjs set and release builds never do. It becomes
//!   `NONSTOPVIBIN_TEST_UPDATE_PORT`.

use std::path::Path;

fn main() -> Result<(), String> {
    // Read when the script runs, not baked in when it compiles: cargo reuses the compiled
    // script for a checkout at another path.
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is unset")?;
    let checkout = Path::new(&manifest).join("../..");
    release_key(&checkout.join("build"))?;
    updates_enabled(&checkout.join("package.json"))?;
    test_feed()
}

fn release_key(build: &Path) -> Result<(), String> {
    let key = build.join("update-signing.pub");
    // The folder: cargo then notices the key file appear.
    println!("cargo::rerun-if-changed={}", build.display());
    let text = match std::fs::read_to_string(&key) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", key.display())),
    };
    // minisign's public key file: an untrusted comment, then the key in base64.
    let line = text.lines().nth(1).unwrap_or_default().trim();
    minisign_verify::PublicKey::from_base64(line)
        .map_err(|_| format!("{} is not a minisign public key", key.display()))?;
    println!("cargo::rustc-env=NONSTOPVIBIN_UPDATE_KEY={line}");
    Ok(())
}

fn updates_enabled(package: &Path) -> Result<(), String> {
    println!("cargo::rerun-if-changed={}", package.display());
    println!("cargo::rustc-check-cfg=cfg(updates_enabled)");
    let text = std::fs::read_to_string(package)
        .map_err(|error| format!("{}: {error}", package.display()))?;
    let manifest: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", package.display()))?;
    match manifest.get("updatesEnabled") {
        Some(serde_json::Value::Bool(true)) => println!("cargo::rustc-cfg=updates_enabled"),
        Some(serde_json::Value::Bool(false)) => {}
        _ => {
            return Err(format!(
                "{}: updatesEnabled must be true or false",
                package.display()
            ));
        }
    }
    Ok(())
}

fn test_feed() -> Result<(), String> {
    const FEED: &str = "NONSTOPVIBIN_TEST_UPDATE_FEED";
    println!("cargo::rerun-if-env-changed={FEED}");
    let Some(feed) = std::env::var_os(FEED) else {
        return Ok(());
    };
    let port: u16 = feed
        .to_str()
        .and_then(|feed| feed.strip_prefix("http://127.0.0.1:"))
        .and_then(|rest| rest.strip_suffix('/'))
        .and_then(|port| port.parse().ok())
        .ok_or_else(|| format!("{FEED} is not http://127.0.0.1:<port>/"))?;
    println!("cargo::rustc-env=NONSTOPVIBIN_TEST_UPDATE_PORT={port}");
    Ok(())
}
