//! The listener and the checks every request passes before its handler: shutdown, Host, the
//! upgrade path, Origin and the session token for `/api`, and the loopback-only agent gateway.

use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use nsv_accounts::catalog::{ModelCatalog, profile_models};
use nsv_accounts::fetch::Fetch;
use nsv_accounts::scheduler::Scheduler;
use nsv_agents::{catalog_entries, offered_models};
use nsv_core::install::Releases;
use nsv_domain::endpoint::{LOOPBACK_HOST, PROFILE_ROUTES};
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::registry::{Agent, registry};
use nsv_domain::secret::KeyDigest;
use nsv_domain::sync::locked;
use nsv_domain::wire::{CatalogModel, Model};
use nsv_gateway::{Activity, Body, Catalog, Host, ProfileAuth, Refusal, serve_connection};
use nsv_store::{AuthSnapshot, Store};
use serde::Serialize;
use serde_json::json;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::daemon::{App, Shared};
use crate::{failure, panic};

/// The Vite development server, whose Host and Origin a development run accepts.
pub const DEV_HOST: &str = "127.0.0.1:5173";

/// Whether the listener accepts, and whether open connections may finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Serving,
    /// No new connections; each open one finishes its response in progress and closes.
    Draining,
    /// Connections still open are dropped.
    Closed,
}

/// An exact DNS name such as a Tailscale Serve host: lowercase labels, at least one dot, an
/// alphabetic top-level label (so no IP literal), no port or wildcard, at most 253 characters.
#[must_use]
pub fn valid_allow_host(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    let label = |text: &str| {
        (1..=63).contains(&text.len())
            && text
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !text.starts_with('-')
            && !text.ends_with('-')
    };
    host.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|text| label(text))
        && labels
            .last()
            .is_some_and(|top| top.starts_with(|c: char| c.is_ascii_lowercase()))
}

/// Management responses: JSON, never cached, never sniffed.
pub(crate) fn json<T: Serialize>(
    status: StatusCode,
    value: &T,
) -> Result<Response<Body>, AppError> {
    let body = serde_json::to_vec(value).map_err(AppError::internal)?;
    let mut response = Response::new(Body::full(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
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

/// An empty answer that closes the connection, as the upgrade path wrote to its socket.
fn closed(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    response
}

/// A browser Origin: an empty header counts as none, as it did in Node.
fn origin(request: &Request<Incoming>) -> Option<Cow<'_, str>> {
    let value = request.headers().get(header::ORIGIN)?;
    if value.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(value.as_bytes()))
}

impl<F: Fetch + Releases + Clone + 'static> App<F> {
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
        host.is_some_and(|host| self.allow_hosts.iter().any(|allowed| allowed == host))
    }

    async fn dispatch(self: Arc<Self>, request: Request<Incoming>) -> Response<Body> {
        match switch(request.headers()) {
            Switch::None => {}
            Switch::WebSocket => return self.upgrade(request).await,
            // The upgrade path sends a bodiless GET, which would drop this request's method
            // and body; main's core answered it with an error too.
            Switch::Other => return closed(StatusCode::BAD_REQUEST),
        }
        match self.handle(request).await {
            Ok(response) => response,
            Err(error) => failure::refusal(error, &self.shared.errors).into_response(),
        }
    }

    /// The WebSocket path, as Node's `upgrade` event: a valid loopback Host and no Origin.
    async fn upgrade(&self, request: Request<Incoming>) -> Response<Body> {
        let host = text(&request, header::HOST);
        if !self.valid_host(host) || self.allowed_host(host) || origin(&request).is_some() {
            return closed(StatusCode::FORBIDDEN);
        }
        self.gateway.upgrade(request).await
    }

    async fn handle(&self, request: Request<Incoming>) -> Result<Response<Body>, AppError> {
        if self.shared.closing() {
            return Err(AppError::shutting_down());
        }
        let host = text(&request, header::HOST);
        if !self.valid_host(host) {
            return Err(AppError::new("Unrecognized host.", 403));
        }
        let allowed = self.allowed_host(host);
        // The path as WHATWG URL parsing sees it, as `new URL(req.url, origin)` did.
        let url = self
            .base
            .join(&request.uri().to_string())
            .map_err(|_| AppError::new("Malformed request path.", 400))?;
        let path = url.path();
        if path.starts_with("/api/") {
            if let Some(origin) = origin(&request) {
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
            let authorized = text(&request, header::AUTHORIZATION)
                .and_then(|presented| presented.strip_prefix("Bearer "))
                .is_some_and(|presented| self.token.matches(presented));
            if !authorized {
                return Err(AppError::new(
                    "Open NonstopVibin from the desktop app, or run nonstopvibin url on the server for a new session link.",
                    401,
                ));
            }
            return self.api(request, &url).await;
        }
        if path.starts_with("/v1") || path.starts_with(PROFILE_ROUTES) {
            if allowed {
                return Err(AppError::new(
                    "The agent API is only available on the loopback address.",
                    403,
                ));
            }
            if origin(&request).is_some() {
                return Err(AppError::new(
                    "The agent API accepts local agent clients, not browser origins.",
                    403,
                ));
            }
            return Ok(self
                .gateway
                .proxy(request)
                .await
                .unwrap_or_else(Refusal::into_response));
        }
        if request.method() != hyper::Method::GET && request.method() != hyper::Method::HEAD {
            return Err(AppError::new("Method not allowed.", 405));
        }
        let head = request.method() == hyper::Method::HEAD;
        let path = path.to_owned();
        self.asset(&request, &path, head).await
    }
}

/// Accepts until shutdown begins; each connection serves its requests in a task of its own.
pub(crate) fn serve<F: Fetch + Releases + Clone + 'static>(
    app: &Arc<App<F>>,
    listener: TcpListener,
) {
    let app = Arc::clone(app);
    tokio::spawn(async move {
        let mut phase = app.phase.subscribe();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => connection(&app, stream),
                    Err(error) => {
                        // Out of descriptors, for one: try again shortly rather than spin.
                        tracing::warn!("could not accept a connection: {error}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
                () = draining(&mut phase) => return,
            }
        }
    });
}

/// Resolves once shutdown began.
async fn draining(phase: &mut watch::Receiver<Phase>) {
    // The sender lives as long as the daemon; without it, there is nothing left to serve.
    let _ = phase.wait_for(|phase| *phase != Phase::Serving).await;
}

fn connection<F: Fetch + Releases + Clone + 'static>(app: &Arc<App<F>>, stream: TcpStream) {
    let served = Arc::clone(app);
    let service = service_fn(move |request| {
        let app = Arc::clone(&served);
        async move { Ok::<_, Infallible>(app.dispatch(request).await) }
    });
    let mut drain = app.phase.subscribe();
    let mut closed = app.phase.subscribe();
    tokio::spawn(panic::request(async move {
        tokio::select! {
            // A failed connection is the client's: a hang-up, a bad request or a timeout.
            _ = serve_connection(stream, service, draining(&mut drain)) => {}
            _ = closed.wait_for(|phase| *phase == Phase::Closed) => {}
        }
    }));
}

/// One auth snapshot as the gateway takes it, shared per request instead of copied.
pub(crate) struct GatewayProfiles {
    source: Arc<AuthSnapshot>,
    entries: HashMap<KeyDigest, Arc<ProfileAuth>>,
}

impl GatewayProfiles {
    /// A store that could not read the keys has no entries: every key fails closed.
    fn of(source: Arc<AuthSnapshot>) -> Self {
        let entries = source
            .entries()
            .into_iter()
            .flatten()
            .filter_map(|(digest, entry)| {
                let auth = ProfileAuth::new(
                    entry.profile.clone(),
                    entry.slug.clone(),
                    entry.client_key.clone(),
                    &entry.core_key,
                )?;
                Some((*digest, Arc::new(auth)))
            })
            .collect();
        Self { source, entries }
    }
}

/// The gateway's view of the daemon: the store's key snapshot, the scheduler's wake and the
/// agents' curated catalogs. It holds no management key.
pub(crate) struct GatewayHost<F> {
    pub(crate) shared: Arc<Shared>,
    pub(crate) auth: watch::Receiver<Arc<AuthSnapshot>>,
    /// The gateway's entries for the store's latest snapshot, built when it changes.
    pub(crate) profiles: Mutex<Option<GatewayProfiles>>,
    pub(crate) scheduler: Scheduler,
    pub(crate) catalog: Arc<ModelCatalog<F>>,
    pub(crate) store: Store,
}

impl<F: Fetch + Clone + 'static> GatewayHost<F> {
    async fn curated(&self, profile: &ProfileId, agent: Agent) -> Result<Catalog, AppError> {
        let models = self.scheduler.pool().models(profile).await?;
        let enriched = if registry().curated(agent) {
            None
        } else {
            Some(enrich(&self.catalog, &self.store, profile, &models).await?)
        };
        let offered = offered_models(agent, models, false, |_| async {
            enriched.unwrap_or_default()
        })
        .await?;
        // The agent's own request gets the OpenAI list; generated code gets its entries.
        if let Some(entries) = catalog_entries(agent, &offered)? {
            return Ok(Catalog::Entries(
                json!({ "models": entries }).to_string().into(),
            ));
        }
        let data: Vec<serde_json::Value> = offered
            .iter()
            .map(|model| {
                let mut entry = json!({ "object": "model" });
                if let (Some(entry), Ok(serde_json::Value::Object(fields))) =
                    (entry.as_object_mut(), serde_json::to_value(model))
                {
                    entry.extend(fields);
                }
                entry
            })
            .collect();
        Ok(Catalog::List(
            json!({ "object": "list", "data": data }).to_string().into(),
        ))
    }
}

/// A profile's models with models.dev metadata matched through its API accounts.
pub(crate) async fn enrich<F: Fetch + 'static>(
    catalog: &ModelCatalog<F>,
    store: &Store,
    profile: &ProfileId,
    models: &[Model],
) -> Result<Vec<CatalogModel>, AppError> {
    Ok(profile_models(catalog, store, profile, models).await?)
}

impl<F: Fetch + Clone + 'static> Host for GatewayHost<F> {
    fn profile(&self, digest: &KeyDigest) -> Option<Arc<ProfileAuth>> {
        // The current snapshot on every request, so a deleted or rotated key fails at once.
        let snapshot = Arc::clone(&self.auth.borrow());
        let mut profiles = locked(&self.profiles);
        if !profiles
            .as_ref()
            .is_some_and(|profiles| Arc::ptr_eq(&profiles.source, &snapshot))
        {
            *profiles = Some(GatewayProfiles::of(snapshot));
        }
        profiles.as_ref()?.entries.get(digest).cloned()
    }

    async fn wake(&self, profile: &ProfileId) -> Result<SocketAddr, Refusal> {
        self.scheduler
            .wake(profile)
            .await
            .map_err(|error| failure::refusal(error, &self.shared.errors))?;
        let port = self
            .scheduler
            .pool()
            .port(profile)
            .map_err(|error| failure::refusal(error, &self.shared.errors))?;
        Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
    }

    async fn catalog(&self, profile: &ProfileId, agent: Agent) -> Result<Catalog, Refusal> {
        self.curated(profile, agent)
            .await
            .map_err(|error| failure::refusal(error, &self.shared.errors))
    }

    fn activity(&self, profile: &ProfileId, activity: Activity) {
        let open = match activity {
            Activity::Opened { .. } => true,
            Activity::Closed { last } => !last,
        };
        self.scheduler.activity(profile, open);
        let edge = matches!(
            activity,
            Activity::Opened { first: true } | Activity::Closed { last: true }
        );
        if self.shared.desktop && edge {
            self.shared.tray.opened(profile, open);
            self.shared
                .tray
                .update(&self.scheduler.pool().state(), &self.scheduler);
        }
    }
}
