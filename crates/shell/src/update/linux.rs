//! The `AppImage`: the image file is the whole app, so the update is the new image, made
//! executable and swapped in at `$APPIMAGE`. A deb is not updated here (`APPIMAGE` is unset).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::Detected;
use crate::install::Install;
use crate::platform::UpdateTarget;

#[derive(Debug, Clone)]
pub(super) struct Target {
    /// `$APPIMAGE`, which the `AppImage` runtime sets to the image's path.
    image: PathBuf,
}

impl UpdateTarget for Target {
    const FEED: &'static str = "latest-linux.yml";
    const PAYLOAD: &'static str = ".AppImage";

    fn detect(install: &Install) -> Detected<Self> {
        match &install.app_image {
            Some(image) => Detected::Target(Self {
                image: image.clone(),
            }),
            None => Detected::None,
        }
    }

    fn path(&self) -> &Path {
        &self.image
    }

    fn executable(&self) -> &Path {
        &self.image
    }

    /// The downloaded image, verified by its hashes, needs only its mode.
    fn prepare(
        &self,
        _staging: &Path,
        payload: &Path,
    ) -> impl Future<Output = Result<PathBuf, String>> {
        std::future::ready(
            std::fs::set_permissions(payload, std::fs::Permissions::from_mode(0o755))
                .map(|()| payload.to_owned())
                .map_err(|error| format!("could not make the update executable: {error}")),
        )
    }
}
