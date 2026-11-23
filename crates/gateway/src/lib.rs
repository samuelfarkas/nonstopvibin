//! The agent gateway: key extraction, routing, header rewrite, and HTTP, SSE and WebSocket
//! pass-through. A port of `src/server/gateway.ts` and the upgrade path of `server.ts`.
//!
//! The server owns the listener, the Host and Origin checks and the rendering of errors. It
//! serves each agent connection with [`serve_connection`] and hands an agent request to
//! [`Gateway::proxy`], or a WebSocket handshake to [`Gateway::upgrade`]. The gateway reaches the
//! rest of the daemon only through [`Host`], whose answers carry no management key, so a
//! profile key cannot reach the management API.
//!
//! Bodies are never parsed or buffered: they pass through as hyper frames, with hyper's buffers
//! capped at [`MAX_BUF_SIZE`]. Each request opens its own upstream connection, which hyper
//! closes when the core's response body or the pending response is dropped. hyper notices a
//! hang-up during a silent stream: once a request is read, its connection keeps a read pending
//! and fails on the agent's FIN, which drops the response body, closing the upstream connection
//! and counting the request out. `tests/gateway/proxy.rs` proves each step.
#![forbid(unsafe_code)]

mod body;
mod flight;
mod headers;
mod route;

use std::borrow::Cow;
use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http1 as client;
use hyper::header::{self, HeaderValue};
use hyper::service::Service;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use nsv_domain::id::{ProfileId, ProfileSlug};
use nsv_domain::registry::{Agent, AgentCatalog, RegistryError, registry};
use nsv_domain::secret::{ClientKey, CoreKey, KeyDigest};
use tokio::net::TcpStream;

pub use body::Body;
use flight::{Counts, InFlight, Release};

/// The cap on hyper's read buffer in both directions. A request or response head must fit;
/// bodies stream through in pieces no larger.
pub const MAX_BUF_SIZE: usize = 256 * 1024;

const UNAVAILABLE: &str = "The profile proxy is unavailable. Restart the profile from the sidebar if NonstopVibin offers it; otherwise quit and reopen NonstopVibin.";

/// A profile as the gateway sees it. It holds no management key.
#[derive(Debug, Clone)]
pub struct ProfileAuth {
    pub id: ProfileId,
    pub slug: ProfileSlug,
    /// The live key, which the presented key must equal.
    pub client_key: ClientKey,
    pub core_key: CoreKey,
}

/// A curated model catalog for an agent, serialised to JSON by the host.
#[derive(Debug, Clone)]
pub enum Catalog {
    /// The `OpenAI` model list the agent reads itself (`{ "object": "list", "data" }`).
    List(Bytes),
    /// Entries for generated agent configuration (`{ "models" }`), labelled with the profile.
    Entries(Bytes),
}

/// A change in a profile's in-flight count, reported after it happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// A request was counted in; `first` when the profile had none open.
    Opened { first: bool },
    /// A request was counted out; `last` when the profile has none left.
    Closed { last: bool },
}

/// What the gateway needs from the rest of the daemon.
pub trait Host: Send + Sync + 'static {
    /// The profile whose client key the index maps `digest` to, if any. The gateway compares
    /// the presented key with the returned live key in constant time, so a stale index entry
    /// fails closed.
    fn profile(&self, digest: &KeyDigest) -> Option<ProfileAuth>;

    /// Wakes the profile's core if it is dormant, and returns its loopback address. The
    /// gateway drops this future when the agent hangs up, so a core start must not depend on
    /// being polled to completion.
    ///
    /// # Errors
    /// The refusal the agent gets, as `usage.wake` threw it.
    fn wake(&self, profile: &ProfileId)
    -> impl Future<Output = Result<SocketAddr, Refusal>> + Send;

    /// The curated catalog for `agent` on this profile's running core.
    ///
    /// # Errors
    /// The refusal the agent gets, such as a pi profile with no priced model.
    fn catalog(
        &self,
        profile: &ProfileId,
        agent: Agent,
    ) -> impl Future<Output = Result<Catalog, Refusal>> + Send;

    /// A profile's in-flight count changed. Called on every change, never under a lock.
    fn activity(&self, profile: &ProfileId, activity: Activity);
}

/// A request turned down by the gateway or its host, with the status and message the agent
/// gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    status: StatusCode,
    message: Cow<'static, str>,
}

impl Refusal {
    #[must_use]
    pub fn new(status: StatusCode, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    fn unavailable() -> Self {
        Self::new(StatusCode::BAD_GATEWAY, UNAVAILABLE)
    }
}

/// The agent gateway. Cheap to clone; clones share the in-flight counts.
pub struct Gateway<H> {
    inner: Arc<Inner<H>>,
}

impl<H> Clone for Gateway<H> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct Inner<H> {
    host: H,
    counts: Counts,
    /// Each agent with a curated catalog and how it asks for it, from the registry.
    catalogs: Vec<(Agent, AgentCatalog)>,
}

impl<H: Host> Release for Inner<H> {
    fn release(&self, profile: &ProfileId) {
        let last = self.counts.close(profile);
        self.host.activity(profile, Activity::Closed { last });
    }
}

/// A routed request's profile, counted in, with its core awake.
struct Lease {
    auth: ProfileAuth,
    core: SocketAddr,
    flight: InFlight,
}

impl<H: Host> Gateway<H> {
    /// # Errors
    /// When the embedded agent registry does not parse.
    pub fn new(host: H) -> Result<Self, RegistryError> {
        let catalogs = registry()?
            .agents
            .iter()
            .filter_map(|(agent, entry)| entry.catalog.map(|by| (agent, by)))
            .collect();
        Ok(Self {
            inner: Arc::new(Inner {
                host,
                counts: Counts::default(),
                catalogs,
            }),
        })
    }

    /// The profile's requests and `WebSocket` relays open now.
    #[must_use]
    pub fn in_flight(&self, profile: &ProfileId) -> u32 {
        self.inner.counts.get(profile)
    }

    /// Answers an agent request under `/v1` or `/p/`: the curated catalog, or the core's
    /// response passed through.
    ///
    /// # Errors
    /// A refusal the server renders as its JSON error, adding `Retry-After: 2` to a 429 as it
    /// does for every 429.
    pub async fn proxy(&self, request: Request<Incoming>) -> Result<Response<Body>, Refusal> {
        let (parts, body) = request.into_parts();
        let route = self.route(&parts.method, &parts.uri, &parts.headers)?;
        let lease = self.lease(route.auth).await?;
        if let Some(agent) = route.catalog {
            return self.catalog(&lease, agent).await;
        }
        let (mut sender, connection) = handshake(lease.core).await?;
        tokio::spawn(connection);
        let mut upstream = Request::new(body);
        *upstream.method_mut() = parts.method;
        *upstream.uri_mut() = route.target;
        *upstream.headers_mut() =
            headers::forwarded(parts.headers, &lease.auth.core_key, lease.core, None)
                .ok_or_else(Refusal::unavailable)?;
        let response = sender
            .send_request(upstream)
            .await
            .map_err(|_| Refusal::unavailable())?;
        let (head, body) = response.into_parts();
        // A fresh response: the core's reason phrase stays behind, as Node's writeHead drops it.
        let mut reply = Response::new(Body::upstream(body, lease.flight));
        *reply.status_mut() = head.status;
        *reply.headers_mut() = head.headers;
        headers::rewrite_response(reply.headers_mut(), slug_value(&lease.auth)?, false);
        Ok(reply)
    }

    /// Answers a WebSocket handshake under `/v1` or `/p/`, after the server's Host and Origin
    /// checks. On the core's `101` it relays the handshake and then bytes in both directions,
    /// without parsing frames, until either side closes. Any other outcome is
    /// `HTTP/1.1 <status> Proxy error` with `Connection: close`.
    pub async fn upgrade(&self, mut request: Request<Incoming>) -> Response<Body> {
        match self.relay(&mut request).await {
            Ok(response) => response,
            Err(refusal) => proxy_error(refusal.status),
        }
    }

    async fn relay(&self, request: &mut Request<Incoming>) -> Result<Response<Body>, Refusal> {
        let route = self.route(request.method(), request.uri(), request.headers())?;
        let lease = self.lease(route.auth).await?;
        let (mut sender, connection) = handshake(lease.core).await?;
        tokio::spawn(connection.with_upgrades());
        let protocol = request.headers().get(header::UPGRADE).cloned();
        let mut upstream = Request::new(Body::empty());
        *upstream.uri_mut() = route.target;
        *upstream.headers_mut() = headers::forwarded(
            std::mem::take(request.headers_mut()),
            &lease.auth.core_key,
            lease.core,
            protocol,
        )
        .ok_or_else(Refusal::unavailable)?;
        let mut response = sender
            .send_request(upstream)
            .await
            .map_err(|_| Refusal::unavailable())?;
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(Refusal::new(response.status(), "Proxy error"));
        }
        let mut reply = Response::new(Body::empty());
        *reply.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        *reply.headers_mut() = std::mem::take(response.headers_mut());
        headers::rewrite_response(reply.headers_mut(), slug_value(&lease.auth)?, true);
        let agent = hyper::upgrade::on(request);
        let core = hyper::upgrade::on(response);
        let flight = lease.flight;
        tokio::spawn(async move {
            let _flight = flight;
            if let (Ok(agent), Ok(core)) = tokio::join!(agent, core) {
                // Either side closing or failing ends the relay; the result has no one to go to.
                let _relayed = tokio::io::copy_bidirectional(
                    &mut TokioIo::new(agent),
                    &mut TokioIo::new(core),
                )
                .await;
            }
        });
        Ok(reply)
    }

    fn route(
        &self,
        method: &hyper::Method,
        uri: &hyper::Uri,
        headers: &hyper::HeaderMap,
    ) -> Result<route::Route, Refusal> {
        route::route(method, uri, headers, &self.inner.catalogs, |digest| {
            self.inner.host.profile(digest)
        })
    }

    /// Counts the request in before waking the core: the wait is in flight, and keeps a dormant
    /// core from an idle stop.
    async fn lease(&self, auth: ProfileAuth) -> Result<Lease, Refusal> {
        let first = self.inner.counts.open(&auth.id).ok_or_else(|| {
            Refusal::new(
                StatusCode::TOO_MANY_REQUESTS,
                "This profile has 32 requests in flight. Retry shortly.",
            )
        })?;
        self.inner
            .host
            .activity(&auth.id, Activity::Opened { first });
        let inner: Arc<Inner<H>> = Arc::clone(&self.inner);
        let owner: Arc<dyn Release> = inner;
        let flight = InFlight::new(owner, auth.id.clone());
        let core = self.inner.host.wake(&auth.id).await?;
        Ok(Lease { auth, core, flight })
    }

    async fn catalog(&self, lease: &Lease, agent: Agent) -> Result<Response<Body>, Refusal> {
        let catalog = self.inner.host.catalog(&lease.auth.id, agent).await?;
        let (body, labelled) = match catalog {
            Catalog::List(body) => (body, false),
            Catalog::Entries(body) => (body, true),
        };
        let mut response = Response::new(Body::full(body));
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        if labelled {
            headers.insert("x-nonstopvibin-profile", slug_value(&lease.auth)?);
        }
        Ok(response)
    }
}

fn slug_value(auth: &ProfileAuth) -> Result<HeaderValue, Refusal> {
    HeaderValue::from_str(auth.slug.as_str()).map_err(|_| Refusal::unavailable())
}

/// One fresh connection to the core, with Nagle off and hyper's buffer capped.
async fn handshake<B>(
    core: SocketAddr,
) -> Result<
    (
        client::SendRequest<B>,
        client::Connection<TokioIo<TcpStream>, B>,
    ),
    Refusal,
>
where
    B: hyper::body::Body + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    // Raw socket errors name local ports and internals; the agent needs only the remedy.
    let stream = TcpStream::connect(core)
        .await
        .map_err(|_| Refusal::unavailable())?;
    stream
        .set_nodelay(true)
        .map_err(|_| Refusal::unavailable())?;
    client::Builder::new()
        .max_buf_size(MAX_BUF_SIZE)
        .handshake(TokioIo::new(stream))
        .await
        .map_err(|_| Refusal::unavailable())
}

fn proxy_error(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
        .extensions_mut()
        .insert(hyper::ext::ReasonPhrase::from_static(b"Proxy error"));
    let headers = response.headers_mut();
    headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    response
}

/// Serves one accepted agent connection the way the gateway needs it: Nagle off, hyper's
/// buffer capped, upgrades enabled and half-closed connections refused, so an agent's FIN
/// ends its requests.
///
/// # Errors
/// When the socket option fails or the connection ends in an error; an agent hanging up
/// mid-response is a `hyper::Error` with `is_incomplete_message()` inside the I/O error.
pub async fn serve_connection<S, B>(stream: TcpStream, service: S) -> io::Result<()>
where
    S: Service<Request<Incoming>, Response = Response<B>>,
    S::Error: Into<Box<dyn Error + Send + Sync>>,
    B: hyper::body::Body + 'static,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    stream.set_nodelay(true)?;
    hyper::server::conn::http1::Builder::new()
        .max_buf_size(MAX_BUF_SIZE)
        .half_close(false)
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await
        .map_err(io::Error::other)
}
