//! In-process fake cores behind the pool's [`Backend`], in real time. A fake reads the config
//! file it was started with (port, keys, auth directory), keeps OAuth credentials as files in
//! that directory and answers the management calls of sign-in and credential changes as the
//! real core does. A browser sign-in finishes when its callback arrives, and leaves the scripted
//! credential behind. No provider is ever contacted.

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};

use nsv_core::pool::{Backend, BoxFuture, Call, CoreHandle};
use nsv_domain::id::ProfileId;
use nsv_net::{HeaderMap, Method, NetError, Response, StatusCode};
use nsv_store::layout::core_config_path;
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, watch};
use url::Url;

/// What the fake cores do, changed by the test as it goes.
pub(crate) struct Knobs {
    /// The `redirect_uri` of each sign-in link; `None` makes links without one.
    pub(crate) redirect: Option<String>,
    pub(crate) expires_in: Option<Value>,
    /// What a finished browser sign-in leaves in the core.
    pub(crate) credential: Value,
    /// `get-auth-status` answers `error`.
    pub(crate) auth_fails: bool,
    /// Ending a pending authorization answers HTTP 500.
    pub(crate) cancel_fails: bool,
    /// Holds each ending of a pending authorization until the receiver turns true; the notify
    /// tells that one arrived.
    pub(crate) cancel_gate: Option<(Arc<Notify>, watch::Receiver<bool>)>,
    /// Answers `/api-call` from its request.
    pub(crate) upstream: Arc<dyn Fn(&Value) -> Value + Send + Sync>,
    /// Holds each `/api-call` until the receiver turns true; the notify tells that one arrived.
    pub(crate) upstream_gate: Option<(Arc<Notify>, watch::Receiver<bool>)>,
    /// Each core exits with code 1 as soon as it is launched, before it answers.
    pub(crate) exits_on_launch: bool,
    /// Cores run but answer no call, as one that never becomes ready.
    pub(crate) silent: bool,
    /// Runs before each sign-in link is answered, given the profile of the core asked.
    pub(crate) auth_url_hook:
        Option<Arc<dyn Fn(ProfileId) -> BoxFuture<'static, ()> + Send + Sync>>,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
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

pub(crate) struct Core {
    pub(crate) profile: ProfileId,
    pub(crate) data: PathBuf,
    pub(crate) binary: PathBuf,
    port: u16,
    management_key: String,
    client_key: String,
    auth: PathBuf,
    config: PathBuf,
    exited: watch::Sender<Option<i32>>,
    /// Pending authorizations by state: whether the browser came back.
    pending: Mutex<HashMap<String, bool>>,
}

impl Core {
    pub(crate) fn alive(&self) -> bool {
        self.exited.borrow().is_none()
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
}

#[derive(Default)]
struct Cores {
    launched: Vec<Arc<Core>>,
    knobs: Knobs,
    /// Every management call: the profile, method and path with query.
    calls: Vec<(ProfileId, Method, String)>,
    /// Data directories whose ledgers were reaped.
    reaped: Vec<PathBuf>,
    states: usize,
}

#[derive(Clone, Default)]
pub(crate) struct FakeCores(Arc<Mutex<Cores>>);

fn respond(status: u16, body: &Value) -> Response {
    Response {
        status: StatusCode::from_u16(status).unwrap(),
        headers: HeaderMap::new(),
        body: body.to_string().into_bytes(),
    }
}

impl FakeCores {
    pub(crate) fn knobs<T>(&self, change: impl FnOnce(&mut Knobs) -> T) -> T {
        change(&mut self.0.lock().unwrap().knobs)
    }

    /// Every launch, in order.
    pub(crate) fn launches(&self) -> Vec<Arc<Core>> {
        self.0.lock().unwrap().launched.clone()
    }

    /// The management calls to `profile`'s cores whose path starts with `prefix`.
    pub(crate) fn calls(&self, profile: &ProfileId, prefix: &str) -> Vec<(Method, String)> {
        self.0
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(called, _, path)| called == profile && path.starts_with(prefix))
            .map(|(_, method, path)| (method.clone(), path.clone()))
            .collect()
    }

    pub(crate) fn reaped(&self) -> Vec<PathBuf> {
        self.0.lock().unwrap().reaped.clone()
    }

    fn launch_core(
        &self,
        binary: &Path,
        data: &Path,
        profile: &ProfileId,
    ) -> io::Result<Arc<Core>> {
        let config = core_config_path(data, profile);
        let settings = yaml(&std::fs::read_to_string(&config)?);
        // An executable whose bytes say so cannot start.
        if std::fs::read(binary).is_ok_and(|bytes| bytes.starts_with(b"broken")) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "broken core",
            ));
        }
        let mut cores = self.0.lock().unwrap();
        let core = Arc::new(Core {
            profile: profile.clone(),
            data: data.to_owned(),
            binary: binary.to_owned(),
            port: u16::try_from(settings["port"].as_u64().unwrap()).unwrap(),
            management_key: settings["remote-management"]["secret-key"]
                .as_str()
                .unwrap()
                .to_owned(),
            client_key: settings["api-keys"][0].as_str().unwrap().to_owned(),
            auth: PathBuf::from(settings["auth-dir"].as_str().unwrap()),
            config,
            exited: watch::Sender::new(None),
            pending: Mutex::new(HashMap::new()),
        });
        cores.launched.push(Arc::clone(&core));
        if cores.knobs.exits_on_launch {
            core.exit(1 << 8);
        }
        Ok(core)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per management route, as the core's router"
    )]
    async fn answer(&self, call: Call) -> Result<Response, NetError> {
        let silent = self.knobs(|knobs| knobs.silent);
        let core = self
            .0
            .lock()
            .unwrap()
            .launched
            .iter()
            .find(|core| core.port == call.port && core.alive())
            .cloned()
            .filter(|_| !silent)
            .ok_or_else(|| NetError::Connect(io::ErrorKind::ConnectionRefused.into()))?;
        if call.target == "/v1/models" {
            return Ok(
                if call.authorization == format!("Bearer {}", core.client_key) {
                    respond(
                        200,
                        &json!({ "data": [{ "id": "fixture-model", "owned_by": "fixture" }] }),
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
            .unwrap()
            .to_owned();
        self.0.lock().unwrap().calls.push((
            core.profile.clone(),
            call.method.clone(),
            target.clone(),
        ));
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
        let path = url.path().to_owned();
        Ok(match (call.method.clone(), path.as_str()) {
            (Method::GET, "/config") => {
                respond(200, &yaml(&std::fs::read_to_string(&core.config).unwrap()))
            }
            (Method::GET, "/auth-files") => respond(200, &listing(&core.auth)),
            // Nothing is proxied through these cores, so their usage queue stays empty.
            (_, "/usage-queue") => respond(200, &json!([])),
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
/// strings are JSON-quoted, `[]` and `{}` for empty collections.
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

impl Backend for FakeCores {
    fn launch(
        &self,
        binary: &Path,
        data: &Path,
        profile: &ProfileId,
    ) -> io::Result<Box<dyn CoreHandle>> {
        Ok(Box::new(Handle(self.launch_core(binary, data, profile)?)))
    }

    fn send(&self, call: Call) -> BoxFuture<'static, Result<Response, NetError>> {
        let fake = self.clone();
        Box::pin(async move { fake.answer(call).await })
    }

    fn reap_orphans(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// The backend of one data directory, recording its reaping.
pub(crate) struct Directory(pub(crate) FakeCores, pub(crate) PathBuf);

impl Backend for Directory {
    fn launch(
        &self,
        binary: &Path,
        data: &Path,
        profile: &ProfileId,
    ) -> io::Result<Box<dyn CoreHandle>> {
        self.0.launch(binary, data, profile)
    }

    fn send(&self, call: Call) -> BoxFuture<'static, Result<Response, NetError>> {
        self.0.send(call)
    }

    fn reap_orphans(&self) -> BoxFuture<'_, io::Result<()>> {
        self.0.0.lock().unwrap().reaped.push(self.1.clone());
        Box::pin(async { Ok(()) })
    }
}

struct Handle(Arc<Core>);

impl CoreHandle for Handle {
    fn record(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn wait(&mut self) -> BoxFuture<'_, io::Result<ExitStatus>> {
        Box::pin(async {
            let mut exited = self.0.exited.subscribe();
            let raw = *exited.wait_for(Option::is_some).await.unwrap();
            Ok(ExitStatus::from_raw(raw.unwrap_or_default()))
        })
    }

    fn terminate(&mut self) -> BoxFuture<'_, io::Result<ExitStatus>> {
        self.0.exit(15);
        self.wait()
    }
}
