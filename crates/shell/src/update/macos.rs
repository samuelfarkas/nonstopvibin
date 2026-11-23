//! The app bundle, as Squirrel.Mac replaced it: the update zip is unpacked with `ditto` beside
//! the bundle, must satisfy the running bundle's designated requirement and carry a core that
//! matches its manifest, and is then swapped in at the bundle's path.

use std::path::{Path, PathBuf};

use nsv_core::binary::{BINARY, PACKAGED_DIRECTORY, verify_core};
use rustix::fs::Access;
use rustix::io::Errno;
use tokio::process::Command;

use super::Detected;
use crate::log;
use crate::platform::UpdateTarget;
/// The bundle the release zip holds at its root.
const BUNDLE: &str = "NonstopVibin.app";

#[derive(Debug, Clone)]
pub(super) struct Target {
    bundle: PathBuf,
    /// `Contents/MacOS/NonstopVibin` inside it, which the swap leaves at the same path.
    executable: PathBuf,
}

impl UpdateTarget for Target {
    const FEED: &'static str = "latest-mac.yml";
    const PAYLOAD: &'static str = ".zip";

    fn detect() -> Detected<Self> {
        let Some((executable, bundle)) = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .ok()
            .and_then(|executable| {
                let bundle = executable.parent()?.parent()?.parent()?.to_owned();
                Some((executable, bundle))
            })
            .filter(|(_, bundle)| {
                bundle
                    .extension()
                    .is_some_and(|extension| extension == "app")
            })
        else {
            return Detected::None;
        };
        let replaceable = bundle
            .parent()
            .is_some_and(|folder| writable(&bundle) && writable(folder));
        if replaceable {
            Detected::Target(Self { bundle, executable })
        } else {
            Detected::NotReplaceable
        }
    }

    fn path(&self) -> &Path {
        &self.bundle
    }

    fn executable(&self) -> &Path {
        &self.executable
    }

    /// Unpacks the zip in `staging` and checks the bundle in it.
    async fn prepare(&self, staging: &Path, payload: &Path) -> Result<PathBuf, String> {
        let unpacked = staging.join("unpacked");
        run(
            Command::new("/usr/bin/ditto")
                .arg("-x")
                .arg("-k")
                .arg(payload)
                .arg(&unpacked),
            "unpack",
        )
        .await?;
        drop(tokio::fs::remove_file(payload).await);
        let app = unpacked.join(BUNDLE);
        let mut entries = std::fs::read_dir(&unpacked).map_err(|error| error.to_string())?;
        let only = entries
            .next()
            .and_then(Result::ok)
            .map(|entry| entry.path())
            == Some(app.clone())
            && entries.next().is_none();
        if !only {
            return Err("the update zip does not hold the app bundle alone".to_owned());
        }
        let requirement = designated(&self.bundle).await?;
        run(
            Command::new("/usr/bin/codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(format!("-R={requirement}"))
                .arg(&app),
            "verify the signature of",
        )
        .await?;
        let core = app
            .join("Contents/Resources")
            .join(PACKAGED_DIRECTORY)
            .join(BINARY);
        tokio::task::spawn_blocking(move || verify_core(&core, None))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|_| "the update's core does not match its manifest".to_owned())?;
        Ok(app)
    }
}

/// Squirrel.Mac's rule: an update must satisfy the running bundle's designated requirement,
/// which scripts/package-mac.ts sets to `identifier "app.nonstopvibin.desktop"` for ad-hoc
/// builds and codesign derives from the certificate for a Developer ID.
async fn designated(bundle: &Path) -> Result<String, String> {
    let output = Command::new("/usr/bin/codesign")
        .args(["--display", "-r-"])
        .arg(bundle)
        .output()
        .await
        .map_err(|error| format!("could not run codesign: {error}"))?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("designated => "))
        .filter(|_| output.status.success())
        .map(str::to_owned)
        .ok_or_else(|| "the running app has no designated requirement".to_owned())
}

async fn run(command: &mut Command, what: &str) -> Result<(), String> {
    let status = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|error| format!("could not {what} the update: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("could not {what} the update ({status})"))
    }
}

/// `replaceableBundle` in desktop/updates.ts: a disk image, App Translocation or a folder the
/// user cannot change refuses.
fn writable(path: &Path) -> bool {
    match rustix::fs::access(path, Access::WRITE_OK) {
        Ok(()) => true,
        Err(Errno::ACCESS | Errno::PERM | Errno::ROFS) => false,
        Err(error) => {
            log(format_args!(
                "could not check whether {} is writable: {error}",
                path.display()
            ));
            false
        }
    }
}
