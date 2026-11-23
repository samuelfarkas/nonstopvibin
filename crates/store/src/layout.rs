//! The layout of the data directory. Installed apps already hold data under these exact names:
//! a rename here strands the owner's profiles, keys and credentials.

use std::env;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Component, Path, PathBuf};

use nsv_domain::id::ProfileId;

/// The data directory: `NONSTOPVIBIN_DATA_DIR`, or `~/.nonstopvibin` when it is unset or empty.
///
/// # Errors
/// When the variable holds a relative path and the working directory is unavailable, or when
/// there is no home directory.
pub fn data_directory() -> io::Result<PathBuf> {
    data_directory_from(env::var_os("NONSTOPVIBIN_DATA_DIR").as_deref())
}

/// [`data_directory`] with the override given: an empty or absent one means `~/.nonstopvibin`.
/// A relative override resolves against the working directory, lexically like Node's
/// `path.resolve`.
///
/// # Errors
/// As [`data_directory`].
pub fn data_directory_from(override_path: Option<&OsStr>) -> io::Result<PathBuf> {
    let chosen = match override_path {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => env::home_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?
            .join(".nonstopvibin"),
    };
    let absolute = if chosen.is_absolute() {
        chosen
    } else {
        env::current_dir()?.join(chosen)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    Ok(resolved)
}

#[must_use]
pub fn database_path(directory: &Path) -> PathBuf {
    directory.join("nonstopvibin.sqlite")
}

#[must_use]
pub fn vault_key_path(directory: &Path) -> PathBuf {
    directory.join("vault.key")
}

#[must_use]
pub fn profile_directory(directory: &Path, profile: &ProfileId) -> PathBuf {
    directory.join("profiles").join(profile.as_str())
}

#[must_use]
pub fn credential_directory(directory: &Path, profile: &ProfileId) -> PathBuf {
    profile_directory(directory, profile).join("auth")
}

#[must_use]
pub fn core_config_path(directory: &Path, profile: &ProfileId) -> PathBuf {
    profile_directory(directory, profile).join("config.yaml")
}

/// The core's arguments; reaping orphaned cores recognises them by this command line.
#[must_use]
pub fn core_arguments(directory: &Path, profile: &ProfileId) -> [OsString; 2] {
    [
        OsString::from("--config"),
        core_config_path(directory, profile).into_os_string(),
    ]
}

/// Throwaway data directories for Claude sign-ins awaiting review and core update checks.
#[must_use]
pub fn oauth_pending_directory(directory: &Path) -> PathBuf {
    directory.join("oauth-pending")
}

/// Core updates installed builds downloaded, as `<version>/`.
#[must_use]
pub fn cores_directory(directory: &Path) -> PathBuf {
    directory.join("cores")
}

/// Ledger of running cores, for the next instance to reap after a crash.
#[must_use]
pub fn core_ledger_directory(directory: &Path) -> PathBuf {
    directory.join("core-processes")
}

#[must_use]
pub fn agent_connections_directory(directory: &Path) -> PathBuf {
    directory.join("agent-connections")
}

/// The headless server's discovery file.
#[must_use]
pub fn session_path(directory: &Path) -> PathBuf {
    directory.join("server.json")
}

/// The headless server's single-instance lock.
#[must_use]
pub fn server_lock_path(directory: &Path) -> PathBuf {
    directory.join("server.lock.sqlite")
}
