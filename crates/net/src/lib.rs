//! The daemon's two HTTP clients. Neither keeps connections between calls, follows a redirect
//! it was not told to, or reads more of a body than its caller allows.
//!
//! - [`loopback`]: HTTP/1 to a port on this machine's loopback interface. No redirects: a 3xx
//!   is an ordinary [`Response`].
//! - [`Https`]: GET over TLS (rustls with ring and the webpki roots), to memory or streamed to a
//!   file with a running SHA-256. Redirects are refused, or followed to allowlisted HTTPS hosts,
//!   five at most.
//!
//! Every outgoing request of `src/server`, and the client and limits its port uses. The timeout
//! covers the whole call, body included, as `AbortSignal.timeout` did. Caps marked *new* had none
//! in TypeScript (`response.json()` read any size).
//!
//! | TypeScript caller | Request | Client | Timeout | Body cap | Redirects |
//! |---|---|---|---|---|---|
//! | `core.ts` `httpManagement` (quota `/api-call`, usage queue, auth files, config) | `127.0.0.1:<core>/v0/management/*` | loopback | per call | 32 MiB *new* | none |
//! | `core.ts` `models` | `127.0.0.1:<core>/v1/models` | loopback | 15 s | 16 MiB *new* | none |
//! | `server.ts` agent check | the daemon's own gateway, `<endpoint>/models` | loopback | 10 s | 16 MiB *new* | none |
//! | `cli.ts` `probe` | a running daemon's `/api/state` | loopback | 2 s | 16 MiB *new* | none |
//! | `api-accounts.ts` model discovery, local provider | `http://{127.0.0.1,localhost,[::1]}:<port>/…/models` | loopback | 20 s | 16 MiB *new* | none |
//! | `api-accounts.ts` model discovery | `https://<any host>/…/models` | [`Https::get`] | 20 s | 16 MiB *new* | refused |
//! | `identity.ts` Claude profile | `https://api.anthropic.com/api/oauth/profile` | [`Https::get`] | 15 s | 1 MiB *new* | refused |
//! | `quota.ts` `OpenCode` Go | `https://opencode.ai/zen/go/v1/usage` | [`Https::get`] | 15 s | 1 MiB *new* | refused |
//! | `model-catalog.ts` | `https://models.dev/api.json` | [`Https::get`] | 10 s | 16 000 000 B | refused |
//! | `usage-pricing.ts` euro rate | `https://api.frankfurter.dev/v2/providers/ecb/rate/usd/eur` | [`Https::get`] | 5 s | 4096 B | refused |
//! | `core-updates.ts` pin feed | `https://raw.githubusercontent.com/…/core-release.json` (or `NONSTOPVIBIN_CORE_FEED`) | [`Https::get`] | 15 s | 64 KiB *new* | `raw.githubusercontent.com` |
//! | `core-install.ts` `download` | `https://github.com/router-for-me/CLIProxyAPI/releases/download/…` | [`Https::download`] | 10 min | 256 MiB | `github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com` |
//!
//! Not clients of this crate: `gateway.ts` proxies to cores with its own streaming hyper
//! connection (the gateway crate), and `credential-socket.ts`, `agent-opencode.ts` and
//! `agent-pi.ts` generate code that the agents run. A local discovery URL names `localhost`;
//! its caller resolves it to a loopback address, as fetch did.
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
