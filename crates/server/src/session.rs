//! `server.json`: how `url`, `status` and a second desktop launch find the running daemon. It
//! is written atomically and owner-only once the daemon has started, read only when it is an
//! owner-only ordinary file, and removed at shutdown only while it still names this process.

use std::io;
use std::path::Path;
use std::time::Duration;

use nsv_domain::secret::SessionToken;
use nsv_net::{Loopback, LoopbackRequest, Method, loopback};
use nsv_store::files::{read_own_file, write_atomic};
use nsv_store::layout::session_path;
use serde::Deserialize;
use serde_json::json;

use crate::http::valid_allow_host;

/// What `server.json` holds.
#[derive(Debug, Clone)]
pub struct Session {
    pub pid: u32,
    pub port: u16,
    pub token: SessionToken,
    pub allow_hosts: Vec<String>,
    pub version: String,
    pub started_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("server.json must be an owner-only ordinary file you own (chmod 600).")]
    Unsafe,
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    pid: u32,
    port: u16,
    token: String,
    allow_hosts: Vec<String>,
    version: String,
    started_at: String,
}

impl Session {
    /// `http://127.0.0.1:<port>/#session=<token>`: the fragment never reaches the server.
    #[must_use]
    pub fn url(&self) -> String {
        format!(
            "http://127.0.0.1:{}/#session={}",
            self.port,
            self.token.expose_secret()
        )
    }

    /// The same link on each allowed HTTPS host.
    #[must_use]
    pub fn https_urls(&self) -> Vec<String> {
        self.allow_hosts
            .iter()
            .map(|host| format!("https://{host}/#session={}", self.token.expose_secret()))
            .collect()
    }

    /// Reads the data directory's `server.json`; a missing, malformed or invalid one is `None`.
    ///
    /// # Errors
    /// [`SessionError::Unsafe`] when it is not an owner-only ordinary file of this user.
    pub fn read(directory: &Path) -> Result<Option<Self>, SessionError> {
        let Some(text) = read_own_file(&session_path(directory), 0o077, 65_536, || {
            SessionError::Unsafe
        })?
        else {
            return Ok(None);
        };
        let Ok(stored) = serde_json::from_str::<Stored>(&text) else {
            return Ok(None);
        };
        let valid = stored.pid > 0
            && stored.port > 0
            && !stored.token.is_empty()
            && stored.allow_hosts.iter().all(|host| valid_allow_host(host));
        Ok(valid.then(|| Self {
            pid: stored.pid,
            port: stored.port,
            token: SessionToken::new(stored.token),
            allow_hosts: stored.allow_hosts,
            version: stored.version,
            started_at: stored.started_at,
        }))
    }

    /// Writes it atomically with mode 0600.
    ///
    /// # Errors
    /// A file-system error.
    pub fn write(&self, directory: &Path) -> io::Result<()> {
        // JSON.stringify's key order, which earlier CLIs read.
        let text = json!({
            "pid": self.pid,
            "port": self.port,
            "token": self.token.expose_secret(),
            "allowHosts": self.allow_hosts,
            "version": self.version,
            "startedAt": self.started_at,
        })
        .to_string();
        write_atomic(&session_path(directory), text.as_bytes(), 0o600)
    }

    /// Removes `server.json` if it still names `pid`: a newer instance may have replaced it.
    ///
    /// # Errors
    /// An unsafe file, or a file-system error.
    pub fn remove(directory: &Path, pid: u32) -> Result<(), SessionError> {
        if Self::read(directory)?.is_some_and(|session| session.pid == pid) {
            match std::fs::remove_file(session_path(directory)) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
                _ => {}
            }
        }
        Ok(())
    }

    /// Authenticated liveness: the daemon answers `GET /api/state` with this token within two
    /// seconds. A stale file or a recycled PID cannot pass.
    pub async fn probe(&self) -> Option<ServerState> {
        let authorization = format!("Bearer {}", self.token.expose_secret());
        let response = loopback(LoopbackRequest {
            to: Loopback::v4(self.port),
            method: &Method::GET,
            target: "/api/state",
            headers: &[("Authorization", &authorization)],
            body: None,
            timeout: Duration::from_secs(2),
            limit: 16 << 20,
        })
        .await
        .ok()?;
        if !response.ok() {
            return None;
        }
        serde_json::from_slice(&response.body).ok()
    }
}

/// What a CLI of any version reads from a running daemon's `GET /api/state`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerState {
    pub version: String,
    pub core_version: String,
}
