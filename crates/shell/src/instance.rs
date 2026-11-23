//! The second launch: the data directory's lock is held, so a daemon runs. Its `server.json`
//! says where, and `POST /api/desktop/show` with its token brings its main window forward.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use nsv_store::files::read_own_file;
use nsv_store::layout::session_path;
use serde::Deserialize;

/// How long a second launch waits for a daemon that holds the lock but is still starting.
const STARTING: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(100);
const ANSWER: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum ShowError {
    #[error("server.json must be an owner-only ordinary file you own (chmod 600).")]
    UnsafeSession,
    #[error("NonstopVibin is already running but has not said where. Try again in a moment.")]
    NoSession,
    #[error("NonstopVibin is already running but did not answer: {0}")]
    NoAnswer(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// The part of `server.json` a second launch needs.
#[derive(Deserialize)]
struct Session {
    port: u16,
    token: String,
}

/// Asks the daemon that holds `data_directory` to show its main window. Blocks for up to a
/// few seconds: call it before any runtime starts, then exit.
///
/// # Errors
/// When `server.json` is unsafe or does not appear, or the daemon does not answer 2xx.
pub fn show_running(data_directory: &Path) -> Result<(), ShowError> {
    let path = session_path(data_directory);
    let deadline = Instant::now() + STARTING;
    let session = loop {
        let text = read_own_file(&path, 0o077, 64 * 1024, || ShowError::UnsafeSession)?;
        // A daemon that is starting has not written it yet; a malformed one counts as absent.
        if let Some(session) = text.and_then(|text| serde_json::from_str::<Session>(&text).ok()) {
            break session;
        }
        if Instant::now() >= deadline {
            return Err(ShowError::NoSession);
        }
        pause(POLL);
    };
    post_show(&session).map_err(|error| ShowError::NoAnswer(error.to_string()))
}

#[expect(
    clippy::disallowed_methods,
    reason = "a second launch, before any runtime"
)]
fn pause(duration: Duration) {
    std::thread::sleep(duration);
}

fn post_show(session: &Session) -> io::Result<()> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, session.port));
    let mut stream = TcpStream::connect_timeout(&address, ANSWER)?;
    stream.set_read_timeout(Some(ANSWER))?;
    stream.set_write_timeout(Some(ANSWER))?;
    let request = format!(
        "POST /api/desktop/show HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        session.port, session.token
    );
    stream.write_all(request.as_bytes())?;
    let mut head = [0u8; 12];
    stream.read_exact(&mut head)?;
    // "HTTP/1.1 2xx"
    if head.starts_with(b"HTTP/1.") && head.get(9) == Some(&b'2') {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "it answered {}",
            String::from_utf8_lossy(&head)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::thread;

    fn directory(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("nsv-shell-instance-{name}-{}", std::process::id()));
        drop(fs::remove_dir_all(&path));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_session(directory: &Path, port: u16, mode: u32) {
        let file = directory.join("server.json");
        fs::write(
            &file,
            format!(r#"{{"pid":1,"port":{port},"token":"t0ken","allowHosts":[],"version":"0.2.0","startedAt":"x"}}"#),
        )
        .unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Answers one request with `status` and hands back what it received.
    fn daemon(status: &'static str) -> (u16, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            stream
                .write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes())
                .unwrap();
            String::from_utf8(request).unwrap()
        });
        (port, served)
    }

    #[test]
    fn asks_the_running_daemon_with_its_token() {
        let data = directory("show");
        let (port, served) = daemon("204 No Content");
        write_session(&data, port, 0o600);
        show_running(&data).unwrap();
        let request = served.join().unwrap();
        assert!(
            request.starts_with("POST /api/desktop/show HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(request.contains(&format!("\r\nHost: 127.0.0.1:{port}\r\n")));
        assert!(request.contains("\r\nAuthorization: Bearer t0ken\r\n"));
        fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn a_refusal_or_an_unsafe_file_is_an_error() {
        let data = directory("refused");
        let (port, served) = daemon("401 Unauthorized");
        write_session(&data, port, 0o600);
        assert!(matches!(show_running(&data), Err(ShowError::NoAnswer(_))));
        served.join().unwrap();
        write_session(&data, port, 0o644);
        assert_eq!(
            show_running(&data).unwrap_err().to_string(),
            "server.json must be an owner-only ordinary file you own (chmod 600)."
        );
        fs::remove_dir_all(data).unwrap();
    }
}
