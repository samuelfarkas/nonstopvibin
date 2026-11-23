//! The build scripts' core artifact tool, over [`nsv_core::binary`] and [`nsv_core::install`]:
//! `cargo run -p nsv-core --example core-tool -- <command>`. Dev-only, never shipped.
//! `bun run core:install`, `bun run core:pin`, the packagers and `core-pin.yml` call it.
//!
//! - `install [--target <platform>_<arch>] [directory]`: installs the bundled pin's core with
//!   its manifest and licence into `directory`, this checkout's `.vendor/core` by default. What
//!   was there is replaced only by a core that passed the pin.
//! - `check [--target <platform>_<arch>] [--signed] <directory>`: fails unless the core in
//!   `directory` may ship; `--signed` holds the binary to its manifest, not the pin.
//! - `rehash <directory>`: records the binary's hash in its manifest, after signing.
//! - `layout`: the core's file names and the bundled version, as JSON.
//! - `pin [version]`: writes `core-release.json` for an upstream release, the latest by default.
//! - `pending`: prints `<version> <published> <pinned> <releases>` when upstream's latest
//!   release is newer than the pin, and nothing otherwise.

use std::collections::HashMap;
use std::error::Error;
use std::fmt::Display;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use std::{env, fs};

use nsv_core::binary::{
    BINARY, Binary, CHECKOUT_DIRECTORY, CoreManifest, CorePin, CoreTarget, LICENSE, MANIFEST,
    PACKAGED_DIRECTORY, Sha256Digest, Targets, bundled_pin, check_core_directory,
};
use nsv_core::install::{CORE_ARCHIVES, LATEST_RELEASE, RELEASES, download, extract, install_core};
use nsv_domain::version::Version;
use nsv_net::{Get, Https, Redirects};
use serde::Deserialize;

type Failure = Box<dyn Error>;

/// The pin file this tool rewrites; the build bundles it.
const PIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/core-release.json");
const CHECKOUT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    match run(&arguments).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Nothing is left to report a failed write to.
            let _ = writeln!(io::stderr(), "{error}");
            ExitCode::FAILURE
        }
    }
}

fn say(line: impl Display) -> Result<(), Failure> {
    Ok(writeln!(io::stdout(), "{line}")?)
}

async fn run(arguments: &[String]) -> Result<(), Failure> {
    let (command, rest) = arguments.split_first().ok_or(USAGE)?;
    let options = Options::parse(rest)?;
    match command.as_str() {
        "install" => {
            let directory = options
                .directory()
                .unwrap_or_else(|| Path::new(CHECKOUT).join(CHECKOUT_DIRECTORY));
            install(&Https::new()?, options.target()?, &directory).await
        }
        "check" => {
            let binary = if options.signed {
                Binary::Signed
            } else {
                Binary::Pinned
            };
            let directory = options.directory().ok_or(USAGE)?;
            Ok(check_core_directory(
                bundled_pin()?,
                &directory,
                options.target()?,
                binary,
            )?)
        }
        "rehash" => {
            let directory = options.directory().ok_or(USAGE)?;
            let mut manifest = CoreManifest::read(&directory)?;
            let hash = Sha256Digest::of_file(&directory.join(BINARY))?;
            manifest.binary_sha256 = Some(hash.to_string());
            Ok(manifest.write(&directory)?)
        }
        "layout" => say(serde_json::json!({
            "checkout": CHECKOUT_DIRECTORY,
            "packaged": PACKAGED_DIRECTORY,
            "binary": BINARY,
            "manifest": MANIFEST,
            "license": LICENSE,
            "version": bundled_pin()?.version,
        })),
        "pin" => {
            let https = Https::new()?;
            let version = match &options.operand {
                Some(text) => Version::parse(text)
                    .ok_or_else(|| format!("Not a CLIProxyAPI version: {text}"))?,
                None => latest(&https).await?.0,
            };
            pin(&https, version).await
        }
        "pending" => pending(&Https::new()?).await,
        _ => Err(USAGE.into()),
    }
}

const USAGE: &str = "Usage: core-tool install|check|rehash|layout|pin|pending (see crates/core/examples/core-tool.rs)";

#[derive(Default)]
struct Options {
    target: Option<CoreTarget>,
    signed: bool,
    /// The directory, or the version to pin.
    operand: Option<String>,
}

impl Options {
    fn parse(arguments: &[String]) -> Result<Self, Failure> {
        let mut options = Self::default();
        let mut arguments = arguments.iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--target" => {
                    let key = arguments.next().ok_or(USAGE)?;
                    options.target = Some(CoreTarget::from_key(key).ok_or_else(|| {
                        format!("Supported core targets: darwin_arm64, darwin_x64, linux_arm64, linux_x64; not {key}.")
                    })?);
                }
                "--signed" => options.signed = true,
                operand => options.operand = Some(operand.to_owned()),
            }
        }
        Ok(options)
    }

    fn directory(&self) -> Option<PathBuf> {
        self.operand.as_ref().map(PathBuf::from)
    }

    fn target(&self) -> Result<CoreTarget, Failure> {
        Ok(self.target.map_or_else(CoreTarget::current, Ok)?)
    }
}

/// Installs beside `directory` and then swaps it in, so a failed install leaves the old core.
async fn install(https: &Https, target: CoreTarget, directory: &Path) -> Result<(), Failure> {
    let pin = bundled_pin()?;
    let parent = directory
        .parent()
        .ok_or("The core directory has no parent.")?;
    let name = directory
        .file_name()
        .ok_or("The core directory has no name.")?;
    fs::create_dir_all(parent)?;
    let staging = nsv_files::unique_directory(parent, &format!("{}-", name.to_string_lossy()))?;
    let installed = async {
        install_core(https, CORE_ARCHIVES, pin, target, &staging, true).await?;
        remove_directory(directory)?;
        fs::rename(&staging, directory)?;
        Ok::<_, Failure>(())
    }
    .await;
    remove_directory(&staging)?;
    installed?;
    say(format_args!(
        "Verified CLIProxyAPI {} ({}/{}).",
        pin.version,
        target.platform(),
        target.arch()
    ))
}

fn remove_directory(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// The pin file as it is now, checked by the daemon's own schema.
fn current_pin() -> Result<CorePin, Failure> {
    Ok(serde_json::from_str(&fs::read_to_string(PIN)?)?)
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    published_at: String,
}

/// Upstream's latest stable release and when it was published.
async fn latest(https: &Https) -> Result<(Version, String), Failure> {
    let authorization = env::var("GH_TOKEN")
        .ok()
        .map(|token| format!("Bearer {token}"));
    let mut headers = vec![("Accept", "application/vnd.github+json")];
    if let Some(value) = &authorization {
        headers.push(("Authorization", value));
    }
    let response = https
        .get(&Get {
            url: LATEST_RELEASE,
            headers: &headers,
            timeout: Duration::from_secs(30),
            limit: 1 << 20,
            redirects: Redirects::Refuse,
        })
        .await?;
    if !response.ok() {
        return Err(format!("No latest release tag (HTTP {}).", response.status.as_u16()).into());
    }
    let release: Release = serde_json::from_slice(&response.body)?;
    let version = release
        .tag_name
        .strip_prefix('v')
        .and_then(Version::parse)
        .ok_or_else(|| format!("Unexpected latest CLIProxyAPI tag: {}", release.tag_name))?;
    Ok((version, release.published_at))
}

async fn pending(https: &Https) -> Result<(), Failure> {
    let pinned = current_pin()?.version;
    let (version, published) = latest(https).await?;
    if version <= pinned {
        writeln!(
            io::stderr(),
            "CLIProxyAPI {pinned} is pinned; upstream's latest is {version}."
        )?;
        return Ok(());
    }
    say(format_args!("{version} {published} {pinned} {RELEASES}"))
}

/// Hashes each target's archive and executable of upstream's `version` into the pin file,
/// keeping its `minApp`. Each archive must match the release's own `checksums.txt`.
async fn pin(https: &Https, version: Version) -> Result<(), Failure> {
    let temporary = nsv_files::unique_directory(&env::temp_dir(), "nonstopvibin-pin-")?;
    let hashed = hash_release(https, version, &temporary).await;
    remove_directory(&temporary)?;
    let (checksums, binaries) = hashed?;
    let min_app = current_pin()?.min_app;
    let pin = CorePin {
        version,
        min_app,
        checksums,
        binaries,
    };
    fs::write(PIN, serde_json::to_string_pretty(&pin)? + "\n")?;
    let since = min_app.map_or_else(String::new, |app| {
        format!(" for NonstopVibin {app} or later")
    });
    say(format_args!(
        "Pinned CLIProxyAPI {version}{since}. Review crates/core/core-release.json."
    ))
}

async fn hash_release(
    https: &Https,
    version: Version,
    temporary: &Path,
) -> Result<(Targets, Targets), Failure> {
    let origin = format!("{CORE_ARCHIVES}/v{version}");
    let listing = temporary.join("checksums.txt");
    download(https, &format!("{origin}/checksums.txt"), &listing).await?;
    let text = fs::read_to_string(&listing)?;
    // `<sha256>  <archive>` per line.
    let published: HashMap<&str, &str> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            match (fields.next(), fields.next(), fields.next()) {
                (Some(sha256), Some(name), None) => Some((name, sha256)),
                _ => None,
            }
        })
        .collect();
    let mut checksums = [Sha256Digest::from([0; 32]); 4];
    let mut binaries = checksums;
    for ((target, checksum), binary_sha256) in CoreTarget::ALL
        .into_iter()
        .zip(&mut checksums)
        .zip(&mut binaries)
    {
        let key = format!("{}_{}", target.platform(), target.arch());
        let name = target.archive(&version);
        let expected = published.get(name.as_str()).ok_or_else(|| {
            format!("CLIProxyAPI {version}: release still publishing or missing {key}.")
        })?;
        let archive = temporary.join(&name);
        let binary = temporary.join(format!("{key}-{BINARY}"));
        let sha256 = download(https, &format!("{origin}/{name}"), &archive).await?;
        if sha256.to_string() != *expected {
            return Err(format!("{name} does not match the release's checksums.txt.").into());
        }
        *checksum = sha256;
        *binary_sha256 = extract(&archive, BINARY, &binary, 0o600).await?;
        fs::remove_file(&archive)?;
        fs::remove_file(&binary)?;
        say(format_args!("Hashed {name}."))?;
    }
    Ok((Targets::new(checksums), Targets::new(binaries)))
}
