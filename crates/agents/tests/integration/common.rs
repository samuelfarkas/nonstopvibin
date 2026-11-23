//! Scratch directories, profiles, a connection source the tests steer, and file snapshots.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, DirBuilder, Permissions};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use nsv_agents::{AgentDirectories, AgentSetup, Connection, ConnectionSource};
use nsv_domain::error::AppError;
use nsv_domain::secret::ClientKey;
use nsv_domain::wire::Profile;
use serde_json::{Value, json};

/// A fresh directory under the system temporary directory, canonical, removed on drop.
pub(crate) struct Scratch {
    pub(crate) root: PathBuf,
}

impl Scratch {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let name = format!(
            "nsv-agents-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let root = std::env::temp_dir().join(name);
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        Self {
            root: fs::canonicalize(root).unwrap(),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Best effort: a leftover scratch directory fails no behaviour.
        drop(fs::remove_dir_all(&self.root));
    }
}

/// Creates `path` and its parents with `mode`.
pub(crate) fn directory(path: &Path, mode: u32) {
    DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
        .unwrap();
    fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
}

/// Writes `content` to `path` with `mode`, creating parents 0755.
pub(crate) fn file(path: &Path, content: &str, mode: u32) {
    directory(path.parent().unwrap(), 0o755);
    fs::write(path, content).unwrap();
    fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
}

pub(crate) fn profile(id: &str, slug: &str, name: &str) -> Profile {
    serde_json::from_value(json!({
        "id": id,
        "slug": slug,
        "name": name,
        "strategy": "round-robin",
        "sessionAffinity": false,
        "enabled": true,
        "createdAt": "2026-09-06",
    }))
    .unwrap()
}

pub(crate) fn key(letter: char) -> String {
    format!("nv_{}", letter.to_string().repeat(43))
}

/// The running profiles' keys and the gateway port, as the server would answer them.
#[derive(Clone)]
pub(crate) struct Broker(Arc<Mutex<BrokerState>>);

struct BrokerState {
    keys: HashMap<String, String>,
    port: u16,
    refusal: AppError,
}

impl Broker {
    pub(crate) fn new(port: u16, refusal: AppError) -> Self {
        Self(Arc::new(Mutex::new(BrokerState {
            keys: HashMap::new(),
            port,
            refusal,
        })))
    }

    pub(crate) fn start(&self, profile: &Profile, key: &str) {
        self.0
            .lock()
            .unwrap()
            .keys
            .insert(profile.id.to_string(), key.to_owned());
    }

    pub(crate) fn stop(&self, profile: &Profile) {
        self.0.lock().unwrap().keys.remove(profile.id.as_str());
    }

    pub(crate) fn set_port(&self, port: u16) {
        self.0.lock().unwrap().port = port;
    }

    pub(crate) fn source(&self) -> ConnectionSource {
        let state = Arc::clone(&self.0);
        Arc::new(move |id| {
            let state = state.lock().unwrap();
            match state.keys.get(id.as_str()) {
                Some(key) => Ok(Connection {
                    key: ClientKey::new(key.clone()),
                    port: state.port,
                }),
                None => Err(state.refusal.clone()),
            }
        })
    }
}

pub(crate) fn setup(data: &Path, broker: &Broker, directories: AgentDirectories) -> AgentSetup {
    AgentSetup::new(data, broker.source(), directories).unwrap()
}

pub(crate) fn home_only(home: &Path) -> AgentDirectories {
    AgentDirectories {
        home: home.to_owned(),
        codex: None,
        opencode: None,
        pi: None,
    }
}

/// Every ordinary file under `root` (symbolic links skipped): path with `root` replaced by
/// `base` → `{ content, mode }`, as the golden recorder walked them.
pub(crate) fn snapshot(root: &Path, base: &str) -> BTreeMap<String, Value> {
    fn visit(directory: &Path, root: &str, base: &str, files: &mut BTreeMap<String, Value>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            let status = fs::symlink_metadata(&path).unwrap();
            if status.is_dir() {
                visit(&path, root, base, files);
            } else if status.is_file() {
                let name = path.to_string_lossy().replacen(root, base, 1);
                let content = String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned();
                let mode = format!("0{:o}", status.permissions().mode() & 0o777);
                files.insert(name, json!({ "content": content, "mode": mode }));
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, &root.to_string_lossy(), base, &mut files);
    files
}

/// What changed between two snapshots: new or changed files, and `null` for removed ones.
pub(crate) fn changes(
    before: &BTreeMap<String, Value>,
    after: &BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    let mut changed: BTreeMap<String, Value> = after
        .iter()
        .filter(|(path, file)| before.get(*path) != Some(*file))
        .map(|(path, file)| (path.clone(), file.clone()))
        .collect();
    for path in before.keys().filter(|path| !after.contains_key(*path)) {
        changed.insert(path.clone(), Value::Null);
    }
    changed
}

/// Replaces generated values with the goldens' placeholders, longest first, in every string
/// and object key.
pub(crate) struct Placeholders(Vec<(String, String)>);

impl Placeholders {
    pub(crate) fn new(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut pairs: Vec<_> = pairs
            .into_iter()
            .filter(|(from, _)| !from.is_empty())
            .collect();
        pairs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        Self(pairs)
    }

    pub(crate) fn text(&self, value: &str) -> String {
        self.0.iter().fold(value.to_owned(), |text, (from, to)| {
            text.replace(from.as_str(), to)
        })
    }

    pub(crate) fn apply(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.apply(item)).collect())
            }
            Value::Object(object) => Value::Object(
                object
                    .iter()
                    .map(|(key, value)| (self.text(key), self.apply(value)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// Runs `program` with `args` in `cwd`, with only a `PATH`, as an agent's credential hook would.
pub(crate) async fn run(program: &str, args: &[&str], cwd: &Path) -> std::process::Output {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin");
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

pub(crate) fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap()
}
