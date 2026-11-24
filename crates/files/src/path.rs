use std::path::{Component, Path, PathBuf};

/// Node's `path.resolve(base, path)` for an absolute `base`: `path` joined to it (an absolute
/// `path` replaces it), with `.` dropped and each `..` removing the name before it, without
/// touching the file system. `..` at the root stays at the root.
#[must_use]
pub fn resolve(base: &Path, path: impl AsRef<Path>) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in base.join(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}
