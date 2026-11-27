//! The data directory and the names inside it. Installed apps already hold data under these
//! exact names: a rename here strands the owner's profiles, keys and credentials.

use std::env;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nsv_domain::id::ProfileId;
use nsv_domain::sha256_hex;

use crate::lock::{LockError, ServerLock};
use crate::path::resolve;

/// A data directory: an absolute path, the root of every file below. Cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir(Arc<Path>);

impl DataDir {
    /// The variable that chooses another data directory, which a service unit carries.
    pub const VARIABLE: &str = "NONSTOPVIBIN_DATA_DIR";

    /// The directory this process's `NONSTOPVIBIN_DATA_DIR` chooses; `None` when it is unset
    /// or empty.
    ///
    /// # Errors
    /// As [`DataDir::chosen`].
    pub fn from_environment() -> io::Result<Option<Self>> {
        Self::chosen(&env::var_os(Self::VARIABLE).unwrap_or_default())
    }

    /// The directory a value of `NONSTOPVIBIN_DATA_DIR` chooses; `None` for an empty one. A
    /// relative path resolves against the working directory, lexically like Node's
    /// `path.resolve`.
    ///
    /// # Errors
    /// When the path is relative and the working directory is unavailable.
    pub fn chosen(value: &OsStr) -> io::Result<Option<Self>> {
        if value.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self::new(absolute(Path::new(value))?)))
    }

    /// `~/.nonstopvibin`, where the data lives unless a variable chooses another directory,
    /// resolved as [`DataDir::chosen`] resolves a path.
    ///
    /// # Errors
    /// When there is no home directory, or it is relative and the working directory is
    /// unavailable.
    pub fn home() -> io::Result<Self> {
        let home = env::home_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?;
        Ok(Self::new(absolute(&home.join(".nonstopvibin"))?))
    }

    /// A data directory already chosen, such as a throwaway one under
    /// [`DataDir::oauth_pending`]. `path` is absolute.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(Arc::from(path.into()))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Takes the directory's single-instance lock, which a daemon holds while it serves it:
    /// vets and tightens the directory (creating it owner-only), then locks it without
    /// waiting. Blocks on the file system.
    ///
    /// # Errors
    /// [`LockError::Occupied`] when any process holds it, [`LockError::UnsafeDirectory`] or
    /// [`LockError::UnsafeLockFile`] for an entry that is not this user's own, unlinked
    /// ordinary one (its mode is left as it is), or a file-system or database error.
    pub fn lock(&self) -> Result<ServerLock, LockError> {
        ServerLock::acquire(self)
    }

    #[must_use]
    pub fn database(&self) -> PathBuf {
        self.0.join("nonstopvibin.sqlite")
    }

    #[must_use]
    pub fn vault_key(&self) -> PathBuf {
        self.0.join("vault.key")
    }

    /// Each profile's directory, as `<profile id>/`.
    #[must_use]
    pub fn profiles(&self) -> PathBuf {
        self.0.join("profiles")
    }

    #[must_use]
    pub fn profile(&self, profile: &ProfileId) -> PathBuf {
        self.profiles().join(profile.as_str())
    }

    #[must_use]
    pub fn credentials(&self, profile: &ProfileId) -> PathBuf {
        self.profile(profile).join("auth")
    }

    #[must_use]
    pub fn core_config(&self, profile: &ProfileId) -> PathBuf {
        self.profile(profile).join("config.yaml")
    }

    /// Throwaway data directories for Claude sign-ins awaiting review and core update checks.
    #[must_use]
    pub fn oauth_pending(&self) -> PathBuf {
        self.0.join("oauth-pending")
    }

    /// Core updates installed builds downloaded, as `<version>/`.
    #[must_use]
    pub fn cores(&self) -> PathBuf {
        self.0.join("cores")
    }

    /// Ledger of running cores, for the next instance to reap after a crash.
    #[must_use]
    pub fn core_ledger(&self) -> PathBuf {
        self.0.join("core-processes")
    }

    /// Claude Agent's files, below which each profile has its Claude Code configuration
    /// directory, `<profile id>/`.
    #[must_use]
    pub fn claude_agent(&self) -> PathBuf {
        self.0.join("claude-agent")
    }

    /// A profile's Claude Code configuration directory: its sign-in and transcripts.
    #[must_use]
    pub fn claude_agent_config(&self, profile: &ProfileId) -> PathBuf {
        self.claude_agent().join(profile.as_str())
    }

    /// The empty directory a profile's `claude` processes run in.
    #[must_use]
    pub fn claude_agent_work(&self, profile: &ProfileId) -> PathBuf {
        self.claude_agent().join("work").join(profile.as_str())
    }

    /// The files a profile's running `claude` processes read: system prompts and MCP
    /// configurations.
    #[must_use]
    pub fn claude_agent_run(&self, profile: &ProfileId) -> PathBuf {
        self.claude_agent().join("run").join(profile.as_str())
    }

    #[must_use]
    pub fn agent_connections(&self) -> PathBuf {
        self.0.join("agent-connections")
    }

    /// The running daemon's discovery file.
    #[must_use]
    pub fn session(&self) -> PathBuf {
        self.0.join("server.json")
    }

    /// The daemon's single-instance lock; see [`DataDir::lock`].
    #[must_use]
    pub fn server_lock(&self) -> PathBuf {
        self.0.join("server.lock.sqlite")
    }

    /// The Linux window helper's webview storage.
    #[must_use]
    pub fn webview(&self) -> PathBuf {
        self.0.join("webview")
    }

    /// The page that hands the browser the session link when no window helper can start. It
    /// holds the session token, so only the owner may read it.
    #[must_use]
    pub fn browser_page(&self) -> PathBuf {
        self.0.join("browser.html")
    }

    /// `/tmp/nonstopvibin-<uid>-<24 hex of SHA-256 of the data directory>`, the agent
    /// credential socket's directory. A short path stays within macOS's 103-byte socket path
    /// limit, whatever the home or data directory.
    #[must_use]
    pub fn socket_directory(&self) -> PathBuf {
        let mut hash = sha256_hex(self.0.as_os_str().as_bytes());
        hash.truncate(24);
        PathBuf::from(format!(
            "/tmp/nonstopvibin-{}-{hash}",
            rustix::process::getuid().as_raw()
        ))
    }

    #[must_use]
    pub fn credential_socket(&self) -> PathBuf {
        self.socket_directory().join("agents.sock")
    }
}

/// `path` against the working directory, lexically like Node's `path.resolve`.
fn absolute(path: &Path) -> io::Result<PathBuf> {
    Ok(if path.is_absolute() {
        resolve(Path::new("/"), path)
    } else {
        resolve(&env::current_dir()?, path)
    })
}

/// The user's configuration directory: `$XDG_CONFIG_HOME`, or `~/.config`. A relative value is
/// ignored, as the XDG Base Directory specification says.
///
/// # Errors
/// When neither is set.
pub fn config_home() -> io::Result<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::home_dir()
                .filter(|home| home.is_absolute())
                .map(|home| home.join(".config"))
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "neither XDG_CONFIG_HOME nor a home directory is set",
            )
        })
}
