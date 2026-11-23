//! The daemon's two HTTP clients. Neither keeps connections between calls, follows a redirect
//! it was not told to, or reads more of a body than its caller allows.
//!
//! - [`loopback`]: HTTP/1 to a port on this machine's loopback interface. No redirects: a 3xx
//!   is an ordinary [`Response`].
//! - [`Https`]: GET over TLS (rustls with ring and the webpki roots), to memory or streamed to a
//!   file with a running SHA-256. Redirects are refused, or followed to allowlisted HTTPS hosts,
//!   five at most.
//!
//! The timeout covers the whole call, body included, as `AbortSignal.timeout` did.
//!
//! The gateway is not a client of this crate: it proxies to cores over its own streaming hyper
//! connection. A local discovery URL names `localhost`; its caller resolves it to a loopback
//! address, as fetch did.
#![forbid(unsafe_code)]

mod https;
mod loopback;
mod url;
mod wire;

use std::io;

pub use hyper::{HeaderMap, Method, StatusCode};

pub use https::{Download, Get, Https, Redirects};
pub use loopback::{Loopback, LoopbackRequest, loopback};

/// Sent on every HTTPS request whose caller names no `User-Agent`.
pub const USER_AGENT: &str = concat!("nonstopvibin/", env!("CARGO_PKG_VERSION"));

/// Hosts a GitHub release download may be redirected to: GitHub serves assets from its CDN.
pub const GITHUB_RELEASE_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// A response read to its end.
#[derive(Debug)]
pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Response {
    /// Whether the status is 2xx, as fetch's `response.ok`.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.status.is_success()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// Not an `https://host[:port]/path` URL, or a header that HTTP cannot carry.
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    #[error("could not connect: {0}")]
    Connect(io::Error),
    #[error("TLS failed: {0}")]
    Tls(io::Error),
    #[error("TLS is not available: {0}")]
    TlsConfig(#[from] rustls::Error),
    #[error("HTTP failed: {0}")]
    Http(#[from] hyper::Error),
    #[error("the request timed out")]
    Timeout,
    #[error("the response is larger than {limit} bytes")]
    TooLarge { limit: u64 },
    /// [`Https::download`] got no 2xx.
    #[error("HTTP {0}")]
    Status(StatusCode),
    /// A redirect the request refuses, or one to a host off its allowlist.
    #[error("refused a redirect")]
    Redirect,
    #[error("more than 5 redirects")]
    TooManyRedirects,
    /// Writing a download failed.
    #[error("{0}")]
    File(io::Error),
}
