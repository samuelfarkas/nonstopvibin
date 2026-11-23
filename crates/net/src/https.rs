//! GET over TLS: to memory, or streamed to a new file with a running SHA-256.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::{Request, StatusCode, header};
use rustls::pki_types::ServerName;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::url::Url;
use crate::wire::{Wire, check_declared, collect, too_large};
use crate::{NetError, Response, USER_AGENT};

const MAX_REDIRECTS: usize = 5;

/// Where a request may be redirected.
#[derive(Debug, Clone, Copy)]
pub enum Redirects<'a> {
    /// A redirect fails the request, as fetch's `redirect: "error"`.
    Refuse,
    /// Followed only to these hosts (lowercase, exact), over HTTPS.
    Follow(&'a [&'a str]),
}

#[derive(Debug, Clone, Copy)]
pub struct Get<'a> {
    /// `https://host[:port]/path?query`; any host.
    pub url: &'a str,
    /// Sent after `Host`; [`USER_AGENT`] unless one names `User-Agent`. On a redirect to
    /// another host, `Authorization` is dropped.
    pub headers: &'a [(&'a str, &'a str)],
    /// For the whole call: every redirect and the body.
    pub timeout: Duration,
    /// The most body bytes read; a larger response fails with [`NetError::TooLarge`].
    pub limit: u64,
    pub redirects: Redirects<'a>,
}

/// A body [`Https::download`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Download {
    pub sha256: [u8; 32],
    pub size: u64,
}

/// The HTTPS client. Build one per daemon: it holds the TLS configuration and root store.
#[derive(Clone)]
pub struct Https {
    tls: TlsConnector,
}

impl std::fmt::Debug for Https {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Https")
    }
}

impl Https {
    /// # Errors
    /// [`NetError::TlsConfig`] if ring offers no safe protocol version.
    pub fn new() -> Result<Self, NetError> {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            tls: TlsConnector::from(Arc::new(config)),
        })
    }

    /// Reads the response to `request` whatever its status; check [`Response::ok`].
    ///
    /// # Errors
    /// [`NetError::Timeout`], [`NetError::TooLarge`], [`NetError::Redirect`],
    /// [`NetError::TooManyRedirects`], [`NetError::Invalid`] for a URL or header, or a
    /// connection, TLS or protocol failure.
    pub async fn get(&self, request: &Get<'_>) -> Result<Response, NetError> {
        get(self, request).await
    }

    /// Streams a 2xx body into a new file at `path` with exactly `mode`, whatever the umask,
    /// and answers its SHA-256 and size. A failure can leave a partial file for the caller
    /// to remove; a non-2xx response creates none.
    ///
    /// # Errors
    /// As [`Https::get`], [`NetError::Status`] for a non-2xx response, and
    /// [`NetError::File`] when the file cannot be created or written (an existing file
    /// included).
    pub async fn download(
        &self,
        request: &Get<'_>,
        path: &Path,
        mode: u32,
    ) -> Result<Download, NetError> {
        download(self, request, path, mode).await
    }
}

/// How a request reaches a host. TLS in the daemon; plain TCP in this module's tests, which
/// prove the redirect, limit and streaming rules without certificates.
trait Connect {
    type Io: AsyncRead + AsyncWrite + Unpin + Send + 'static;
    async fn connect(&self, url: &Url) -> Result<Self::Io, NetError>;
}

impl Connect for Https {
    type Io = tokio_rustls::client::TlsStream<TcpStream>;

    async fn connect(&self, url: &Url) -> Result<Self::Io, NetError> {
        let name = ServerName::try_from(url.host.clone())
            .map_err(|_| NetError::Invalid("a host TLS cannot name"))?;
        let stream = TcpStream::connect((url.host.as_str(), url.port))
            .await
            .map_err(NetError::Connect)?;
        stream.set_nodelay(true).map_err(NetError::Connect)?;
        self.tls.connect(name, stream).await.map_err(NetError::Tls)
    }
}

async fn get(connector: &impl Connect, request: &Get<'_>) -> Result<Response, NetError> {
    tokio::time::timeout(request.timeout, async {
        let (mut wire, response) = head(connector, request).await?;
        collect(&mut wire, response, request.limit).await
    })
    .await
    .map_err(|_| NetError::Timeout)?
}

async fn download(
    connector: &impl Connect,
    request: &Get<'_>,
    path: &Path,
    mode: u32,
) -> Result<Download, NetError> {
    tokio::time::timeout(request.timeout, async {
        let (mut wire, response) = head(connector, request).await?;
        let (head, mut body) = response.into_parts();
        if !head.status.is_success() {
            return Err(NetError::Status(head.status));
        }
        check_declared(&head.headers, request.limit)?;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path)
            .await
            .map_err(NetError::File)?;
        file.set_permissions(Permissions::from_mode(mode))
            .await
            .map_err(NetError::File)?;
        let mut hash = Sha256::new();
        let mut size = 0_usize;
        wire.read(&mut body, async |data: Bytes| {
            if too_large(size, data.len(), request.limit) {
                return Err(NetError::TooLarge {
                    limit: request.limit,
                });
            }
            size = size.saturating_add(data.len());
            hash.update(&data);
            file.write_all(&data).await.map_err(NetError::File)
        })
        .await?;
        file.flush().await.map_err(NetError::File)?;
        Ok(Download {
            sha256: hash.finalize().into(),
            size: u64::try_from(size).unwrap_or(u64::MAX),
        })
    })
    .await
    .map_err(|_| NetError::Timeout)?
}

/// Sends the GET, following redirects as `request` allows, and answers the final head.
async fn head<C: Connect>(
    connector: &C,
    request: &Get<'_>,
) -> Result<(Wire<C::Io>, hyper::Response<Incoming>), NetError> {
    let first = Url::parse(request.url).ok_or(NetError::Invalid("not an HTTPS URL"))?;
    let mut url = first.clone();
    for _ in 0..=MAX_REDIRECTS {
        let io = connector.connect(&url).await?;
        let (wire, response) =
            Wire::send(io, build(request, &url, url.host == first.host)?).await?;
        let Some(location) = redirect_location(&response) else {
            return Ok((wire, response));
        };
        let Redirects::Follow(hosts) = request.redirects else {
            return Err(NetError::Redirect);
        };
        url = url
            .redirect(location)
            .filter(|next| hosts.contains(&next.host.as_str()))
            .ok_or(NetError::Redirect)?;
    }
    Err(NetError::TooManyRedirects)
}

/// The `Location` of a redirect status; a 3xx without one is an ordinary response.
fn redirect_location(response: &hyper::Response<Incoming>) -> Option<&str> {
    let redirect = matches!(
        response.status(),
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    );
    redirect
        .then(|| response.headers().get(header::LOCATION)?.to_str().ok())
        .flatten()
}

fn build(request: &Get<'_>, url: &Url, same_host: bool) -> Result<Request<Full<Bytes>>, NetError> {
    let mut builder = Request::get(url.target.as_str()).header(header::HOST, url.authority());
    let mut agent = false;
    for (name, value) in request.headers {
        if !same_host && name.eq_ignore_ascii_case("authorization") {
            continue;
        }
        agent |= name.eq_ignore_ascii_case("user-agent");
        builder = builder.header(*name, *value);
    }
    if !agent {
        builder = builder.header(header::USER_AGENT, USER_AGENT);
    }
    builder
        .body(Full::new(Bytes::new()))
        .map_err(|_| NetError::Invalid("a header HTTP cannot carry"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    use tokio::net::TcpListener;

    use super::*;

    /// Plain TCP to a local stub per host name.
    struct Plain(HashMap<&'static str, u16>);

    impl Connect for Plain {
        type Io = TcpStream;
        async fn connect(&self, url: &Url) -> Result<TcpStream, NetError> {
            let port = self.0.get(url.host.as_str()).expect("a stub for the host");
            TcpStream::connect(("127.0.0.1", *port))
                .await
                .map_err(NetError::Connect)
        }
    }

    /// A stub that answers each connection with the next canned response and records each
    /// request head.
    async fn stub(responses: Vec<Vec<u8>>) -> (u16, tokio::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen, heads) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            for response in responses {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    head.push_str(&line);
                }
                seen.send(head).await.unwrap();
                let mut stream = reader.into_inner();
                stream.write_all(&response).await.unwrap();
                stream.shutdown().await.unwrap();
                let _ = stream.read(&mut [0; 1]).await;
            }
        });
        (port, heads)
    }

    fn redirect(location: &str) -> Vec<u8> {
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n")
            .into_bytes()
    }

    fn ok(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn request<'a>(url: &'a str, redirects: Redirects<'a>) -> Get<'a> {
        Get {
            url,
            headers: &[("Authorization", "Bearer secret")],
            timeout: Duration::from_secs(5),
            limit: 1024,
            redirects,
        }
    }

    #[tokio::test]
    async fn follows_redirects_only_to_allowlisted_hosts_and_drops_authorization() {
        let (origin, mut origin_heads) = stub(vec![redirect("https://assets.example/file")]).await;
        let (assets, mut asset_heads) = stub(vec![ok("archive")]).await;
        let connector = Plain(HashMap::from([
            ("origin.example", origin),
            ("assets.example", assets),
        ]));
        let response = get(
            &connector,
            &request(
                "https://origin.example/x",
                Redirects::Follow(&["assets.example"]),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.body, b"archive");
        let first = origin_heads.recv().await.unwrap();
        assert!(first.starts_with("GET /x HTTP/1.1\r\n"), "{first}");
        assert!(first.contains("host: origin.example\r\n"), "{first}");
        assert!(
            first.contains("authorization: Bearer secret\r\n"),
            "{first}"
        );
        assert!(
            first.contains(&format!("user-agent: {USER_AGENT}\r\n")),
            "{first}"
        );
        let second = asset_heads.recv().await.unwrap();
        assert!(second.starts_with("GET /file HTTP/1.1\r\n"), "{second}");
        assert!(
            !second.to_ascii_lowercase().contains("authorization"),
            "{second}"
        );
    }

    #[tokio::test]
    async fn refuses_redirects_off_the_allowlist_or_when_told_to() {
        for (location, redirects) in [
            (
                "https://evil.example/",
                Redirects::Follow(&["assets.example"]),
            ),
            (
                "http://assets.example/",
                Redirects::Follow(&["assets.example"]),
            ),
            ("/same-host", Redirects::Refuse),
        ] {
            let (origin, _heads) = stub(vec![redirect(location)]).await;
            let connector = Plain(HashMap::from([("origin.example", origin)]));
            let result = get(&connector, &request("https://origin.example/", redirects)).await;
            assert!(
                matches!(result, Err(NetError::Redirect)),
                "{location}: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn stops_after_five_redirects() {
        let (origin, _heads) = stub(vec![redirect("/again"); 6]).await;
        let connector = Plain(HashMap::from([("origin.example", origin)]));
        let result = get(
            &connector,
            &request(
                "https://origin.example/",
                Redirects::Follow(&["origin.example"]),
            ),
        )
        .await;
        assert!(
            matches!(result, Err(NetError::TooManyRedirects)),
            "{result:?}"
        );

        let (origin, _heads) =
            stub([vec![redirect("/again"); 5], vec![ok("fifth")]].concat()).await;
        let connector = Plain(HashMap::from([("origin.example", origin)]));
        let response = get(
            &connector,
            &request(
                "https://origin.example/",
                Redirects::Follow(&["origin.example"]),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.body, b"fifth");
    }

    #[tokio::test]
    async fn caps_the_body_declared_or_streamed() {
        let declared = b"HTTP/1.1 200 OK\r\nContent-Length: 2000\r\n\r\n".to_vec();
        let chunked = [
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".as_slice(),
            format!("{:x}\r\n{}\r\n0\r\n\r\n", 1025, "a".repeat(1025)).as_bytes(),
        ]
        .concat();
        for response in [declared, chunked] {
            let (origin, _heads) = stub(vec![response]).await;
            let connector = Plain(HashMap::from([("origin.example", origin)]));
            let result = get(
                &connector,
                &request("https://origin.example/", Redirects::Refuse),
            )
            .await;
            assert!(
                matches!(result, Err(NetError::TooLarge { limit: 1024 })),
                "{result:?}"
            );
        }
    }

    #[tokio::test]
    async fn times_out_the_whole_call() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let silent = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let connector = Plain(HashMap::from([("origin.example", port)]));
        let mut slow = request("https://origin.example/", Redirects::Refuse);
        slow.timeout = Duration::from_millis(200);
        let result = get(&connector, &slow).await;
        assert!(matches!(result, Err(NetError::Timeout)), "{result:?}");
        silent.abort();
    }

    #[tokio::test]
    async fn downloads_stream_to_an_exact_mode_file_with_their_sha256() {
        let directory = std::env::temp_dir().join(format!("nsv-net-{}", std::process::id()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let path = directory.join("archive.tar.gz");
        let _ = tokio::fs::remove_file(&path).await;
        let (origin, _heads) = stub(vec![ok("core"), ok("again")]).await;
        let connector = Plain(HashMap::from([("origin.example", origin)]));
        let get = request("https://origin.example/a", Redirects::Refuse);
        let written = download(&connector, &get, &path, 0o640).await.unwrap();
        // sha256("core"), as tests/core-updates.test.ts states it.
        assert_eq!(
            hex(&written.sha256),
            "0d45f5fd462b8c70bffb10021ac1bcff3f58f29b1faf7568595095427d42812c"
        );
        assert_eq!(written.size, 4);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"core");
        let mode = tokio::fs::metadata(&path)
            .await
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640);
        // An existing file is never overwritten.
        let again = download(&connector, &get, &path, 0o640).await;
        assert!(matches!(again, Err(NetError::File(_))), "{again:?}");
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_download_status_creates_no_file() {
        let path = std::env::temp_dir().join(format!("nsv-net-missing-{}", std::process::id()));
        let (origin, _heads) = stub(vec![
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
        ])
        .await;
        let connector = Plain(HashMap::from([("origin.example", origin)]));
        let result = download(
            &connector,
            &request("https://origin.example/a", Redirects::Refuse),
            &path,
            0o600,
        )
        .await;
        assert!(
            matches!(result, Err(NetError::Status(StatusCode::NOT_FOUND))),
            "{result:?}"
        );
        assert!(!path.exists());
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().fold(String::new(), |mut text, byte| {
            use std::fmt::Write;
            let _ = write!(text, "{byte:02x}");
            text
        })
    }
}
