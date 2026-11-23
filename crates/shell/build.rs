//! Embeds the release key, `build/update-signing.pub`, once the owner has committed it
//! (docs/release-signing.md): from then on the updater accepts only releases whose
//! `SHA256SUMS` that key signed.

use std::path::Path;

fn main() -> Result<(), String> {
    let build = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build");
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
    let valid = line.len() == 56
        && line.starts_with("RW")
        && line
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='));
    if !valid {
        return Err(format!("{} is not a minisign public key", key.display()));
    }
    println!("cargo::rustc-env=NONSTOPVIBIN_UPDATE_KEY={line}");
    Ok(())
}
