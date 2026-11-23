//! The credential socket: an owner-only Unix socket that answers the protocol in `credential.rs`
//! for the helpers and generated code agents run. Each request reads at most a 4 KiB head and
//! lasts at most 5 s; it never offers a management operation.

use std::fs::{self, Permissions};
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nsv_domain::id::{ProfileId, is_uuid};
use nsv_domain::secret::ClientKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};

use crate::credential::{HOST, PROFILES_PATH, SEPARATOR};
use crate::error::{AgentError, conflicting, refused};
use crate::files::remove_file;
use crate::{Connection, ConnectionSource};

/// The largest request head read, as the TypeScript server's `maxHeaderSize`.
const HEAD_LIMIT: usize = 4096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// Pause after a failed accept (for example out of descriptors) so the loop does not spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

const REJECTED: &str = "Agent connection request rejected.";
const STOPPED: &str = "Open nonstopvibin and start this profile.";

/// A running socket server. Dropping it stops accepting; [`Server::close`] also ends open
/// requests and removes the socket file.
pub(crate) struct Server {
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
    path: PathBuf,
}

impl Server {
    pub(crate) async fn close(self) -> io::Result<()> {
        // The accept loop ends when its sender is dropped.
        drop(self.shutdown);
        if let Err(error) = self.task.await
            && error.is_panic()
        {
            std::panic::resume_unwind(error.into_panic());
        }
        // A closed listener no longer answers, so its file is ours to remove, as libuv did.
        remove_file(&self.path)
    }
}

/// Serves the protocol on `path`, replacing a stale socket an earlier run left.
pub(crate) async fn serve(path: &Path, connection: ConnectionSource) -> Result<Server, AgentError> {
    remove_stale_socket(path).await?;
    let listener = UnixListener::bind(path)?;
    if let Err(error) = fs::set_permissions(path, Permissions::from_mode(0o600)) {
        drop(listener);
        remove_file(path)?;
        return Err(error.into());
    }
    let (shutdown, stopped) = oneshot::channel();
    let task = tokio::spawn(accept(listener, connection, stopped));
    Ok(Server {
        shutdown,
        task,
        path: path.to_owned(),
    })
}

/// Removes a socket file only when it is this user's and nothing answers on it.
async fn remove_stale_socket(path: &Path) -> Result<(), AgentError> {
    let status = match fs::symlink_metadata(path) {
        Ok(status) => status,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !status.file_type().is_socket() || status.uid() != rustix::process::getuid().as_raw() {
        return Err(refused(
            "The agent connection path is occupied by an unexpected file.",
        ));
    }
    match tokio::time::timeout(PROBE_TIMEOUT, UnixStream::connect(path)).await {
        Err(_elapsed) => Err(refused("The existing agent connection did not respond.")),
        Ok(Ok(_live)) => Err(conflicting(
            "Another nonstopvibin instance is serving these agent connections.",
        )),
        Ok(Err(error))
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            Ok(remove_file(path)?)
        }
        Ok(Err(error)) => Err(error.into()),
    }
}

async fn accept(
    listener: UnixListener,
    connection: ConnectionSource,
    mut stopped: oneshot::Receiver<()>,
) {
    let mut requests = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut stopped => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    requests.spawn(answer(stream, Arc::clone(&connection)));
                }
                Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
            },
            Some(_) = requests.join_next(), if !requests.is_empty() => {}
        }
    }
    // Dropping `requests` aborts the open ones, as `closeAllConnections` did.
}

/// One request: its reply, or nothing when the client sends no complete head in time or hangs
/// up, which the TypeScript server did not report either.
async fn answer(mut stream: UnixStream, connection: ConnectionSource) {
    let replied = tokio::time::timeout(REQUEST_TIMEOUT, async {
        let reply = match read_head(&mut stream).await? {
            Head::Complete(head) => reply(&head, &connection),
            Head::TooLarge => {
                "HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\n\r\n"
                    .to_owned()
            }
            Head::Closed => return Ok(()),
        };
        stream.write_all(reply.as_bytes()).await?;
        stream.shutdown().await
    })
    .await;
    drop(replied);
}

enum Head {
    Complete(Vec<u8>),
    TooLarge,
    Closed,
}

async fn read_head(stream: &mut UnixStream) -> io::Result<Head> {
    let mut head = Vec::with_capacity(512);
    let mut chunk = [0; 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        let Some(bytes) = chunk.get(..read).filter(|bytes| !bytes.is_empty()) else {
            return Ok(Head::Closed);
        };
        head.extend_from_slice(bytes);
        match head.windows(4).position(|window| window == b"\r\n\r\n") {
            Some(end) if end <= HEAD_LIMIT => {
                head.truncate(end);
                return Ok(Head::Complete(head));
            }
            Some(_) => return Ok(Head::TooLarge),
            None if head.len() > HEAD_LIMIT => return Ok(Head::TooLarge),
            None => {}
        }
    }
}

/// The reply to one request head: the key and port for a running profile, 409 for any other
/// profile, 403 for anything outside the protocol.
fn reply(head: &[u8], connection: &ConnectionSource) -> String {
    let head = String::from_utf8_lossy(head);
    let Some(request) = Request::parse(&head) else {
        return "HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n".to_owned();
    };
    let id = request
        .target
        .strip_prefix(PROFILES_PATH)
        .filter(|id| {
            id.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
        })
        .filter(|id| is_uuid(id))
        .and_then(|id| ProfileId::parse(id).ok());
    let Some(id) = id.filter(|_| {
        request.method == "GET" && request.host == Some(HOST) && request.origin.is_empty()
    }) else {
        return refusal("403 Forbidden", REJECTED);
    };
    match connection(&id) {
        Ok(Connection { key, port })
            if ClientKey::well_formed(key.expose_secret()) && port >= 1 =>
        {
            let body = format!("{}{SEPARATOR}{port}", key.expose_secret());
            format!(
                "HTTP/1.1 200 OK\r\nCache-Control: no-store\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        }
        _ => refusal("409 Conflict", STOPPED),
    }
}

fn refusal(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nCache-Control: no-store\r\nContent-Type: text/plain\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
        body.len()
    )
}

struct Request<'a> {
    method: &'a str,
    target: &'a str,
    /// The first `Host` header, as Node keeps it.
    host: Option<&'a str>,
    /// Every `Origin` header joined with `, `, as Node joins them.
    origin: String,
}

impl<'a> Request<'a> {
    fn parse(head: &'a str) -> Option<Self> {
        let mut lines = head.split("\r\n");
        let mut start = lines.next()?.split(' ');
        let (method, target, version) = (start.next()?, start.next()?, start.next()?);
        if start.next().is_some()
            || method.is_empty()
            || target.is_empty()
            || !version.starts_with("HTTP/1.")
        {
            return None;
        }
        let mut request = Self {
            method,
            target,
            host: None,
            origin: String::new(),
        };
        let mut origins = Vec::new();
        for line in lines {
            let (name, value) = line.split_once(':')?;
            if name.is_empty()
                || name
                    .bytes()
                    .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
            {
                return None;
            }
            let value = value.trim_matches([' ', '\t']);
            if name.eq_ignore_ascii_case("host") {
                request.host.get_or_insert(value);
            } else if name.eq_ignore_ascii_case("origin") {
                origins.push(value);
            }
        }
        request.origin = origins.join(", ");
        Some(request)
    }
}
