//! HTTP/1 to a port on the loopback interface: the core management API, a core's or the
//! daemon's own model list, a running daemon's state, a local provider.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Method, Request, header};
use tokio::net::TcpStream;

use crate::wire::{Wire, collect};
use crate::{NetError, Response};

/// A loopback address and port: the only place [`loopback`] connects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loopback(SocketAddr);

impl Loopback {
    /// `127.0.0.1:<port>`, where cores and the daemon listen.
    #[must_use]
    pub const fn v4(port: u16) -> Self {
        Self(SocketAddr::new(
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
        ))
    }

    /// `None` unless `address` is a loopback address.
    #[must_use]
    pub fn new(address: SocketAddr) -> Option<Self> {
        address.ip().is_loopback().then_some(Self(address))
    }

    #[must_use]
    pub const fn address(self) -> SocketAddr {
        self.0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LoopbackRequest<'a> {
    pub to: Loopback,
    pub method: &'a Method,
    /// The path and query, starting with `/`.
    pub target: &'a str,
    /// Sent after `Host`, which is always the address.
    pub headers: &'a [(&'a str, &'a str)],
    pub body: Option<&'a [u8]>,
    /// For the whole call: connect, request and body.
    pub timeout: Duration,
    /// The most body bytes read; a larger response fails with [`NetError::TooLarge`].
    pub limit: u64,
}

/// Sends one request on a new connection and reads the whole response. A 3xx is answered as
/// is: nothing is followed.
///
/// # Errors
/// [`NetError::Timeout`], [`NetError::TooLarge`], [`NetError::Invalid`] for a target or header
/// HTTP cannot carry, or a connection or protocol failure.
pub async fn loopback(request: LoopbackRequest<'_>) -> Result<Response, NetError> {
    tokio::time::timeout(request.timeout, exchange(&request))
        .await
        .map_err(|_| NetError::Timeout)?
}

async fn exchange(request: &LoopbackRequest<'_>) -> Result<Response, NetError> {
    let mut builder = Request::builder()
        .method(request.method)
        .uri(request.target)
        .header(header::HOST, request.to.0.to_string());
    for (name, value) in request.headers {
        builder = builder.header(*name, *value);
    }
    let body = request.body.map_or_else(Bytes::new, Bytes::copy_from_slice);
    let built = builder
        .body(Full::new(body))
        .map_err(|_| NetError::Invalid("a target or header HTTP cannot carry"))?;
    let stream = TcpStream::connect(request.to.0)
        .await
        .map_err(NetError::Connect)?;
    stream.set_nodelay(true).map_err(NetError::Connect)?;
    let (mut wire, response) = Wire::send(stream, built).await?;
    collect(&mut wire, response, request.limit).await
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    use nsv_testkit::Port;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::StatusCode;

    /// Answers one connection with `response` and hands back the raw request it read.
    async fn stub(response: &'static [u8]) -> (Loopback, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let to = Loopback::new(listener.local_addr().unwrap()).unwrap();
        let served = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut seen = Vec::new();
            let mut buffer = [0; 4096];
            while !String::from_utf8_lossy(&seen).contains("\r\n\r\n") || !seen.ends_with(b"}") {
                let read = stream.read(&mut buffer).await.unwrap();
                seen.extend_from_slice(&buffer[..read]);
                if read == 0 || (seen.ends_with(b"\r\n\r\n") && !seen.starts_with(b"POST")) {
                    break;
                }
            }
            stream.write_all(response).await.unwrap();
            stream.shutdown().await.unwrap();
            String::from_utf8(seen).unwrap()
        });
        (to, served)
    }

    fn request(to: Loopback) -> LoopbackRequest<'static> {
        LoopbackRequest {
            to,
            method: &Method::POST,
            target: "/v0/management/config?x=1",
            headers: &[
                ("Authorization", "Bearer management"),
                ("Content-Type", "application/json"),
            ],
            body: Some(br#"{"a":1}"#),
            timeout: Duration::from_secs(5),
            limit: 64,
        }
    }

    #[test]
    fn only_loopback_addresses_are_accepted() {
        assert!(Loopback::new("127.0.0.1:1".parse().unwrap()).is_some());
        assert!(Loopback::new(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 1)).is_some());
        assert!(Loopback::new("192.168.1.2:1".parse().unwrap()).is_none());
        assert!(Loopback::new("0.0.0.0:1".parse().unwrap()).is_none());
        assert_eq!(Loopback::v4(8317).address().to_string(), "127.0.0.1:8317");
    }

    #[tokio::test]
    async fn sends_the_request_and_reads_the_response() {
        let (to, served) =
            stub(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}").await;
        let response = loopback(request(to)).await.unwrap();
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body, br#"{"ok":true}"#);
        let seen = served.await.unwrap();
        assert!(
            seen.starts_with("POST /v0/management/config?x=1 HTTP/1.1\r\n"),
            "{seen}"
        );
        assert!(
            seen.contains(&format!("host: {}\r\n", to.address())),
            "{seen}"
        );
        assert!(
            seen.contains("authorization: Bearer management\r\n"),
            "{seen}"
        );
        assert!(seen.ends_with("\r\n\r\n{\"a\":1}"), "{seen}");
    }

    #[tokio::test]
    async fn a_redirect_is_answered_not_followed() {
        let (to, _served) = stub(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        let response = loopback(request(to)).await.unwrap();
        assert_eq!(response.status, StatusCode::FOUND);
    }

    #[tokio::test]
    async fn a_body_over_the_limit_fails() {
        let (to, _served) = stub(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n41\r\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n0\r\n\r\n",
        )
        .await;
        let result = loopback(request(to)).await;
        assert!(
            matches!(result, Err(NetError::TooLarge { limit: 64 })),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_silent_server_times_out_and_a_closed_port_fails_to_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let to = Loopback::new(listener.local_addr().unwrap()).unwrap();
        let mut slow = request(to);
        slow.timeout = Duration::from_millis(200);
        let result = loopback(slow).await;
        assert!(matches!(result, Err(NetError::Timeout)), "{result:?}");
        let closed = SocketAddr::from((Ipv4Addr::LOCALHOST, Port::reserve().release()));
        let to = Loopback::new(closed).unwrap();
        let result = loopback(request(to)).await;
        assert!(matches!(result, Err(NetError::Connect(_))), "{result:?}");
    }
}
