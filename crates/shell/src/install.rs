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
    /// An installed build: `resources` holds the bundled core.
    pub packaged: bool,
    /// Linux's `$APPIMAGE`, the image file the `AppImage` runtime mounted this executable from;
    /// the mount moves with every launch.
    pub app_image: Option<PathBuf>,
    /// The macOS app bundle the executable runs from.
    pub bundle: Option<PathBuf>,
}

impl Install {
    /// `bundled_core` is where a package keeps its core, relative to its resources.
    #[must_use]
    pub fn detect(bundled_core: &Path) -> Self {
        let executable = std::env::current_exe().and_then(std::fs::canonicalize).ok();
        // `<bundle>.app/Contents/MacOS/<executable>`.
        let bundle = executable
            .as_deref()
            .and_then(|executable| executable.parent()?.parent()?.parent())
            .filter(|bundle| {
                cfg!(target_os = "macos")
                    && bundle
                        .extension()
                        .is_some_and(|extension| extension == "app")
            })
            .map(Path::to_path_buf);
        let app_image = std::env::var_os("APPIMAGE")
            .map(PathBuf::from)
            .filter(|image| image.is_absolute());
        let package = bundle.as_ref().map_or_else(
            || {
                executable
                    .as_deref()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
            },
            |bundle| Some(bundle.join(BUNDLE_RESOURCES)),
        );
        let (resources, packaged) = match package {
            Some(resources) if resources.join(bundled_core).is_file() => (resources, true),
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
