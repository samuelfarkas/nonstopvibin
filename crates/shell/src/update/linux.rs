//! The `AppImage`: the image file is the whole app, so the update is the new image, made
//! executable and swapped in at `$APPIMAGE`. A deb is not updated here (`APPIMAGE` is unset).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::Detected;

pub(super) const FEED: &str = "latest-linux.yml";
pub(super) const PAYLOAD: &str = ".AppImage";

#[derive(Debug, Clone)]
pub(super) struct Target {
    /// `$APPIMAGE`, which the `AppImage` runtime sets to the image's path.
    image: PathBuf,
}

impl Target {
    pub(super) fn detect() -> Detected<Self> {
        match std::env::var_os("APPIMAGE").map(PathBuf::from) {
            Some(image) if image.is_absolute() => Detected::Target(Self { image }),
            _ => Detected::None,
        }
    }

    /// What the swap replaces.
    pub(super) fn path(&self) -> &Path {
        &self.image
    }

    /// What starts the updated app.
    pub(super) fn executable(&self) -> &Path {
        &self.image
    }

    /// The downloaded image, verified by its hashes, needs only its mode.
    /// Ready at once; the macOS twin unpacks and checks the bundle.
    #[expect(
        clippy::unused_self,
        reason = "the macOS twin checks against its bundle"
    )]
    pub(super) fn prepare(
        &self,
        _staging: &Path,
        payload: &Path,
    ) -> std::future::Ready<Result<PathBuf, String>> {
        let made = std::fs::set_permissions(payload, std::fs::Permissions::from_mode(0o755))
            .map(|()| payload.to_owned())
            .map_err(|error| format!("could not make the update executable: {error}"));
        std::future::ready(made)
    }
}
