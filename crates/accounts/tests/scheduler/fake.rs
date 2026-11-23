//! In-process fake cores behind the pool's [`Backend`], so the pool and the scheduler run
//! under tokio's paused clock. A fake core reads its generated `config.yaml` as the real one
//! does (port, keys, routing), answers the management API with its management key, keeps a
//! usage queue in its own memory, and exits on SIGTERM after a scripted delay.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use nsv_core::pool::{Backend, BoxFuture, Call, CoreHandle};
use nsv_core::processes::KILL_GRACE;
use nsv_domain::id::ProfileId;
use nsv_net::{HeaderMap, NetError, Response, StatusCode};
use nsv_store::layout::core_config_path;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

/// How one launch of a profile's core behaves.
#[derive(Debug, Clone)]
pub(crate) struct Script {
    /// The executable cannot be started.
    pub(crate) launch_error: bool,
    /// When it starts answering its management API; `None`: never.
    pub(crate) answers: Option<Duration>,
    /// It exits with code 1 this long after launch.
    pub(crate) exits_after: Option<Duration>,
    /// It ignores SIGTERM, so only the SIGKILL after the grace ends it.
    pub(crate) ignores_term: bool,
    /// How long it takes to exit after SIGTERM.
    pub(crate) stop_takes: Duration,
    /// Config file writes it misses before it reloads one.
    pub(crate) misses_writes: usize,
    /// Signalling it fails, as a broken `kill` would.
    pub(crate) unkillable: bool,
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
        }
    }
}

/// Holds a management request to `path` (a prefix) while `when` says so, until released. An
/// account list is read before the hold, a usage queue after it.
pub(crate) struct Hold {
    pub(crate) path: &'static str,
    pub(crate) when: Box<dyn Fn(&ProfileId) -> bool + Send + Sync>,
    pub(crate) released: watch::Receiver<bool>,
    pub(crate) holding: Arc<AtomicUsize>,
}

#[derive(Default)]
pub(crate) struct Knobs {
    pub(crate) scripts: HashMap<ProfileId, VecDeque<Script>>,
    /// `/auth-files` answers per profile.
    pub(crate) listings: HashMap<ProfileId, Value>,
    /// Profiles whose account list answers HTTP 500.
    pub(crate) failing_listing: HashSet<ProfileId>,
    /// Profiles whose usage queue answers HTTP 500.
    pub(crate) failing_queue: HashSet<ProfileId>,
    /// Profiles whose usage queue reads as empty while kept here.
    pub(crate) hidden_queue: HashSet<ProfileId>,
    pub(crate) hold: Option<Hold>,
    /// A model the upstream answers 429.
    pub(crate) limited: Option<String>,
}

pub(crate) struct Core {
    pub(crate) profile: ProfileId,
    pub(crate) port: u16,
    /// Its position among every launch of the fake.
    pub(crate) index: usize,
    management_key: String,
    api_key: String,
    pub(crate) launched: Instant,
    script: Script,
    exited: watch::Sender<Option<i32>>,
    queue: Mutex<Vec<Value>>,
    config: PathBuf,
    /// The routing it applied, the config file version it last looked at (modification time
    /// and text), and how many more writes it will miss.
    applied: Mutex<(Value, Option<SystemTime>, String, usize)>,
    pub(crate) terms: AtomicUsize,
}

impl Core {
    pub(crate) fn alive(&self) -> bool {
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
            if status.is_some() {
                return false;
            }
            *status = Some(raw);
            true
        });
    }

    /// The routing its config file holds, read as the core reloads it. A real core reloads on
    /// each write event; the fake sees a write by the file's modification time, which the
    /// kernel keeps at a coarse tick, so a core that misses writes waits a tick of real time
    /// after each look: consecutive writes it is asked about then differ in time.
    fn routing(&self) -> Value {
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
                applied.0 = read_routing(&self.config);
            }
        }
        applied.0.clone()
    }
}

/// The `routing` block of a generated config, as the core reports it.
fn read_routing(config: &Path) -> Value {
    let text = std::fs::read_to_string(config).unwrap_or_default();
    json!({
        "strategy": config_value(&text, "strategy").unwrap_or_default(),
        "session-affinity": config_value(&text, "session-affinity").as_deref() == Some("true"),
        "session-affinity-ttl": config_value(&text, "session-affinity-ttl").unwrap_or_default(),
    })
}

/// A scalar of the generated YAML, whose keys and strings are JSON-quoted.
fn scalar(text: &str) -> String {
    serde_json::from_str::<String>(text).unwrap_or_else(|_| text.to_owned())
}

fn config_value(text: &str, key: &str) -> Option<String> {
    let prefix = format!("\"{key}\": ");
    text.lines()
        .find_map(|line| line.trim().strip_prefix(&prefix).map(scalar))
}

#[derive(Default)]
struct Cores {
    launched: Vec<Arc<Core>>,
    knobs: Knobs,
    requests: usize,
    /// Every management request: the profile, its path and when.
    calls: Vec<(ProfileId, String, Instant)>,
}

#[derive(Clone, Default)]
pub(crate) struct Fake(Arc<Mutex<Cores>>);

impl Fake {
    pub(crate) fn knobs<T>(&self, change: impl FnOnce(&mut Knobs) -> T) -> T {
        change(&mut self.0.lock().unwrap().knobs)
    }

    /// Scripts the profile's next launches, in order; later ones behave by default.
    pub(crate) fn script(&self, profile: &ProfileId, scripts: impl IntoIterator<Item = Script>) {
        self.knobs(|knobs| {
            knobs
                .scripts
                .entry(profile.clone())
                .or_default()
                .extend(scripts);
        });
    }

    /// Every launch of the profile's core, in order.
    pub(crate) fn launches(&self, profile: &ProfileId) -> Vec<Arc<Core>> {
        let cores = self.0.lock().unwrap();
        cores
            .launched
            .iter()
            .filter(|core| core.profile == *profile)
            .cloned()
            .collect()
    }

    /// When each management request to the profile's cores whose path starts with `prefix`
    /// arrived.
    pub(crate) fn calls(&self, profile: &ProfileId, prefix: &str) -> Vec<Instant> {
        let cores = self.0.lock().unwrap();
        cores
            .calls
            .iter()
            .filter(|(called, path, _)| called == profile && path.starts_with(prefix))
            .map(|(_, _, at)| *at)
            .collect()
    }

    pub(crate) fn latest(&self, profile: &ProfileId) -> Option<Arc<Core>> {
        self.launches(profile).pop()
    }

    /// Kills the profile's latest core as a crash would.
    pub(crate) fn kill(&self, profile: &ProfileId) {
        if let Some(core) = self.latest(profile) {
            core.exit(9);
        }
    }

    /// A gateway request for `model` to the core on `port`: the upstream answers 200, or 429
    /// for the limited model, and the core queues its usage. 502 without an answering core.
    pub(crate) fn request(&self, port: u16, model: &str) -> u16 {
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

    fn launch_core(&self, data: &Path, profile: &ProfileId) -> io::Result<Arc<Core>> {
        let config = core_config_path(data, profile);
        let text = std::fs::read_to_string(&config)?;
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
        let keys = text
            .split_once("\"api-keys\":\n  - ")
            .and_then(|(_, rest)| rest.lines().next())
            .map_or_else(String::new, scalar);
        let modified = std::fs::metadata(&config)
            .and_then(|metadata| metadata.modified())
            .ok();
        let core = Arc::new(Core {
            profile: profile.clone(),
            port: config_value(&text, "port")
                .and_then(|port| port.parse().ok())
                .unwrap_or(0),
            index: cores.launched.len(),
            management_key: config_value(&text, "secret-key").unwrap_or_default(),
            api_key: keys,
            launched: Instant::now(),
            exited: watch::Sender::new(None),
            queue: Mutex::new(Vec::new()),
            applied: Mutex::new((
                read_routing(&config),
                modified,
                text.clone(),
                script.misses_writes,
            )),
            config,
            script,
            terms: AtomicUsize::new(0),
        });
        cores.launched.push(Arc::clone(&core));
        if let Some(after) = core.script.exits_after {
            let exiting = Arc::clone(&core);
            tokio::spawn(async move {
                tokio::time::sleep(after).await;
                exiting.exit(1 << 8);
            });
        }
        Ok(core)
    }

    async fn answer(&self, call: Call) -> Result<Response, NetError> {
        let refused = || NetError::Connect(io::Error::from(io::ErrorKind::ConnectionRefused));
        let core = {
            let cores = self.0.lock().unwrap();
            cores
                .launched
                .iter()
                .find(|core| core.port == call.port && core.alive())
                .cloned()
        }
        .filter(|core| core.answering())
        .ok_or_else(refused)?;
        if call.target == "/v1/models" {
            if call.authorization != format!("Bearer {}", core.api_key) {
                return Ok(respond(401, &json!({ "error": "unauthorized" })));
            }
            return Ok(respond(
                200,
                &json!({ "object": "list", "data": [{ "id": "fixture-model", "owned_by": "fixture" }, { "id": 7 }] }),
            ));
        }
        if call.authorization != format!("Bearer {}", core.management_key) {
            return Ok(respond(401, &json!({ "error": "invalid management key" })));
        }
        let path = call
            .target
            .strip_prefix("/v0/management")
            .unwrap_or_default()
            .to_owned();
        self.0
            .lock()
            .unwrap()
            .calls
            .push((core.profile.clone(), path.clone(), Instant::now()));
        let hold = self.knobs(|knobs| {
            knobs.hold.as_ref().and_then(|hold| {
                (path.starts_with(hold.path) && (hold.when)(&core.profile))
                    .then(|| (hold.released.clone(), Arc::clone(&hold.holding)))
            })
        });
        let waiting = async {
            if let Some((mut released, holding)) = hold {
                holding.fetch_add(1, Ordering::SeqCst);
                let _ = released.wait_for(|released| *released).await;
            }
        };
        if path == "/config" {
            waiting.await;
            return Ok(respond(200, &json!({ "routing": core.routing() })));
        }
        if path == "/auth-files" {
            if self.knobs(|knobs| knobs.failing_listing.contains(&core.profile)) {
                return Ok(respond(
                    500,
                    &json!({ "error": "account list unavailable" }),
                ));
            }
            let listing = self
                .knobs(|knobs| knobs.listings.get(&core.profile).cloned())
                .unwrap_or_else(|| json!({ "files": [] }));
            waiting.await;
            return Ok(respond(200, &listing));
        }
        if path.starts_with("/usage-queue") {
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
            return Ok(respond(200, &Value::Array(page)));
        }
        Ok(respond(404, &json!({ "error": "not found" })))
    }
}

fn respond(status: u16, body: &Value) -> Response {
    Response {
        status: StatusCode::from_u16(status).unwrap(),
        headers: HeaderMap::new(),
        body: body.to_string().into_bytes(),
    }
}

impl Backend for Fake {
    fn launch(
        &self,
        _binary: &Path,
        data: &Path,
        profile: &ProfileId,
    ) -> io::Result<Box<dyn CoreHandle>> {
        Ok(Box::new(Handle(self.launch_core(data, profile)?)))
    }

    fn send(&self, call: Call) -> BoxFuture<'static, Result<Response, NetError>> {
        let fake = self.clone();
        Box::pin(async move { fake.answer(call).await })
    }

    fn reap_orphans(&self) -> BoxFuture<'_, io::Result<()>> {
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
}
