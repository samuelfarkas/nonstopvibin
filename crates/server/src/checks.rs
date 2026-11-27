//! The checks every request passes before its handler: shutdown, Host, the upgrade path, Origin
//! and the session token for `/api`, and the loopback-only agent gateway, usage hubs and Claude
//! Agent engine.

use std::borrow::Cow;
use std::fmt;

use hyper::body::Incoming;
use hyper::header::{self, HeaderMap};
use hyper::{Method, Request, StatusCode};
use nsv_domain::endpoint::{CLAUDE_AGENT_ROUTES, LOOPBACK_HOST, Origin, PROFILE_ROUTES};
use nsv_domain::error::AppError;
use nsv_domain::secret::SessionToken;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::hub;
use crate::listener::Phase;

/// The Vite development server, whose Host and Origin a development run accepts.
pub(crate) const DEV_HOST: &str = "127.0.0.1:5173";

/// An exact DNS name such as a Tailscale Serve host, which an HTTPS reverse proxy uses for the
/// interface and the management API: lowercase labels, at least one dot, an alphabetic
/// top-level label (so no IP literal), no port or wildcard, at most 253 characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AllowedHost(String);

/// A host name that is not an exact DNS name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an exact DNS name")]
pub struct InvalidHost;

impl AllowedHost {
    /// # Errors
    /// When `host` is not an exact DNS name.
    pub fn parse(host: &str) -> Result<Self, InvalidHost> {
        let labels: Vec<&str> = host.split('.').collect();
        let label = |text: &str| {
            (1..=63).contains(&text.len())
                && text
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !text.starts_with('-')
                && !text.ends_with('-')
        };
        let valid = host.len() <= 253
            && labels.len() >= 2
            && labels.iter().all(|text| label(text))
            && labels
                .last()
                .is_some_and(|top| top.starts_with(|c: char| c.is_ascii_lowercase()));
        if valid {
            Ok(Self(host.to_owned()))
        } else {
            Err(InvalidHost)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AllowedHost {
    type Error = InvalidHost;

    fn try_from(host: String) -> Result<Self, InvalidHost> {
        Self::parse(&host)
    }
}

impl From<AllowedHost> for String {
    fn from(host: AllowedHost) -> Self {
        host.0
    }
}

impl fmt::Display for AllowedHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a request goes once it passed the checks, or why it may not.
pub(crate) enum Admission {
    /// A refused upgrade: an empty answer that closes the connection.
    Closed(StatusCode),
    WebSocket,
    Refused(AppError),
    /// The management API, with the request URL as WHATWG URL parsing sees it.
    Api(url::Url),
    /// A profile's usage hub, `/p/<slug>/v0/management…`, with the request URL as `Api` has it.
    Hub(url::Url),
    /// A profile's Claude Agent engine, `/claude-agent/<profile id>/…`: the core's Messages
    /// requests and Claude Code's MCP calls, with the request URL as `Api` has it.
    ClaudeAgent(url::Url),
    Gateway,
    Asset {
        path: String,
        head: bool,
    },
}

/// What a request must present: a Host this daemon answers to, the Origin its interface runs
/// on, and the session token.
pub(crate) struct Checks {
    port: u16,
    origin: Origin,
    /// `origin` as the base that request targets resolve against, as `new URL(req.url, origin)`.
    base: url::Url,
    allow_hosts: Vec<AllowedHost>,
    development: bool,
    token: SessionToken,
    phase: watch::Receiver<Phase>,
}

impl Checks {
    pub(crate) fn new(
        port: u16,
        origin: Origin,
        allow_hosts: Vec<AllowedHost>,
        development: bool,
        token: SessionToken,
        phase: watch::Receiver<Phase>,
    ) -> Self {
        #[expect(
            clippy::expect_used,
            reason = "an Origin is http://<loopback host>:<port>, which is a URL"
        )]
        let base = url::Url::parse(origin.as_str()).expect("an origin is a URL");
        Self {
            port,
            origin,
            base,
            allow_hosts,
            development,
            token,
            phase,
        }
    }

    pub(crate) fn token(&self) -> &SessionToken {
        &self.token
    }

    pub(crate) fn allow_hosts(&self) -> &[AllowedHost] {
        &self.allow_hosts
    }

    pub(crate) fn admit(&self, request: &Request<Incoming>) -> Admission {
        match switch(request.headers()) {
            Switch::None => {}
            Switch::WebSocket => return self.upgrade(request),
            // The upgrade path sends a bodiless GET, which would drop this request's method
            // and body; main's core answered it with an error too.
            Switch::Other => return Admission::Closed(StatusCode::BAD_REQUEST),
        }
        self.request(request).unwrap_or_else(Admission::Refused)
    }

    /// The WebSocket path, as Node's `upgrade` event: a valid loopback Host and no Origin.
    fn upgrade(&self, request: &Request<Incoming>) -> Admission {
        let host = text(request, header::HOST);
        if !self.valid_host(host) || self.allowed_host(host) || origin(request).is_some() {
            Admission::Closed(StatusCode::FORBIDDEN)
        } else {
            Admission::WebSocket
        }
    }

    fn request(&self, request: &Request<Incoming>) -> Result<Admission, AppError> {
        if *self.phase.borrow() >= Phase::Draining {
            return Err(AppError::shutting_down());
        }
        let host = text(request, header::HOST);
        if !self.valid_host(host) {
            return Err(AppError::new("Unrecognized host.", 403));
        }
        let allowed = self.allowed_host(host);
        let url = self
            .base
            .join(&request.uri().to_string())
            .map_err(|_| AppError::malformed_path())?;
        let path = url.path();
        if path.starts_with("/api/") {
            if let Some(origin) = origin(request) {
                let accepted = if allowed {
                    host.is_some_and(|host| origin == format!("https://{host}"))
                } else {
                    origin == self.origin.as_str()
                        || (self.development && origin == format!("http://{DEV_HOST}"))
                };
                if !accepted {
                    return Err(AppError::new(
                        "Cross-origin management requests are not allowed.",
                        403,
                    ));
                }
            }
            let authorized = text(request, header::AUTHORIZATION)
                .and_then(|presented| presented.strip_prefix("Bearer "))
                .is_some_and(|presented| self.token.matches(presented));
            if !authorized {
                return Err(AppError::new(
                    "Open NonstopVibin from the desktop app, or run nonstopvibin url on the server for a new session link.",
                    401,
                ));
            }
            return Ok(Admission::Api(url));
        }
        let engine = path.starts_with(CLAUDE_AGENT_ROUTES);
        if path.starts_with("/v1") || path.starts_with(PROFILE_ROUTES) || engine {
            if allowed {
                return Err(AppError::new(
                    "The agent API is only available on the loopback address.",
                    403,
                ));
            }
            if origin(request).is_some() {
                return Err(AppError::new(
                    "The agent API accepts local agent clients, not browser origins.",
                    403,
                ));
            }
            if hub::target(path).is_some() {
                return Ok(Admission::Hub(url));
            }
            if engine {
                return Ok(Admission::ClaudeAgent(url));
            }
            return Ok(Admission::Gateway);
        }
        if request.method() != Method::GET && request.method() != Method::HEAD {
            return Err(AppError::method_not_allowed());
        }
        Ok(Admission::Asset {
            path: path.to_owned(),
            head: request.method() == Method::HEAD,
        })
    }

    fn valid_host(&self, host: Option<&str>) -> bool {
        let Some(host) = host else { return false };
        host.strip_prefix(LOOPBACK_HOST)
            .and_then(|rest| rest.strip_prefix(':'))
            .or_else(|| host.strip_prefix("localhost:"))
            // The port's exact decimal text: `parse` would also take a sign or leading zeros.
            .is_some_and(|port| port.parse() == Ok(self.port) && !port.starts_with(['+', '0']))
            || (self.development && host == DEV_HOST)
            || self.allowed_host(Some(host))
    }

    fn allowed_host(&self, host: Option<&str>) -> bool {
        host.is_some_and(|host| {
            self.allow_hosts
                .iter()
                .any(|allowed| allowed.as_str() == host)
        })
    }
}

/// Every header value of a request as text; one that is not text reads as absent.
fn text(request: &Request<Incoming>, name: header::HeaderName) -> Option<&str> {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

/// What a request asks to switch to. Node's `upgrade` event fired for `Connection: upgrade`
/// with an `Upgrade` header; without the token the `Upgrade` header is ignored.
enum Switch {
    None,
    WebSocket,
    Other,
}

fn switch(headers: &HeaderMap) -> Switch {
    let tokens = |name| {
        headers
            .get_all(name)
            .into_iter()
            .flat_map(|value| value.as_bytes().split(|&byte| byte == b','))
            .map(<[u8]>::trim_ascii)
    };
    let requested = headers.contains_key(header::UPGRADE)
        && tokens(header::CONNECTION).any(|token| token.eq_ignore_ascii_case(b"upgrade"));
    if !requested {
        Switch::None
    } else if tokens(header::UPGRADE).any(|token| token.eq_ignore_ascii_case(b"websocket")) {
        Switch::WebSocket
    } else {
        Switch::Other
    }
}

/// A browser Origin as Node read it: repeated fields joined with ", ", which no accepted origin
/// matches, and an empty one counts as none.
fn origin(request: &Request<Incoming>) -> Option<Cow<'_, str>> {
    let mut values = request.headers().get_all(header::ORIGIN).iter();
    let mut origin = String::from_utf8_lossy(values.next()?.as_bytes());
    for value in values {
        let joined = origin.to_mut();
        joined.push_str(", ");
        joined.push_str(&String::from_utf8_lossy(value.as_bytes()));
    }
    (!origin.is_empty()).then_some(origin)
}
