//! `server.json`: how `url`, `status` and a second desktop launch find the running daemon. It
//! is written atomically and owner-only once the daemon has started, read only when it is an
//! owner-only ordinary file, removed when a daemon takes the data directory's lock, and removed
//! at shutdown only while it still names this process.

use std::io;
use std::time::{Duration, SystemTime};

use hyper::StatusCode;
use nsv_domain::endpoint::{loopback_origin, session_link};
use nsv_domain::secret::SessionToken;
use nsv_domain::time::{epoch_ms, iso_string, parse_iso};
use nsv_files::{DataDir, read_own_file, write_atomic_unsynced};
use nsv_net::{Loopback, LoopbackRequest, Method, loopback};
use serde::Deserialize;
use serde_json::json;

use crate::checks::{AllowedHost, DEV_HOST};

#[derive(Debug, Clone)]
pub struct Session {
    pub pid: u32,
    pub port: u16,
    pub token: SessionToken,
    pub allow_hosts: Vec<AllowedHost>,
    pub version: String,
    /// When the daemon started, in milliseconds since the epoch.
    pub started_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("server.json must be an owner-only ordinary file you own (chmod 600).")]
    Unsafe,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Why a second desktop launch could not bring the running daemon's window forward.
#[derive(Debug, thiserror::Error)]
pub enum ShowError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("NonstopVibin is already running but has not said where. Try again in a moment.")]
    NoSession,
    #[error("NonstopVibin is already running but did not answer: {0}")]
    NoAnswer(String),
}

/// How long a second launch waits for a daemon that holds the lock but is still starting: its
/// restore starts each kept-running profile's core, which may take up to 15 s to answer.
const STARTING: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(100);

/// The largest answer the daemon reads from a loopback port: its state, or a profile's models.
pub(crate) const LOOPBACK_LIMIT: u64 = 16 << 20;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    pid: u32,
    port: u16,
    token: String,
    allow_hosts: Vec<AllowedHost>,
    version: String,
    started_at: String,
}

impl Session {
    /// `http://127.0.0.1:<port>/#session=<token>`: the fragment never reaches the server.
    #[must_use]
    pub fn url(&self) -> String {
        session_link(&loopback_origin(self.port), &self.token)
    }

    /// The same link on the Vite development server, which serves a development run's
    /// interface.
    #[must_use]
    pub fn development_url(&self) -> String {
        session_link(&format!("http://{DEV_HOST}"), &self.token)
    }

    /// When the daemon started, as `Date.toISOString()` writes it.
    #[must_use]
    pub fn started(&self) -> String {
        iso_string(self.started_at)
    }

    /// Whether the daemon started at or after `time`.
    #[must_use]
    pub fn started_since(&self, time: SystemTime) -> bool {
        self.started_at >= epoch_ms(time)
    }

    /// The same link on each allowed HTTPS host.
    #[must_use]
    pub fn https_urls(&self) -> Vec<String> {
        self.allow_hosts
            .iter()
            .map(|host| session_link(&format!("https://{host}"), &self.token))
            .collect()
    }

    /// Reads the data directory's `server.json`; a missing, malformed or invalid one is `None`.
    ///
    /// # Errors
    /// [`SessionError::Unsafe`] when it is not an owner-only ordinary file of this user.
    pub fn read(directory: &DataDir) -> Result<Option<Self>, SessionError> {
        let Some(text) =
            read_own_file(&directory.session(), 0o077, 65_536, || SessionError::Unsafe)?
        else {
            return Ok(None);
        };
        let Ok(stored) = serde_json::from_str::<Stored>(&text) else {
            return Ok(None);
        };
        let Some(started_at) = parse_iso(&stored.started_at) else {
            return Ok(None);
        };
        let valid = stored.pid > 0 && stored.port > 0 && !stored.token.is_empty();
        Ok(valid.then(|| Self {
            pid: stored.pid,
            port: stored.port,
            token: SessionToken::new(stored.token),
            allow_hosts: stored.allow_hosts,
            version: stored.version,
            started_at,
        }))
    }

    /// # Errors
    /// A file-system error.
    pub fn write(&self, directory: &DataDir) -> io::Result<()> {
        // JSON.stringify's key order, which earlier CLIs read.
        let text = json!({
            "pid": self.pid,
            "port": self.port,
            "token": self.token.expose_secret(),
            "allowHosts": self.allow_hosts,
            "version": self.version,
            "startedAt": iso_string(self.started_at),
        })
        .to_string();
        write_atomic_unsynced(&directory.session(), text.as_bytes(), 0o600)
    }

    /// Removes `server.json` if it still names `pid`: a newer instance may have replaced it.
    ///
    /// # Errors
    /// An unsafe file, or a file-system error.
    pub fn remove(directory: &DataDir, pid: u32) -> Result<(), SessionError> {
        if Self::read(directory)?.is_some_and(|session| session.pid == pid) {
            Self::discard(directory)?;
        }
        Ok(())
    }

    /// Removes `server.json`, whichever process it names. Only the holder of the data
    /// directory's lock may: the lock proves an earlier run's file stale.
    ///
    /// # Errors
    /// A file-system error.
    pub(crate) fn discard(directory: &DataDir) -> io::Result<()> {
        match std::fs::remove_file(directory.session()) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// The second desktop launch: the daemon holding `directory`'s lock shows its main window on
    /// `POST /api/desktop/show` with its token. Waits up to half a minute for a daemon that is
    /// still starting: until `server.json` exists, names a port that answers, and holds the
    /// daemon's token rather than an earlier run's.
    ///
    /// # Errors
    /// An unsafe `server.json`, none in time, an answer other than 2xx or 401, or no 2xx answer
    /// in time.
    pub async fn show_main(directory: &DataDir) -> Result<(), ShowError> {
        let deadline = tokio::time::Instant::now() + STARTING;
        loop {
            let pending = match Self::read(directory)? {
                None => ShowError::NoSession,
                Some(session) => {
                    let authorization = format!("Bearer {}", session.token.expose_secret());
                    let answer = loopback(LoopbackRequest {
                        to: Loopback::v4(session.port),
                        method: &Method::POST,
                        target: "/api/desktop/show",
                        headers: &[("Authorization", &authorization)],
                        body: None,
                        timeout: Duration::from_secs(2),
                        limit: 64 * 1024,
                    })
                    .await;
                    let refused = |status: StatusCode| {
                        ShowError::NoAnswer(format!("it answered HTTP/1.1 {}", status.as_u16()))
                    };
                    match answer {
                        Ok(response) if response.ok() => return Ok(()),
                        Ok(response) if response.status == StatusCode::UNAUTHORIZED => {
                            refused(response.status)
                        }
                        Ok(response) => return Err(refused(response.status)),
                        Err(error) => ShowError::NoAnswer(error.to_string()),
                    }
                }
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(pending);
            }
            tokio::time::sleep(POLL).await;
        }
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
            limit: LOOPBACK_LIMIT,
        })
        .await
        .ok()?;
        if !response.ok() {
            return None;
        }
        serde_json::from_slice(&response.body).ok()
    }
}

/// What a CLI of any version reads from a running daemon's `GET /api/state`: only the two
/// fields every version answers, so a daemon of another version still parses, where all of
/// [`nsv_domain::wire::AppState`] would not.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerState {
    pub version: String,
    pub core_version: String,
}
