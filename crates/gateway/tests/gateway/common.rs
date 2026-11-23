//! Shared test pieces: a `Host` over fixed profiles, the gateway on a loopback listener with
//! the server's error rendering, and a raw HTTP/1.1 client like `tests/golden/areas/gateway.ts`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hyper::body::Incoming;
use hyper::header;
use hyper::service::service_fn;
use hyper::{Request, Response};
use nsv_domain::id::{ProfileId, ProfileSlug};
use nsv_domain::registry::Agent;
use nsv_domain::secret::{ClientKey, CoreKey, KeyDigest};
use nsv_gateway::{Activity, Body, Catalog, Gateway, Host, ProfileAuth, Refusal, serve_connection};
use nsv_testkit::dechunk;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub(crate) const WORK: &str = "11111111-1111-4111-8111-111111111111";

pub(crate) fn key(letter: char) -> String {
    format!("nv_{}", letter.to_string().repeat(43))
}

pub(crate) fn profile(id: &str, slug: &str, key: &str) -> Arc<ProfileAuth> {
    Arc::new(
        ProfileAuth::new(
            ProfileId::parse(id).unwrap(),
            ProfileSlug::parse(slug).unwrap(),
            ClientKey::new(key),
            &CoreKey::new(format!("core-{slug}")),
        )
        .unwrap(),
    )
}

/// How `wake` answers.
#[derive(Clone, Copy)]
pub(crate) enum Wake {
    At(SocketAddr),
    Never,
}

pub(crate) struct TestHost {
    pub(crate) index: HashMap<KeyDigest, Arc<ProfileAuth>>,
    pub(crate) wake: Wake,
    pub(crate) catalogs: HashMap<(String, Agent), Result<Catalog, Refusal>>,
    pub(crate) activity: Arc<Mutex<Vec<(String, Activity)>>>,
}

impl TestHost {
    /// The work profile alone, its core at `core`.
    pub(crate) fn work(wake: Wake) -> Self {
        let work = profile(WORK, "work", &key('a'));
        Self {
            index: HashMap::from([(KeyDigest::of(&key('a')), work)]),
            wake,
            catalogs: HashMap::new(),
            activity: Arc::default(),
        }
    }
}

impl Host for TestHost {
    fn profile(&self, digest: &KeyDigest) -> Option<Arc<ProfileAuth>> {
        self.index.get(digest).cloned()
    }

    async fn wake(&self, _profile: &ProfileId) -> Result<SocketAddr, Refusal> {
        match self.wake {
            Wake::At(address) => Ok(address),
            Wake::Never => std::future::pending().await,
        }
    }

    fn catalog(
        &self,
        profile: &ProfileId,
        agent: Agent,
    ) -> impl Future<Output = Result<Catalog, Refusal>> + Send {
        std::future::ready(self.catalogs[&(profile.to_string(), agent)].clone())
    }

    fn activity(&self, profile: &ProfileId, activity: Activity) {
        self.activity
            .lock()
            .unwrap()
            .push((profile.to_string(), activity));
    }
}

async fn dispatch(gateway: Gateway<TestHost>, request: Request<Incoming>) -> Response<Body> {
    if request.headers().contains_key(header::UPGRADE) {
        gateway.upgrade(request).await
    } else {
        gateway
            .proxy(request)
            .await
            .unwrap_or_else(Refusal::into_response)
    }
}

/// The gateway on a loopback port, served as the server will serve it.
pub(crate) async fn spawn_gateway(host: TestHost) -> (SocketAddr, Gateway<TestHost>) {
    let gateway = Gateway::new(host);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served = gateway.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let gateway = served.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let gateway = gateway.clone();
                    async move { Ok::<_, Infallible>(dispatch(gateway, request).await) }
                });
                // Agents hanging up mid-response are part of these tests.
                let _served = serve_connection(stream, service, std::future::pending()).await;
            });
        }
    });
    (address, gateway)
}

pub(crate) type Header = (String, String);

pub(crate) struct RawRequest {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) headers: Vec<Header>,
    pub(crate) body: String,
}

/// The bytes `send` in `tests/golden/areas/gateway.ts` writes.
pub(crate) fn encode(port: u16, request: &RawRequest) -> Vec<u8> {
    let has = |name: &str| {
        request
            .headers
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case(name))
    };
    let mut out = format!("{} {} HTTP/1.1\r\n", request.method, request.target);
    if !has("host") {
        write!(out, "Host: 127.0.0.1:{port}\r\n").unwrap();
    }
    for (key, value) in &request.headers {
        write!(out, "{key}: {value}\r\n").unwrap();
    }
    if !request.body.is_empty() && !has("transfer-encoding") {
        write!(out, "Content-Length: {}\r\n", request.body.len()).unwrap();
    }
    out.push_str("Connection: close\r\n\r\n");
    out.push_str(&request.body);
    out.into_bytes()
}

pub(crate) struct RawResponse {
    pub(crate) status: u16,
    pub(crate) reason: String,
    /// Lowercase names, in order.
    pub(crate) headers: Vec<Header>,
    pub(crate) body: Vec<u8>,
}

impl RawResponse {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Splits a response head from its body, without de-chunking.
pub(crate) fn parse_head(raw: &[u8]) -> Option<(RawResponse, usize)> {
    let end = raw.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let mut lines = head.split("\r\n");
    let status_line = lines.next()?;
    let mut parts = status_line.splitn(3, ' ');
    let _version = parts.next();
    let status = parts.next()?.parse().ok()?;
    let reason = parts.next().unwrap_or_default().to_owned();
    let headers = lines
        .map(|line| {
            let (key, value) = line.split_once(':').unwrap_or((line, ""));
            (key.to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect();
    Some((
        RawResponse {
            status,
            reason,
            headers,
            body: Vec::new(),
        },
        end + 4,
    ))
}

/// Sends one request on a new connection and reads the response to EOF.
pub(crate) async fn send(address: SocketAddr, request: &RawRequest) -> RawResponse {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(&encode(address.port(), request))
        .await
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let (mut response, start) = parse_head(&raw).unwrap();
    let body = &raw[start..];
    response.body = if response.header("transfer-encoding") == Some("chunked") {
        dechunk(body)
    } else {
        body.to_vec()
    };
    response
}

/// Polls `check` every millisecond until it holds, or panics after `limit`.
pub(crate) async fn within(limit: Duration, what: &str, check: impl Fn() -> bool) -> Duration {
    let start = Instant::now();
    while !check() {
        assert!(start.elapsed() < limit, "{what} took over {limit:?}");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    start.elapsed()
}
