//! The engine's HTTP side: `/claude-agent/<profile>/v1/messages` and `/mcp`, the profiles'
//! [`Sessions`], and the sign-in probe.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http_body_util::BodyExt;
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use nsv_domain::endpoint::claude_agent_base;
use nsv_domain::id::ProfileId;
use nsv_domain::secret::{ApiKey, McpToken};
use nsv_domain::sync::locked;
use nsv_domain::wire::ClaudeAgentStatus;
use nsv_files::DataDir;
use nsv_gateway::Body;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::conversation::{self, Conversation, Settings};
use crate::driver::{self, Driver, GRACE, Head, Request as Turn, Sink};
use crate::mcp;
use crate::process::{self, Found, Mode, Persistence};
use crate::sessions::{self, Answer, Paths, Plan, Sessions};
use crate::translate::{Failure, Offered, Translator};

/// The largest Messages request read: a long conversation with images.
const MESSAGES_LIMIT: usize = 64 << 20;
/// The largest MCP message read.
const MCP_LIMIT: usize = 4 << 20;
/// How often a held tool call tells Claude Code it is still running.
const PROGRESS: Duration = Duration::from_secs(30);
/// How long the sign-in probe may take.
const PROBE: Duration = Duration::from_secs(30);
/// The oldest Claude Code that delivers a user message flagged `client_composed` as written.
/// An older one expands `@path` mentions and runs slash commands in the text it gets, such as
/// a checkpoint's tool results.
const OLDEST: (u64, u64, u64) = (2, 1, 248);

/// A profile's Claude Agent account as the engine needs it.
#[derive(Debug, Clone)]
pub struct Account {
    /// The key the profile's core presents.
    pub key: ApiKey,
}

/// What the engine needs from the rest of the daemon.
pub trait Host: Send + Sync + 'static {
    /// The profile's Claude Agent account, if it has one.
    fn account(&self, profile: &ProfileId) -> impl Future<Output = Option<Account>> + Send;
}

/// Where the engine runs.
#[derive(Debug, Clone)]
pub struct Options {
    pub data: DataDir,
    /// The daemon's loopback port, where Claude Code reaches the MCP endpoint.
    pub port: u16,
    /// `claude` itself; `None` looks on `PATH` and where its installers put it.
    pub program: Option<PathBuf>,
}

/// What Claude Code's `initialize` answer says about its sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// Its version, `claude_code_version`; absent on a Claude Code too old to say.
    pub version: Option<String>,
    pub signed_in: bool,
    pub email: Option<String>,
    /// The models it offers, by full name, without repeats.
    pub models: Vec<String>,
}

/// Why Claude Code cannot serve a profile yet: an HTTP status and what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unready {
    pub status: u16,
    pub message: String,
}

impl Unready {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

/// Serves Claude Agent accounts; clones share it.
pub struct Engine<H> {
    inner: Arc<Inner<H>>,
}

impl<H> Clone for Engine<H> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct Inner<H> {
    host: H,
    options: Options,
    profiles: Mutex<HashMap<ProfileId, Arc<Sessions>>>,
    closed: AtomicBool,
}

/// An Anthropic error answer.
fn error(failure: &Failure) -> Response<Body> {
    let mut headers = HeaderMap::new();
    if let Some(at) = failure.retry_at {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        if let Ok(value) = HeaderValue::try_from(at.saturating_sub(now).max(1).to_string()) {
            headers.insert(header::RETRY_AFTER, value);
        }
    }
    let status = StatusCode::from_u16(failure.status).unwrap_or(StatusCode::BAD_GATEWAY);
    nsv_gateway::json(status, headers, failure.body().to_string())
}

fn refuse(status: u16, kind: &'static str, message: impl Into<String>) -> Response<Body> {
    error(&Failure::new(status, kind, message))
}

fn json(status: StatusCode, value: &Value) -> Response<Body> {
    nsv_gateway::json(status, HeaderMap::new(), value.to_string())
}

fn events(body: Body) -> Response<Body> {
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// The body, at most `limit` bytes; `None` when it is larger or the client hung up.
async fn read(request: Request<Incoming>, limit: usize) -> Option<Bytes> {
    let mut body = request.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.ok()?.into_data() {
            if bytes.len() + data.len() > limit {
                return None;
            }
            bytes.extend_from_slice(&data);
        }
    }
    Some(bytes.into())
}

/// `value` as one shell word.
fn quoted(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+:@%=,".contains(&b));
    if plain {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// The command that signs Claude Code in for one profile.
fn sign_in(found: Option<&Found>, config: &Path) -> String {
    let program = match found {
        Some(found) if !found.on_path => quoted(&found.path.to_string_lossy()),
        _ => "claude".to_owned(),
    };
    format!(
        "CLAUDE_CONFIG_DIR={} {program} auth login",
        quoted(&config.to_string_lossy())
    )
}

/// The profile and the rest of a path under `/claude-agent/`, if it is one.
fn target(path: &str) -> Option<(&str, &str)> {
    path.strip_prefix(nsv_domain::endpoint::CLAUDE_AGENT_ROUTES)?
        .split_once('/')
}

/// The path below `/claude-agent/<profile>`.
enum Target {
    Messages,
    Mcp,
}

impl<H: Host> Engine<H> {
    #[must_use]
    pub fn new(host: H, options: Options) -> Self {
        Self {
            inner: Arc::new(Inner {
                host,
                options,
                profiles: Mutex::default(),
                closed: AtomicBool::new(false),
            }),
        }
    }

    /// Answers a request for `/claude-agent/<profile>/<rest>` that passed the daemon's Host and
    /// Origin checks.
    pub async fn answer(&self, request: Request<Incoming>, path: &str) -> Response<Body> {
        let target = target(path).and_then(|(profile, rest)| {
            let profile = ProfileId::parse(profile).ok()?;
            let target = match rest {
                "v1/messages" => Target::Messages,
                "mcp" => Target::Mcp,
                _ => return None,
            };
            Some((profile, target))
        });
        let Some((profile, target)) = target else {
            return refuse(404, "not_found_error", "Not found.");
        };
        if self.inner.closed.load(Ordering::Acquire) {
            return refuse(503, "overloaded_error", "NonstopVibin is shutting down.");
        }
        match (target, request.method()) {
            (Target::Messages, &Method::POST) => self.messages(&profile, request).await,
            (Target::Mcp, &Method::POST) => self.mcp(&profile, request).await,
            _ => {
                let mut refused = refuse(405, "invalid_request_error", "Method not allowed.");
                refused
                    .headers_mut()
                    .insert(header::ALLOW, HeaderValue::from_static("POST"));
                refused
            }
        }
    }

    /// The account whose key `headers` present for `profile`.
    async fn authorized(&self, profile: &ProfileId, headers: &HeaderMap) -> Option<Account> {
        let presented = bearer(headers)?.to_owned();
        let account = self.inner.host.account(profile).await?;
        account.key.matches(&presented).then_some(account)
    }

    /// `claude` as the options name it or as found on this machine.
    async fn found(&self) -> Option<Found> {
        if let Some(path) = &self.inner.options.program {
            return Some(Found {
                path: path.clone(),
                on_path: false,
            });
        }
        tokio::task::spawn_blocking(|| {
            process::find(
                std::env::var_os("PATH").as_deref(),
                std::env::home_dir().as_deref(),
            )
        })
        .await
        .ok()
        .flatten()
    }

    /// The profile's sessions, with their directories; the first use sweeps what an earlier
    /// run of the daemon left.
    async fn sessions(&self, profile: &ProfileId) -> std::io::Result<Arc<Sessions>> {
        let sessions = Arc::clone(
            locked(&self.inner.profiles)
                .entry(profile.clone())
                .or_insert_with(|| {
                    let paths = Paths::new(&self.inner.options.data, profile);
                    Arc::new(Sessions::new(profile.clone(), paths))
                }),
        );
        sessions.prepare().await?;
        Ok(sessions)
    }

    async fn messages(&self, profile: &ProfileId, request: Request<Incoming>) -> Response<Body> {
        if self.authorized(profile, request.headers()).await.is_none() {
            return refuse(401, "authentication_error", "Invalid API key.");
        }
        let Some(body) = read(request, MESSAGES_LIMIT).await else {
            return refuse(413, "request_too_large", "The request is too large.");
        };
        let conversation = match conversation::parse(&body) {
            Ok(conversation) => conversation,
            Err(invalid) => return refuse(400, "invalid_request_error", invalid.0),
        };
        let sessions = match self.sessions(profile).await {
            Ok(sessions) => sessions,
            Err(cause) => {
                tracing::warn!(%cause, "Claude Agent could not prepare its directories");
                return refuse(
                    502,
                    "api_error",
                    "Claude Agent could not prepare its files.",
                );
            }
        };
        let plan = match sessions.claim(&conversation).await {
            Ok(plan) => plan,
            Err(sessions::Closed) => {
                return refuse(
                    503,
                    "overloaded_error",
                    "Claude Agent stopped serving this profile.",
                );
            }
        };
        self.serve(&sessions, plan, &conversation).await
    }

    /// Hands the claimed process its turn, then answers with what it writes.
    async fn serve(
        &self,
        sessions: &Arc<Sessions>,
        plan: Plan,
        conversation: &Conversation,
    ) -> Response<Body> {
        let (head, decided) = oneshot::channel();
        let (body, received) = mpsc::channel(64);
        let sink = Sink {
            head: Some(head),
            body,
            stream: conversation.stream,
        };
        let tail = conversation.tail_digest();
        let (key, requests, turn, results) = match plan {
            Plan::Continue {
                key,
                requests,
                results,
            } => (key, requests, Turn::Continue { sink, tail }, results),
            Plan::Append {
                key,
                requests,
                content,
            } => {
                let turn = Turn::Turn {
                    content,
                    sink,
                    tail,
                };
                (key, requests, turn, Vec::new())
            }
            Plan::Start {
                key,
                persistence,
                content,
                token,
                previous,
            } => {
                let starting = Starting {
                    sessions,
                    key,
                    done: false,
                };
                // Only a new process needs claude: a live one goes on if it was removed since.
                let Some(found) = self.found().await else {
                    return refuse(
                        502,
                        "api_error",
                        "Claude Code (claude) was not found. Install it, sign it in for this profile, then try again.",
                    );
                };
                if let Some(previous) = previous {
                    let _ = previous.await;
                }
                let first = Turn::Turn {
                    content,
                    sink,
                    tail,
                };
                let settings = &conversation.settings;
                let started = self
                    .start(&starting, &found, settings, persistence, &token, first)
                    .await;
                if let Err(cause) = started {
                    tracing::warn!(%cause, "Claude Code could not be started");
                    return refuse(502, "api_error", "Claude Code could not be started.");
                }
                starting.handed_over();
                return answer(decided, received).await;
            }
        };
        // The session was idle or holding, so its one-request queue is empty.
        if requests.try_send(turn).is_err() {
            sessions.ended(key);
            return refuse(502, "api_error", "Claude Code stopped.");
        }
        sessions.deliver(key, results);
        answer(decided, received).await
    }

    /// Starts `claude` for the claimed process and hands it its first turn.
    async fn start(
        &self,
        starting: &Starting<'_>,
        found: &Found,
        settings: &Settings,
        persistence: Persistence,
        token: &McpToken,
        first: Turn,
    ) -> std::io::Result<()> {
        let sessions = starting.sessions;
        let paths = &sessions.paths;
        let key = starting.key;
        let system = paths.run.join(format!("{key}.system"));
        let mcp = (!settings.tools.is_empty()).then(|| paths.run.join(format!("{key}.mcp.json")));
        let mut writes = vec![(system.clone(), settings.system.clone().into_bytes())];
        if let Some(mcp) = &mcp {
            let config = json!({ "mcpServers": { "client": {
                "type": "http",
                "url": format!(
                    "{}/mcp",
                    claude_agent_base(self.inner.options.port, &sessions.profile)
                ),
                "headers": { "Authorization": format!("Bearer {}", token.expose_secret()) },
            } } });
            writes.push((mcp.clone(), config.to_string().into_bytes()));
        }
        let files: Vec<PathBuf> = writes.iter().map(|(path, _)| path.clone()).collect();
        let written = tokio::task::spawn_blocking(move || {
            writes
                .iter()
                .try_for_each(|(path, bytes)| nsv_files::write_atomic_unsynced(path, bytes, 0o600))
        })
        .await
        .map_err(std::io::Error::other)
        .and_then(|written| written);
        let arguments = process::arguments(&Mode::Turns {
            settings,
            system: &system,
            mcp: mcp.as_deref(),
            persistence: &persistence,
        });
        let environment = process::environment(&paths.config, settings.max_tokens, |name| {
            std::env::var_os(name)
        });
        let spawned = written.and_then(|()| {
            process::command(&found.path, &paths.work, &arguments, environment).spawn()
        });
        let child = match spawned {
            Ok(child) => child,
            Err(error) => {
                tokio::task::spawn_blocking(move || sessions::remove(&files));
                return Err(error);
            }
        };
        let offered = Arc::new(Offered::new(&settings.tools));
        let driver = Driver {
            sessions: Arc::clone(sessions),
            key,
            child,
            translator: Translator::new(offered, Arc::from(sign_in(Some(found), &paths.config))),
            files,
            session: persistence.session().map(str::to_owned),
            turning: false,
        };
        let (requests, received) = mpsc::channel(1);
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(driver.run(received, stopped));
        if !sessions.started(key, requests.clone(), stop, task) {
            return Err(std::io::Error::other(
                "the process was stopped while starting",
            ));
        }
        requests
            .try_send(first)
            .map_err(|_| std::io::Error::other("Claude Code exited at once"))
    }

    async fn mcp(&self, profile: &ProfileId, request: Request<Incoming>) -> Response<Body> {
        let sessions = locked(&self.inner.profiles).get(profile).cloned();
        let found = bearer(request.headers()).and_then(|token| {
            sessions
                .as_ref()
                .and_then(|sessions| sessions.by_token(token))
        });
        let (Some(sessions), Some((key, tools))) = (sessions, found) else {
            return json(
                StatusCode::UNAUTHORIZED,
                &json!({ "error": "Invalid token." }),
            );
        };
        let message = read(request, MCP_LIMIT)
            .await
            .and_then(|body| mcp::parse(&body));
        let Some(message) = message else {
            return json(
                StatusCode::BAD_REQUEST,
                &json!({ "error": "Not a JSON-RPC message." }),
            );
        };
        let (id, method, params) = match message {
            mcp::Message::Notification { method, params } => {
                if method == "notifications/cancelled"
                    && let Some(request) = mcp::cancelled(&params)
                {
                    sessions.cancel(key, request);
                }
                return nsv_gateway::json(StatusCode::ACCEPTED, HeaderMap::new(), Bytes::new());
            }
            mcp::Message::Request { id, method, params } => (id, method, params),
        };
        let answered = match method.as_str() {
            "initialize" => mcp::initialized(&params),
            "ping" => json!({}),
            "tools/list" => mcp::tools(&tools),
            "tools/call" => {
                let Some(call) = mcp::call(&params) else {
                    return json(StatusCode::OK, &mcp::invalid(&id));
                };
                return match sessions.answer(key, &call.tool_use, &id) {
                    Some(Answer::Now(result)) => events(Body::full(mcp::event(&mcp::result(
                        &id,
                        &mcp::outcome(&result),
                    )))),
                    Some(Answer::Later(waiting)) => hold(id, call.progress, waiting),
                    None => json(StatusCode::NOT_FOUND, &json!({ "error": "Session ended." })),
                };
            }
            other => return json(StatusCode::OK, &mcp::not_found(&id, other)),
        };
        json(StatusCode::OK, &mcp::result(&id, &answered))
    }

    /// What Claude Code's `initialize` answer says for `profile`, without an API call, and
    /// the command that signs it in.
    async fn probe(&self, profile: &ProfileId) -> (Option<Found>, String, Result<Probe, Unready>) {
        let found = self.found().await;
        let config = self.inner.options.data.claude_agent_config(profile);
        let command = sign_in(found.as_ref(), &config);
        let Some(program) = found.as_ref().map(|found| found.path.clone()) else {
            return (
                None,
                command,
                Err(Unready::new(
                    409,
                    "Claude Code (claude) was not found. Install it first.",
                )),
            );
        };
        let probed = match self.sessions(profile).await {
            Ok(sessions) => probe(&program, &sessions.paths)
                .await
                .ok_or_else(|| {
                    Unready::new(
                        502,
                        "Claude Code did not answer. Run claude once in a terminal, then try again.",
                    )
                })
                .and_then(recent),
            Err(_) => Err(Unready::new(
                500,
                "Claude Agent could not prepare its files.",
            )),
        };
        (found, command, probed)
    }

    /// Claude Code's sign-in and models for `profile`, once it can serve it.
    ///
    /// # Errors
    /// 409 when `claude` is missing, not signed in or offers no models; 502 when it does not
    /// answer.
    pub async fn ready(&self, profile: &ProfileId) -> Result<Probe, Unready> {
        let (_, command, probed) = self.probe(profile).await;
        let probe = probed?;
        if !probe.signed_in {
            return Err(Unready::new(
                409,
                format!("Sign Claude Code in for this profile first: {command}"),
            ));
        }
        if probe.models.is_empty() {
            return Err(Unready::new(
                409,
                "Claude Code offers no models for this sign-in.",
            ));
        }
        Ok(probe)
    }

    /// What the UI shows before connecting: where `claude` is, the sign-in command, and the
    /// sign-in state.
    pub async fn status(&self, profile: &ProfileId) -> ClaudeAgentStatus {
        let (found, sign_in, probed) = self.probe(profile).await;
        let (signed_in, error) = match (&found, probed) {
            (Some(_), Ok(probe)) => (probe.signed_in, None),
            (Some(_), Err(unready)) => (false, Some(unready.message)),
            (None, _) => (false, None),
        };
        ClaudeAgentStatus {
            claude: found.map(|found| found.path.to_string_lossy().into_owned()),
            sign_in,
            signed_in,
            error,
        }
    }

    /// Stops the profile's processes and forgets its sessions, as when its account goes.
    pub async fn forget(&self, profile: &ProfileId) {
        let sessions = locked(&self.inner.profiles).remove(profile);
        for task in sessions.iter().flat_map(|sessions| sessions.close()) {
            let _ = task.await;
        }
    }

    /// Stops every process, every profile's at once; requests after this are refused.
    pub async fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        let tasks: Vec<_> = locked(&self.inner.profiles)
            .values()
            .flat_map(|sessions| sessions.close())
            .collect();
        for task in tasks {
            let _ = task.await;
        }
    }
}

/// A claimed process not yet handed to its driver: if the request is dropped first, the
/// claim goes.
struct Starting<'a> {
    sessions: &'a Arc<Sessions>,
    key: u64,
    done: bool,
}

impl Starting<'_> {
    fn handed_over(mut self) {
        self.done = true;
    }
}

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.sessions.ended(self.key);
        }
    }
}

/// The response once its head is decided. While it waits, the body channel's receiver lives
/// here, so a client that hangs up closes it.
async fn answer(heard: oneshot::Receiver<Head>, received: mpsc::Receiver<Bytes>) -> Response<Body> {
    match heard.await {
        Ok(Head::Stream) => events(Body::channel(received)),
        Ok(Head::Json(body)) => nsv_gateway::json(StatusCode::OK, HeaderMap::new(), body),
        Ok(Head::Error(failure)) => error(&failure),
        Err(_) => refuse(502, "api_error", "Claude Code stopped before it answered."),
    }
}

/// A `tools/call` held until the harness answers it: an event stream that reports progress
/// every [`PROGRESS`], then carries the answer. It ends without one if the call is cancelled.
fn hold(
    id: Value,
    progress: Option<Value>,
    waiting: oneshot::Receiver<conversation::ToolResult>,
) -> Response<Body> {
    let (send, body) = mpsc::channel(4);
    tokio::spawn(async move {
        let mut waiting = waiting;
        let mut seconds = 0;
        loop {
            tokio::select! {
                answered = &mut waiting => {
                    if let Ok(result) = answered {
                        let _ = send.send(mcp::event(&mcp::result(&id, &mcp::outcome(&result)))).await;
                    }
                    return;
                }
                () = tokio::time::sleep(PROGRESS) => {
                    seconds += PROGRESS.as_secs();
                    if let Some(token) = &progress
                        && send.send(mcp::event(&mcp::progress(token, seconds))).await.is_err()
                    {
                        return;
                    }
                }
                () = send.closed() => return,
            }
        }
    });
    events(Body::channel(body))
}

/// Starts `claude` only to read its `initialize` answer.
async fn probe(program: &Path, paths: &Paths) -> Option<Probe> {
    let arguments = process::arguments(&Mode::Probe);
    let environment = process::environment(&paths.config, None, |name| std::env::var_os(name));
    let mut child = process::command(program, &paths.work, &arguments, environment)
        .spawn()
        .ok()?;
    let (mut stdin, stdout) = (child.stdin.take()?, child.stdout.take()?);
    let mut lines = BufReader::new(stdout).lines();
    let asked = async {
        stdin
            .write_all(b"{\"type\":\"control_request\",\"request_id\":\"initialize\",\"request\":{\"subtype\":\"initialize\"}}\n")
            .await
            .ok()?;
        driver::control(&mut lines, "initialize").await
    };
    let answered = tokio::time::timeout(PROBE, asked).await.ok().flatten();
    drop(stdin);
    if tokio::time::timeout(GRACE, child.wait()).await.is_err() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    read_probe(&answered?)
}

/// `probe` if its Claude Code is at least [`OLDEST`].
fn recent(probe: Probe) -> Result<Probe, Unready> {
    let parsed = probe.version.as_deref().and_then(|version| {
        let mut parts = version.split('.').map(str::parse::<u64>);
        match (parts.next(), parts.next(), parts.next()) {
            (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch))) => Some((major, minor, patch)),
            _ => None,
        }
    });
    if parsed.is_some_and(|version| version >= OLDEST) {
        return Ok(probe);
    }
    let (major, minor, patch) = OLDEST;
    let claude = probe.version.map_or_else(
        || "This Claude Code".to_owned(),
        |version| format!("Claude Code {version}"),
    );
    Err(Unready::new(
        409,
        format!(
            "{claude} is too old for Claude Agent, which needs {major}.{minor}.{patch} or newer. Update it, then try again."
        ),
    ))
}

/// The sign-in and models of an `initialize` control response.
fn read_probe(response: &Value) -> Option<Probe> {
    (response.get("subtype").and_then(Value::as_str) == Some("success")).then_some(())?;
    let answer = response.get("response")?;
    let account = answer.get("account");
    let field = |name: &str| {
        account
            .and_then(|account| account.get(name))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let token_source = field("tokenSource").filter(|source| source != "none");
    let mut models: Vec<String> = Vec::new();
    for model in answer
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = model.get("resolvedModel").and_then(Value::as_str)
            && !models.iter().any(|known| known == id)
        {
            models.push(id.to_owned());
        }
    }
    Some(Probe {
        version: answer
            .get("claude_code_version")
            .and_then(Value::as_str)
            .map(str::to_owned),
        signed_in: token_source.is_some() || field("apiKeySource").is_some(),
        email: field("email"),
        models,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_initialize_answer_gives_the_sign_in_and_each_model_once() {
        let response = json!({
            "subtype": "success",
            "request_id": "initialize",
            "response": {
                "models": [
                    { "value": "default", "resolvedModel": "claude-opus-5-5" },
                    { "value": "opus", "resolvedModel": "claude-opus-5-5" },
                    { "value": "sonnet", "resolvedModel": "claude-sonnet-5-5" },
                ],
                "account": { "tokenSource": "claude.ai", "email": "me@example.com" },
                "claude_code_version": "2.1.326",
            },
        });
        assert_eq!(
            read_probe(&response),
            Some(Probe {
                version: Some("2.1.326".to_owned()),
                signed_in: true,
                email: Some("me@example.com".to_owned()),
                models: vec!["claude-opus-5-5".to_owned(), "claude-sonnet-5-5".to_owned()],
            })
        );
        let signed_out = json!({
            "subtype": "success",
            "response": { "models": [], "account": { "tokenSource": "none", "apiProvider": "firstParty" } },
        });
        assert_eq!(
            read_probe(&signed_out),
            Some(Probe {
                version: None,
                signed_in: false,
                email: None,
                models: Vec::new(),
            })
        );
        assert_eq!(
            read_probe(&json!({ "subtype": "error", "error": "x" })),
            None
        );
    }

    #[test]
    fn claude_code_before_verbatim_prompts_is_too_old() {
        let probe = |version: Option<&str>| Probe {
            version: version.map(str::to_owned),
            signed_in: true,
            email: None,
            models: Vec::new(),
        };
        for version in ["2.1.248", "2.1.1000", "2.2.0", "3.0.0"] {
            assert!(recent(probe(Some(version))).is_ok(), "{version}");
        }
        for version in [Some("2.1.247"), Some("2.0.999"), Some("2.1"), None] {
            assert_eq!(
                recent(probe(version)).map_err(|unready| unready.status),
                Err(409)
            );
        }
        assert_eq!(
            recent(probe(None)).unwrap_err().message,
            "This Claude Code is too old for Claude Agent, which needs 2.1.248 or newer. Update it, then try again."
        );
    }

    #[test]
    fn the_sign_in_command_quotes_what_a_shell_would_split() {
        let on_path = Found {
            path: PathBuf::from("/usr/bin/claude"),
            on_path: true,
        };
        assert_eq!(
            sign_in(
                Some(&on_path),
                Path::new("/home/u/.nonstopvibin/claude-agent/p")
            ),
            "CLAUDE_CONFIG_DIR=/home/u/.nonstopvibin/claude-agent/p claude auth login"
        );
        let local = Found {
            path: PathBuf::from("/home/u/.local/bin/claude"),
            on_path: false,
        };
        assert_eq!(
            sign_in(
                Some(&local),
                Path::new("/Users/me/Library/Application Support/x's")
            ),
            r"CLAUDE_CONFIG_DIR='/Users/me/Library/Application Support/x'\''s' /home/u/.local/bin/claude auth login"
        );
    }

    #[test]
    fn engine_paths_name_a_profile_and_the_rest() {
        assert_eq!(
            target("/claude-agent/p-1/v1/messages"),
            Some(("p-1", "v1/messages"))
        );
        assert_eq!(target("/claude-agent/p-1"), None);
        assert_eq!(target("/p/slug/v1/messages"), None);
    }
}
