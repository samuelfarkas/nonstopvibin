//! Where this build is installed, detected once at launch: the executable, and the package or
//! source checkout around it. The shell derives its helper, its login item and its update target
//! from it, and the daemon its core and interface.

use std::io;
use std::path::{Path, PathBuf};

/// A macOS app bundle's resources, relative to the bundle.
pub(crate) const BUNDLE_RESOURCES: &str = "Contents/Resources";

const HELPER: &str = "nonstopvibin-window";

#[derive(Debug, Clone)]
pub struct Install {
    /// This executable with symlinks resolved, as `~/.local/bin` may link into a package;
    /// `None` when the system cannot say.
    pub executable: Option<PathBuf>,
    /// The package's files, or the source checkout this was built from.
    pub resources: PathBuf,
    /// An installed build: `resources` holds the package's interface or its bundled core.
    pub packaged: bool,
    /// Linux's `$APPIMAGE`, the image file the `AppImage` runtime mounted this executable from;
    /// the mount moves with every launch.
    pub app_image: Option<PathBuf>,
    /// The macOS app bundle the executable runs from.
    pub bundle: Option<PathBuf>,
}

impl Install {
    /// `interface` and `bundled_core` are where a package keeps its built interface and its
    /// core, relative to its resources. Either marks a package, so one that lost its core still
    /// reports that core missing rather than running as a source checkout.
    #[must_use]
    pub fn detect(interface: &Path, bundled_core: &Path) -> Self {
        let executable = std::env::current_exe().and_then(std::fs::canonicalize).ok();
        let directory = executable.as_deref().and_then(Path::parent);
        // macOS runs `<bundle>/Contents/MacOS/<executable>`, whatever the bundle is named.
        let macos_bundle = directory
            .and_then(|directory| directory.parent()?.parent())
            .filter(|_| cfg!(target_os = "macos"));
        let package = match macos_bundle {
            Some(bundle) => Some(bundle.join(BUNDLE_RESOURCES)),
            None => directory.map(Path::to_path_buf),
        };
        // The updater replaces only an app bundle.
        let bundle = macos_bundle
            .filter(|bundle| {
                bundle
                    .extension()
                    .is_some_and(|extension| extension == "app")
            })
            .map(Path::to_path_buf);
        let app_image = app_image(
            std::env::var_os("APPIMAGE").map(PathBuf::from),
            std::env::var_os("APPDIR").and_then(|mount| std::fs::canonicalize(mount).ok()),
            executable.as_deref(),
        );
        let (resources, packaged) = match package {
            Some(resources)
                if resources.join(interface).is_dir() || resources.join(bundled_core).is_file() =>
            {
                (resources, true)
            }
            _ => {
                let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
                (std::fs::canonicalize(&checkout).unwrap_or(checkout), false)
            }
        };
        Self {
            executable,
            resources,
            packaged,
            app_image,
            bundle,
        }
    }

    /// The window helper beside the executable: the same folder in a package, a checkout or an
    /// `AppImage`, `Contents/MacOS` in the app bundle.
    ///
    /// # Errors
    /// When the executable's path is unknown.
    pub(crate) fn helper(&self) -> io::Result<PathBuf> {
        let directory = self
            .executable
            .as_deref()
            .and_then(Path::parent)
            .ok_or_else(|| io::Error::other("the executable's folder is unknown"))?;
        Ok(directory.join(HELPER))
    }
}

/// `$APPIMAGE`, when this executable runs from that image's mount `$APPDIR`. A build started by
/// another `AppImage` inherits both, and must not update or autostart that image.
fn app_image(
    image: Option<PathBuf>,
    mount: Option<PathBuf>,
    executable: Option<&Path>,
) -> Option<PathBuf> {
    let (image, mount, executable) = (image?, mount?, executable?);
    (image.is_absolute() && executable.starts_with(&mount)).then_some(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_appimage_is_this_build_only_when_it_runs_from_that_image() {
        let image = Some(PathBuf::from("/home/me/Apps/NonstopVibin.AppImage"));
        let mount = Some(PathBuf::from("/tmp/.mount_NonstoAbc123"));
        let inside = Path::new("/tmp/.mount_NonstoAbc123/nonstopvibin");
        assert_eq!(app_image(image.clone(), mount.clone(), Some(inside)), image);
        // A deb build started from another AppImage inherits that image's variables.
        let installed = Path::new("/opt/NonstopVibin/nonstopvibin");
        assert_eq!(app_image(image.clone(), mount, Some(installed)), None);
        assert_eq!(app_image(image, None, Some(inside)), None);
        let relative = Some(PathBuf::from("NonstopVibin.AppImage"));
        let mount = Some(PathBuf::from("/tmp/.mount_NonstoAbc123"));
        assert_eq!(app_image(relative, mount, Some(inside)), None);
    }
}
