//! In-process fake cores behind the pool's [`Backend`], on tokio's clock, paused or not. A fake
//! core reads the `config.yaml` it was started with as the real one does (port, keys, auth
//! directory, routing), answers the management API with its management key, keeps OAuth
//! credentials as files in its auth directory and a usage queue in its own memory, and exits
//! on SIGTERM after a scripted delay. A browser sign-in finishes when its callback arrives, and
//! leaves the scripted credential behind. No provider is ever contacted.
#![expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::missing_panics_doc,
    clippy::unwrap_used,
    reason = "test support: a panic fails the test that called it, and is its report"
)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use nsv_core::pool::{Backend, BoxFuture, Call, CoreHandle};
use nsv_core::processes::KILL_GRACE;
use nsv_domain::id::ProfileId;
use nsv_files::DataDir;
use nsv_net::{HeaderMap, Method, NetError, Response, StatusCode};
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use url::Url;

/// How one launch of a profile's core behaves.
#[derive(Debug, Clone)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent behaviours a test scripts one at a time"
)]
pub struct Script {
    /// The executable cannot be started.
    pub launch_error: bool,
    /// When it starts answering its management API; `None`: never.
    pub answers: Option<Duration>,
    /// It exits with code 1 this long after launch.
    pub exits_after: Option<Duration>,
    /// It ignores SIGTERM, so only the SIGKILL after the grace ends it.
    pub ignores_term: bool,
    /// How long it takes to exit after SIGTERM.
    pub stop_takes: Duration,
    /// Config file writes it misses before it reloads one.
    pub misses_writes: usize,
    /// Signalling it fails, as a broken `kill` would.
    pub unkillable: bool,
    /// Another local user's program holds its port and answers every call to it as a ready
    /// core would, so the core does not own its port.
    pub squatted: bool,
    /// Once sent SIGTERM it closes its listener and such a program takes the port.
    pub squatted_once_terminated: bool,
    /// How long checking that it owns its port takes, as `lsof` can on macOS.
    pub listens_takes: Duration,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            launch_error: false,
            answers: Some(Duration::ZERO),
            exits_after: None,
            ignores_term: false,
            stop_takes: Duration::ZERO,
            misses_writes: 0,
            unkillable: false,
            squatted: false,
            squatted_once_terminated: false,
            listens_takes: Duration::ZERO,
        }
    }
}

/// A held management path; see [`FakeCores::hold`].
struct Hold {
    path: &'static str,
    when: Box<dyn Fn(&ProfileId) -> bool + Send + Sync>,
    released: watch::Receiver<bool>,
    holding: Arc<AtomicUsize>,
}

/// What the fake cores do, changed by the test as it goes.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent switches a test flips one at a time"
)]
pub struct Knobs {
    pub scripts: HashMap<ProfileId, VecDeque<Script>>,
    /// `/auth-files` answers per profile, in place of the credential files.
    pub listings: HashMap<ProfileId, Value>,
    /// Profiles whose account list answers HTTP 500.
    pub failing_listing: HashSet<ProfileId>,
    /// Profiles whose usage queue answers HTTP 500.
    pub failing_queue: HashSet<ProfileId>,
    /// Profiles whose usage queue reads as empty while kept here.
    pub hidden_queue: HashSet<ProfileId>,
    hold: Option<Hold>,
    /// A model the upstream answers 429.
    pub limited: Option<String>,
    /// Sees each management call (the core and its path) before the core does; an answer it
    /// gives replaces the core's, and `None` from it breaks the connection.
    pub intercept: Option<Intercept>,
    /// The `redirect_uri` of each sign-in link; `None` makes links without one.
    pub redirect: Option<String>,
    pub expires_in: Option<Value>,
    /// What a finished browser sign-in leaves in the core.
    pub credential: Value,
    /// `get-auth-status` answers `error`.
    pub auth_fails: bool,
    /// Ending a pending authorization answers HTTP 500.
    pub cancel_fails: bool,
    /// Holds each ending of a pending authorization until the receiver turns true; the notify
    /// tells that one arrived.
    pub cancel_gate: Option<(Arc<Notify>, watch::Receiver<bool>)>,
    /// Answers `/api-call` from its request.
    pub upstream: Arc<dyn Fn(&Value) -> Value + Send + Sync>,
    /// Holds each `/api-call` until the receiver turns true; the notify tells that one arrived.
    pub upstream_gate: Option<(Arc<Notify>, watch::Receiver<bool>)>,
    /// Each core exits with code 1 as soon as it is launched, before it answers.
    pub exits_on_launch: bool,
    /// Cores run but answer no call, as one that never becomes ready.
    pub silent: bool,
    /// Runs before each sign-in link is answered, given the profile of the core asked.
    pub auth_url_hook: Option<Arc<dyn Fn(ProfileId) -> BoxFuture<'static, ()> + Send + Sync>>,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            scripts: HashMap::new(),
            listings: HashMap::new(),
            failing_listing: HashSet::new(),
            failing_queue: HashSet::new(),
            hidden_queue: HashSet::new(),
            hold: None,
            limited: None,
            intercept: None,
            redirect: None,
            expires_in: None,
            credential: Value::Null,
            auth_fails: false,
            cancel_fails: false,
            cancel_gate: None,
            upstream: Arc::new(
                |_| json!({ "status_code": 200, "body": { "five_hour": { "utilization": 25 } } }),
            ),
            upstream_gate: None,
            exits_on_launch: false,
            silent: false,
            auth_url_hook: None,
        }
    }
}

pub type Intercept =
    Arc<dyn Fn(&Core, &str) -> Option<BoxFuture<'static, Option<Value>>> + Send + Sync>;

pub struct Core {
    pub profile: ProfileId,
    pub port: u16,
    /// Its position among every launch of the fake.
    pub index: usize,
    pub data: PathBuf,
    pub binary: PathBuf,
    pub launched: Instant,
    pub terms: AtomicUsize,
    management_key: String,
    client_key: String,
    auth: PathBuf,
    config: PathBuf,
    script: Script,
    exited: watch::Sender<Option<i32>>,
    queue: Mutex<Vec<Value>>,
    /// The config it applied, the config file version it last looked at (modification time
    /// and text), and how many more writes it will miss.
    applied: Mutex<(Value, Option<SystemTime>, String, usize)>,
    /// Pending authorizations by state: whether the browser came back.
    pending: Mutex<HashMap<String, bool>>,
}

impl Core {
    #[must_use]
    pub fn alive(&self) -> bool {
        self.exited.borrow().is_none()
    }

    fn answering(&self) -> bool {
        self.alive()
            && self
                .script
                .answers
                .is_some_and(|after| Instant::now() >= self.launched + after)
    }

    fn exit(&self, raw: i32) {
        self.exited.send_if_modified(|status| {
            let first = status.is_none();
            if first {
                *status = Some(raw);
            }
            first
        });
    }

    /// The config its file holds, read as the core reloads it. A real core reloads on each
    /// write event; the fake sees a write by the file's modification time, which the kernel
    /// keeps at a coarse tick, so a core that misses writes waits a tick of real time after
    /// each look: consecutive writes it is asked about then differ in time.
    fn config(&self) -> Value {
        if self.script.misses_writes > 0 {
            let started = std::time::Instant::now();
            while started.elapsed() < Duration::from_millis(5) {
                std::thread::park_timeout(Duration::from_millis(5));
            }
        }
        let mut applied = self.applied.lock().unwrap();
        let modified = std::fs::metadata(&self.config)
            .and_then(|metadata| metadata.modified())
            .ok();
        let text = std::fs::read_to_string(&self.config).unwrap_or_default();
        if modified != applied.1 || text != applied.2 {
            applied.1 = modified;
            applied.2 = text;
            if applied.3 > 0 {
                applied.3 -= 1;
            } else {
                applied.0 = yaml(&applied.2);
            }
        }
        applied.0.clone()
    }
}

#[derive(Default)]
struct Cores {
    launched: Vec<Arc<Core>>,
    knobs: Knobs,
    requests: usize,
    /// Every management call: the profile, method, path with query, when, and its timeout.
    calls: Vec<(ProfileId, Method, String, Instant, Duration)>,
    /// Data directories whose ledgers were reaped.
    reaped: Vec<PathBuf>,
    states: usize,
    /// Calls with an `Authorization` header that a squatter received.
    squatted: usize,
    /// The profile and `auth_index` of each `/reset-quota` call.
    quota_resets: Vec<(ProfileId, String)>,
}

#[derive(Clone, Default)]
pub struct FakeCores(Arc<Mutex<Cores>>);

/// An answer with a JSON or text body and no headers.
#[must_use]
pub fn response(status: u16, body: impl Into<Vec<u8>>) -> Response {
    Response {
        status: StatusCode::from_u16(status).unwrap(),
        headers: HeaderMap::new(),
        body: body.into(),
    }
}

fn respond(status: u16, body: &Value) -> Response {
    response(status, body.to_string())
}

impl FakeCores {
    pub fn knobs<T>(&self, change: impl FnOnce(&mut Knobs) -> T) -> T {
        change(&mut self.0.lock().unwrap().knobs)
    }

    /// Scripts the profile's next launches, in order; later ones behave by default.
    pub fn script(&self, profile: &ProfileId, scripts: impl IntoIterator<Item = Script>) {
        self.knobs(|knobs| {
            knobs
                .scripts
                .entry(profile.clone())
                .or_default()
                .extend(scripts);
        });
    }

    /// Holds management requests to `path` (a prefix) while `when` says so, until the answered
    /// sender turns true; the counter tells how many were held. An account list is read before
    /// the hold, a usage queue after it.
    pub fn hold(
        &self,
        path: &'static str,
        when: impl Fn(&ProfileId) -> bool + Send + Sync + 'static,
    ) -> (watch::Sender<bool>, Arc<AtomicUsize>) {
        let (release, released) = watch::channel(false);
        let holding = Arc::new(AtomicUsize::new(0));
        self.knobs(|knobs| {
            knobs.hold = Some(Hold {
                path,
                when: Box::new(when),
                released,
                holding: Arc::clone(&holding),
            });
        });
        (release, holding)
    }

    /// Every launch, in order.
    #[must_use]
    pub fn launches(&self) -> Vec<Arc<Core>> {
        self.0.lock().unwrap().launched.clone()
    }

    /// Every launch of the profile's core, in order.
    #[must_use]
    pub fn launches_of(&self, profile: &ProfileId) -> Vec<Arc<Core>> {
        let cores = self.0.lock().unwrap();
        cores
            .launched
            .iter()
            .filter(|core| core.profile == *profile)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn latest(&self, profile: &ProfileId) -> Option<Arc<Core>> {
        self.launches_of(profile).pop()
    }

    /// Kills the profile's latest core as a crash would.
    pub fn kill(&self, profile: &ProfileId) {
        if let Some(core) = self.latest(profile) {
            core.exit(9);
        }
    }

    /// The management calls to `profile`'s cores whose path starts with `prefix`.
    #[must_use]
    pub fn calls(&self, profile: &ProfileId, prefix: &str) -> Vec<(Method, String)> {
        self.0
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(called, _, path, ..)| called == profile && path.starts_with(prefix))
            .map(|(_, method, path, ..)| (method.clone(), path.clone()))
            .collect()
    }

    /// When each management call to `profile`'s cores whose path starts with `prefix` arrived.
    #[must_use]
    pub fn called_at(&self, profile: &ProfileId, prefix: &str) -> Vec<Instant> {
        self.0
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(called, _, path, ..)| called == profile && path.starts_with(prefix))
            .map(|(_, _, _, at, _)| *at)
            .collect()
    }

    /// The timeout of each management call to `profile`'s cores whose path starts with `prefix`.
    #[must_use]
    pub fn timeouts(&self, profile: &ProfileId, prefix: &str) -> Vec<Duration> {
        self.0
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(called, _, path, ..)| called == profile && path.starts_with(prefix))
            .map(|(.., timeout)| *timeout)
            .collect()
    }

    /// The `auth_index` of each `/reset-quota` call to `profile`'s cores, in order.
    #[must_use]
    pub fn quota_resets(&self, profile: &ProfileId) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .quota_resets
            .iter()
            .filter(|(called, _)| called == profile)
            .map(|(_, index)| index.clone())
            .collect()
    }

    #[must_use]
    pub fn reaped(&self) -> Vec<PathBuf> {
        self.0.lock().unwrap().reaped.clone()
    }

    /// How many calls with an `Authorization` header squatters received.
    #[must_use]
    pub fn squatted(&self) -> usize {
        self.0.lock().unwrap().squatted
    }

    /// A gateway request for `model` to the core on `port`: the upstream answers 200, or 429
    /// for the limited model, and the core queues its usage. 502 without an answering core.
    #[must_use]
    pub fn request(&self, port: u16, model: &str) -> u16 {
        let mut cores = self.0.lock().unwrap();
        cores.requests += 1;
        let request = cores.requests;
        let limited = cores.knobs.limited.as_deref() == Some(model);
        let Some(core) = cores
            .launched
            .iter()
            .find(|core| core.port == port && core.answering())
        else {
            return 502;
        };
        let mut entry = json!({
            "request_id": format!("request-{request}"),
            "timestamp": "2026-11-23T10:00:00.000Z",
            "provider": "openai-compatibility",
            "model": model,
            "auth_index": "1",
            "tokens": { "input_tokens": 11, "output_tokens": 3, "total_tokens": 14 },
        });
        if limited {
            entry["failed"] = json!(true);
            entry["fail"] = json!({ "status_code": 429 });
        }
        core.queue.lock().unwrap().push(entry);
        if limited { 429 } else { 200 }
    }

    fn launch_core(
        &self,
        binary: &Path,
        data: &DataDir,
        profile: &ProfileId,
    ) -> io::Result<Arc<Core>> {
        let config = data.core_config(profile);
        let text = std::fs::read_to_string(&config)?;
        // An executable whose bytes say so cannot start.
        if std::fs::read(binary).is_ok_and(|bytes| bytes.starts_with(b"broken")) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "broken core",
            ));
        }
        let mut cores = self.0.lock().unwrap();
        let script = cores
            .knobs
            .scripts
            .get_mut(profile)
            .and_then(VecDeque::pop_front)
            .unwrap_or_default();
        if script.launch_error {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such file"));
        }
        let settings = yaml(&text);
        let modified = std::fs::metadata(&config)
            .and_then(|metadata| metadata.modified())
            .ok();
        let core = Arc::new(Core {
            profile: profile.clone(),
            port: u16::try_from(settings["port"].as_u64().unwrap()).unwrap(),
            index: cores.launched.len(),
            data: data.path().to_owned(),
            binary: binary.to_owned(),
            launched: Instant::now(),
            terms: AtomicUsize::new(0),
            management_key: settings["remote-management"]["secret-key"]
                .as_str()
                .unwrap()
                .to_owned(),
            client_key: settings["api-keys"][0].as_str().unwrap().to_owned(),
            auth: PathBuf::from(settings["auth-dir"].as_str().unwrap()),
            config,
            exited: watch::Sender::new(None),
            queue: Mutex::new(Vec::new()),
            applied: Mutex::new((settings, modified, text, script.misses_writes)),
            pending: Mutex::new(HashMap::new()),
            script,
        });
        cores.launched.push(Arc::clone(&core));
        if cores.knobs.exits_on_launch {
            core.exit(1 << 8);
        }
        if let Some(after) = core.script.exits_after {
            let exiting = Arc::clone(&core);
            tokio::spawn(async move {
                tokio::time::sleep(after).await;
                exiting.exit(1 << 8);
            });
        }
        Ok(core)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per management route, as the core's router"
    )]
    async fn answer(&self, call: Call) -> Result<Response, NetError> {
        {
            let mut cores = self.0.lock().unwrap();
            let squatted = cores.launched.iter().any(|core| {
                core.port == call.port
                    && (core.script.squatted
                        || (core.script.squatted_once_terminated
                            && core.terms.load(Ordering::SeqCst) > 0))
            });
            if squatted {
                if !call.authorization.is_empty() {
                    cores.squatted += 1;
                }
                return Ok(respond(200, &json!({})));
            }
        }
        let silent = self.knobs(|knobs| knobs.silent);
        let core = self
            .0
            .lock()
            .unwrap()
            .launched
            .iter()
            .find(|core| core.port == call.port && core.alive())
            .cloned()
            .filter(|core| core.answering() && !silent)
            .ok_or_else(|| NetError::Connect(io::ErrorKind::ConnectionRefused.into()))?;
        if call.target == "/v1/models" {
            return Ok(
                if call.authorization == format!("Bearer {}", core.client_key) {
                    respond(
                        200,
                        &json!({ "object": "list", "data": [{ "id": "fixture-model", "owned_by": "fixture" }, { "id": 7 }] }),
                    )
                } else {
                    respond(401, &json!({ "error": "unauthorized" }))
                },
            );
        }
        if call.authorization != format!("Bearer {}", core.management_key) {
            return Ok(respond(401, &json!({ "error": "invalid management key" })));
        }
        let target = call
            .target
            .strip_prefix("/v0/management")
            .unwrap_or_default()
            .to_owned();
        self.0.lock().unwrap().calls.push((
            core.profile.clone(),
            call.method.clone(),
            target.clone(),
            Instant::now(),
            call.timeout,
        ));
        let intercepted = self
            .knobs(|knobs| knobs.intercept.clone())
            .and_then(|intercept| intercept(&core, &target));
        if let Some(answer) = intercepted {
            return answer.await.map_or_else(
                || Err(NetError::Connect(io::ErrorKind::ConnectionReset.into())),
                |answer| Ok(respond(200, &answer)),
            );
        }
        let hold = self.knobs(|knobs| {
            knobs.hold.as_ref().and_then(|hold| {
                (target.starts_with(hold.path) && (hold.when)(&core.profile))
                    .then(|| (hold.released.clone(), Arc::clone(&hold.holding)))
            })
        });
        let waiting = async {
            if let Some((mut released, holding)) = hold {
                holding.fetch_add(1, Ordering::SeqCst);
                let _ = released.wait_for(|released| *released).await;
            }
        };
        let url = Url::parse(&format!("http://core{target}")).unwrap();
        let query = |key: &str| {
            url.query_pairs()
                .find(|(name, _)| name == key)
                .map_or_else(String::new, |(_, value)| value.into_owned())
        };
        let body: Value = call
            .body
            .as_deref()
            .map_or(Value::Null, |body| serde_json::from_slice(body).unwrap());
        Ok(match (call.method.clone(), url.path()) {
            (Method::GET, "/config") => {
                waiting.await;
                respond(200, &core.config())
            }
            (Method::GET, "/auth-files") => {
                if self.knobs(|knobs| knobs.failing_listing.contains(&core.profile)) {
                    return Ok(respond(
                        500,
                        &json!({ "error": "account list unavailable" }),
                    ));
                }
                let listing = self
                    .knobs(|knobs| knobs.listings.get(&core.profile).cloned())
                    .unwrap_or_else(|| listing(&core.auth));
                waiting.await;
                respond(200, &listing)
            }
            (_, "/usage-queue") => {
                waiting.await;
                let (failing, hidden) = self.knobs(|knobs| {
                    (
                        knobs.failing_queue.contains(&core.profile),
                        knobs.hidden_queue.contains(&core.profile),
                    )
                });
                if failing {
                    return Ok(respond(500, &json!({ "error": "queue unavailable" })));
                }
                if hidden {
                    return Ok(respond(200, &json!([])));
                }
                let mut queue = core.queue.lock().unwrap();
                let count = queue.len().min(500);
                let page: Vec<Value> = queue.drain(..count).collect();
                respond(200, &Value::Array(page))
            }
            // The models the core routes to the credential `name`: one named after it, and an
            // entry without a string id.
            (Method::GET, "/auth-files/models") => respond(
                200,
                &json!({ "models": [
                    { "id": format!("model-for-{}", query("name")), "object": "model", "owned_by": "fixture" },
                    { "id": 7 },
                ] }),
            ),
            (Method::POST, "/reset-quota") => {
                self.0.lock().unwrap().quota_resets.push((
                    core.profile.clone(),
                    body["auth_index"].as_str().unwrap().to_owned(),
                ));
                respond(200, &json!({ "status": "ok" }))
            }
            (Method::POST, "/auth-files") => {
                write_credential(&core.auth.join(query("name")), &body);
                respond(200, &json!({ "status": "ok" }))
            }
            (Method::DELETE, "/auth-files") => {
                match std::fs::remove_file(core.auth.join(query("name"))) {
                    Ok(()) => respond(200, &json!({ "status": "ok" })),
                    Err(_) => respond(404, &json!({ "error": "not found" })),
                }
            }
            (Method::PATCH, "/auth-files/status" | "/auth-files/fields") => {
                let file = core.auth.join(body["name"].as_str().unwrap());
                let mut credential: Value =
                    serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
                for (key, value) in body.as_object().unwrap() {
                    if key != "name" {
                        credential[key] = value.clone();
                    }
                }
                write_credential(&file, &credential);
                respond(200, &json!({ "status": "ok" }))
            }
            (Method::GET, auth) if auth.ends_with("-auth-url") => {
                let hook = self.knobs(|knobs| knobs.auth_url_hook.clone());
                if let Some(hook) = hook {
                    hook(core.profile.clone()).await;
                }
                let (state, redirect, expires) = {
                    let mut cores = self.0.lock().unwrap();
                    cores.states += 1;
                    (
                        format!("state-{}", cores.states),
                        cores.knobs.redirect.clone(),
                        cores.knobs.expires_in.clone(),
                    )
                };
                core.pending.lock().unwrap().insert(state.clone(), false);
                let mut link = Url::parse("https://auth.example.invalid/authorize").unwrap();
                link.query_pairs_mut().append_pair("state", &state);
                if let Some(redirect) = redirect {
                    link.query_pairs_mut()
                        .append_pair("redirect_uri", &redirect);
                }
                let mut answer = json!({ "url": link.as_str(), "state": state });
                if let Some(expires) = expires {
                    answer["expires_in"] = expires;
                }
                respond(200, &answer)
            }
            (Method::GET, "/get-auth-status") => {
                let done = core.pending.lock().unwrap().get(&query("state")).copied();
                let status = if self.knobs(|knobs| knobs.auth_fails) {
                    "error"
                } else if done == Some(true) {
                    "ok"
                } else {
                    "wait"
                };
                respond(200, &json!({ "status": status }))
            }
            (Method::POST, "/oauth-callback") => {
                let redirect = Url::parse(body["redirect_url"].as_str().unwrap()).unwrap();
                let state = redirect
                    .query_pairs()
                    .find(|(name, _)| name == "state")
                    .map_or_else(String::new, |(_, value)| value.into_owned());
                let known = core.pending.lock().unwrap().insert(state, true).is_some();
                if !known {
                    return Ok(respond(404, &json!({ "error": "unknown state" })));
                }
                let credential = self.knobs(|knobs| knobs.credential.clone());
                let name = format!(
                    "{}-{}.json",
                    credential["type"].as_str().unwrap(),
                    credential["email"].as_str().unwrap_or("signed-in")
                );
                write_credential(&core.auth.join(name), &credential);
                respond(200, &json!({ "status": "ok" }))
            }
            (Method::DELETE, "/oauth-session") => {
                let gate = self.knobs(|knobs| knobs.cancel_gate.clone());
                if let Some((entered, mut released)) = gate {
                    entered.notify_one();
                    let _released = released.wait_for(|released| *released).await;
                }
                if self.knobs(|knobs| knobs.cancel_fails) {
                    return Ok(respond(500, &json!({ "error": "cancel failed" })));
                }
                core.pending.lock().unwrap().remove(&query("state"));
                respond(200, &json!({}))
            }
            (Method::POST, "/api-call") => {
                let gate = self.knobs(|knobs| knobs.upstream_gate.clone());
                if let Some((entered, mut released)) = gate {
                    entered.notify_one();
                    let _released = released.wait_for(|released| *released).await;
                }
                let upstream = self.knobs(|knobs| Arc::clone(&knobs.upstream));
                respond(200, &upstream(&body))
            }
            _ => respond(404, &json!({ "error": "not found" })),
        })
    }
}

/// Writes a credential as the core does: owner-only JSON.
fn write_credential(path: &Path, credential: &Value) {
    let _missing = std::fs::remove_file(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    io::Write::write_all(&mut file, credential.to_string().as_bytes()).unwrap();
}

/// `/auth-files`: each credential file of the directory, with a stable auth index.
fn listing(auth: &Path) -> Value {
    let mut files = Vec::new();
    let mut names: Vec<String> = std::fs::read_dir(auth).map_or_else(
        |_| Vec::new(),
        |listing| {
            listing
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        },
    );
    names.sort();
    for name in names
        .into_iter()
        .filter(|name| Path::new(name).extension() == Some("json".as_ref()))
    {
        let Ok(credential) =
            serde_json::from_slice::<Value>(&std::fs::read(auth.join(&name)).unwrap())
        else {
            continue;
        };
        let mut entry = Map::new();
        entry.insert("name".to_owned(), json!(name));
        entry.insert("type".to_owned(), credential["type"].clone());
        entry.insert("provider".to_owned(), credential["type"].clone());
        entry.insert("auth_index".to_owned(), json!(format!("index-{name}")));
        entry.insert(
            "disabled".to_owned(),
            json!(credential["disabled"] == json!(true)),
        );
        for key in ["email", "note", "priority", "id_token"] {
            if let Some(value) = credential.get(key) {
                entry.insert(key.to_owned(), value.clone());
            }
        }
        files.push(Value::Object(entry));
    }
    json!({ "files": files })
}

/// The JSON a config file written by `nsv_core::config` holds: block YAML whose keys and
/// strings are JSON-quoted, `[]` and `{}` for empty collections. An empty file, as one being
/// rewritten reads, holds an empty map.
fn yaml(text: &str) -> Value {
    // Each line's indent and text; a list item's `- ` becomes indent, flagged.
    let mut lines = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut indent = line.len() - line.trim_start().len();
        let mut rest = line.trim_start();
        let mut item = false;
        while let Some(inner) = rest.strip_prefix("- ") {
            indent += 2;
            rest = inner;
            item = true;
        }
        lines.push((indent, rest.to_owned(), item));
    }
    if lines.is_empty() {
        return Value::Object(Map::new());
    }
    let mut index = 0;
    block(&mut lines, &mut index)
}

fn block(lines: &mut [(usize, String, bool)], index: &mut usize) -> Value {
    let (indent, _, item) = lines[*index].clone();
    if item {
        let mut list = Vec::new();
        while *index < lines.len() && lines[*index].0 == indent && lines[*index].2 {
            lines[*index].2 = false;
            list.push(if key_of(&lines[*index].1).is_some() {
                block(lines, index)
            } else {
                *index += 1;
                scalar(&lines[*index - 1].1)
            });
        }
        return Value::Array(list);
    }
    let mut map = Map::new();
    while *index < lines.len() && lines[*index].0 == indent && !lines[*index].2 {
        let (key, rest) = key_of(&lines[*index].1).unwrap();
        *index += 1;
        let value = if rest.is_empty() {
            block(lines, index)
        } else {
            scalar(&rest)
        };
        map.insert(key, value);
    }
    Value::Object(map)
}

/// `"key": rest` or `"key":`.
fn key_of(text: &str) -> Option<(String, String)> {
    let mut stream = serde_json::Deserializer::from_str(text).into_iter::<String>();
    let key = stream.next()?.ok()?;
    let rest = text.get(stream.byte_offset()..)?.strip_prefix(':')?;
    Some((key, rest.trim().to_owned()))
}

fn scalar(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

/// The backend of one data directory, recording its reaping.
pub struct Directory(pub FakeCores, pub DataDir);

impl Backend for Directory {
    fn launch(&self, binary: &Path, profile: &ProfileId) -> io::Result<Box<dyn CoreHandle>> {
        Ok(Box::new(Handle(
            self.0.launch_core(binary, &self.1, profile)?,
        )))
    }

    fn send(&self, call: Call) -> BoxFuture<'static, Result<Response, NetError>> {
        let fake = self.0.clone();
        Box::pin(async move { fake.answer(call).await })
    }

    fn reap_orphans(&self) -> BoxFuture<'_, io::Result<()>> {
        self.0
            .0
            .lock()
            .unwrap()
            .reaped
            .push(self.1.path().to_owned());
        Box::pin(async { Ok(()) })
    }
}

struct Handle(Arc<Core>);

impl Handle {
    async fn exited(&self) -> ExitStatus {
        let mut exited = self.0.exited.subscribe();
        let raw = *exited
            .wait_for(Option::is_some)
            .await
            .expect("the core outlives its handle");
        ExitStatus::from_raw(raw.unwrap_or_default())
    }
}

impl CoreHandle for Handle {
    fn record(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn wait(&mut self) -> BoxFuture<'_, io::Result<ExitStatus>> {
        Box::pin(async { Ok(self.exited().await) })
    }

    fn terminate(&mut self) -> BoxFuture<'_, io::Result<ExitStatus>> {
        Box::pin(async {
            if self.0.alive() {
                if self.0.script.unkillable {
                    return Err(io::Error::from(io::ErrorKind::PermissionDenied));
                }
                self.0.terms.fetch_add(1, Ordering::SeqCst);
                let (grace, signal) = if self.0.script.ignores_term {
                    (KILL_GRACE, 9)
                } else {
                    (self.0.script.stop_takes, 15)
                };
                tokio::select! {
                    _ = self.exited() => {}
                    () = tokio::time::sleep(grace) => self.0.exit(signal),
                }
            }
            Ok(self.exited().await)
        })
    }

    fn listens(&self, port: u16) -> BoxFuture<'static, io::Result<bool>> {
        let owned = port == self.0.port && !self.0.script.squatted;
        let takes = self.0.script.listens_takes;
        Box::pin(async move {
            tokio::time::sleep(takes).await;
            Ok(owned)
        })
    }
}
