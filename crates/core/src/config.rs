//! A profile's core `config.yaml`: generated from the store before every core start, and
//! compared with what a running core reports to tell when a change has been applied.

use std::collections::BTreeMap;
use std::fs::{File, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use nsv_domain::endpoint::OPENCODE_SESSION_HEADER;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::js;
use nsv_domain::registry::{Protocol, Provider, Strategy, registry};
use nsv_domain::secret::{ApiKey, CoreKey, ManagementKey};
use nsv_domain::wire::{ApiAccount, Profile};
use nsv_files::owner_only_directory;
use nsv_store::{Store, StoreError};
use rustix::fs::{Mode, OFlags};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::yaml::to_yaml;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelEntry {
    pub name: String,
    pub alias: String,
}

/// An OpenAI-compatible provider: accounts with the same endpoint, prefix and models share one,
/// each key an entry.
#[derive(Debug, Clone)]
pub struct CompatibleEntry {
    pub name: String,
    pub prefix: String,
    pub base_url: String,
    pub headers: BTreeMap<&'static str, String>,
    pub api_keys: Vec<ApiKey>,
    pub models: Vec<ModelEntry>,
}

/// A `claude-api-key` or `codex-api-key` entry: one account's models of that protocol.
#[derive(Debug, Clone)]
pub struct KeyEntry {
    pub api_key: ApiKey,
    pub prefix: String,
    pub base_url: String,
    pub headers: BTreeMap<&'static str, String>,
    pub models: Vec<ModelEntry>,
}

/// The settings of one core that vary; everything else in its config is fixed.
#[derive(Debug, Clone)]
pub struct CoreConfiguration {
    pub port: u16,
    pub auth_dir: String,
    pub core_key: CoreKey,
    pub management_key: ManagementKey,
    pub strategy: Strategy,
    pub session_affinity: bool,
    pub openai_compatibility: Vec<CompatibleEntry>,
    pub claude_api_key: Vec<KeyEntry>,
    pub codex_api_key: Vec<KeyEntry>,
}

/// Headers requests to a preset with `upstream` carry.
fn upstream_headers(upstream: Provider) -> BTreeMap<&'static str, String> {
    match upstream {
        Provider::OpencodeGo => BTreeMap::from([
            (
                "User-Agent",
                format!("{} (coding-agent-gateway)", nsv_net::USER_AGENT),
            ),
            (
                OPENCODE_SESSION_HEADER,
                format!("${OPENCODE_SESSION_HEADER}"),
            ),
        ]),
        _ => BTreeMap::new(),
    }
}

impl CoreConfiguration {
    /// The config of `profile`'s core on `port` with its keys and enabled `accounts`;
    /// `api_key` answers an account's upstream key.
    ///
    /// # Errors
    /// What `api_key` returns, or [`StoreError::Json`] when a model list does not serialize.
    pub fn new(
        profile: &Profile,
        port: u16,
        auth_dir: String,
        (core_key, management_key): (CoreKey, ManagementKey),
        accounts: &[ApiAccount],
        mut api_key: impl FnMut(&AccountId) -> Result<ApiKey, StoreError>,
    ) -> Result<Self, StoreError> {
        let providers = &registry().providers;
        let mut compatible: Vec<CompatibleEntry> = Vec::new();
        let mut claude = Vec::new();
        let mut responses = Vec::new();
        for account in accounts.iter().filter(|account| !account.disabled) {
            let headers = providers
                .get(account.provider)
                .preset
                .as_ref()
                .and_then(|preset| preset.upstream)
                .map_or_else(BTreeMap::new, upstream_headers);
            let mut key: Option<ApiKey> = None;
            for &protocol in Protocol::ALL {
                let models: Vec<ModelEntry> = account
                    .models
                    .iter()
                    .filter(|model| model.protocol == protocol)
                    .map(|model| ModelEntry {
                        name: model.id.clone(),
                        alias: model.id.clone(),
                    })
                    .collect();
                if models.is_empty() {
                    continue;
                }
                let key = match &key {
                    Some(key) => key.clone(),
                    None => key.insert(api_key(&account.id)?).clone(),
                };
                match protocol {
                    Protocol::Openai => {
                        let identity: String =
                            serde_json::to_string(&(&account.base_url, &account.prefix, &models))?;
                        let name = account.provider.compatible(&identity);
                        if let Some(existing) = compatible.iter_mut().find(|p| p.name == name) {
                            existing.api_keys.push(key);
                        } else {
                            compatible.push(CompatibleEntry {
                                name,
                                prefix: account.prefix.clone(),
                                base_url: account.base_url.clone(),
                                headers: headers.clone(),
                                api_keys: vec![key],
                                models,
                            });
                        }
                    }
                    Protocol::Anthropic => claude.push(KeyEntry {
                        api_key: key,
                        prefix: account.prefix.clone(),
                        base_url: without_v1(&account.base_url).to_owned(),
                        headers: headers.clone(),
                        models,
                    }),
                    Protocol::Responses => responses.push(KeyEntry {
                        api_key: key,
                        prefix: account.prefix.clone(),
                        base_url: account.base_url.clone(),
                        headers: headers.clone(),
                        models,
                    }),
                }
            }
        }
        Ok(Self {
            port,
            auth_dir,
            core_key,
            management_key,
            strategy: profile.strategy,
            session_affinity: profile.session_affinity,
            openai_compatibility: compatible,
            claude_api_key: claude,
            codex_api_key: responses,
        })
    }

    /// The whole config as JSON, secrets included, as the file holds it.
    #[must_use]
    pub fn to_json(&self) -> Map<String, Value> {
        let models = |models: &[ModelEntry]| -> Vec<Value> {
            models
                .iter()
                .map(|m| json!({ "name": m.name, "alias": m.alias }))
                .collect()
        };
        let key_entry = |entry: &KeyEntry| {
            json!({
                "api-key": entry.api_key.expose_secret(),
                "prefix": entry.prefix,
                "base-url": entry.base_url,
                "headers": entry.headers,
                "models": models(&entry.models),
            })
        };
        let compatible: Vec<Value> = self
            .openai_compatibility
            .iter()
            .map(|entry| {
                json!({
                    "name": entry.name,
                    "prefix": entry.prefix,
                    "base-url": entry.base_url,
                    "headers": entry.headers,
                    "api-key-entries": entry.api_keys.iter()
                        .map(|key| json!({ "api-key": key.expose_secret() }))
                        .collect::<Vec<_>>(),
                    "models": models(&entry.models),
                })
            })
            .collect();
        let document = json!({
            "host": "127.0.0.1",
            "port": self.port,
            "auth-dir": self.auth_dir,
            "api-keys": [self.core_key.expose_secret()],
            "remote-management": {
                "allow-remote": false,
                "secret-key": self.management_key.expose_secret(),
                "disable-control-panel": true,
                "disable-auto-update-panel": true,
            },
            "debug": false,
            // Skip body-capturing middleware, including error-only capture. This is an upstream
            // performance option, unrelated to the application's license.
            "commercial-mode": true,
            "logging-to-file": false,
            "request-log": false,
            "error-logs-max-files": 10,
            "usage-statistics-enabled": true,
            "redis-usage-queue-retention-seconds": 86400,
            "request-retry": 1,
            "max-retry-interval": 5,
            "ws-auth": true,
            "quota-exceeded": {
                "switch-project": true,
                "switch-preview-model": false,
                "antigravity-credits": false,
            },
            "routing": {
                "strategy": self.strategy.as_str(),
                "session-affinity": self.session_affinity,
                "session-affinity-ttl": "1h",
            },
            "plugins": { "enabled": false },
            "openai-compatibility": compatible,
            "claude-api-key": self.claude_api_key.iter().map(key_entry).collect::<Vec<_>>(),
            "codex-api-key": self.codex_api_key.iter().map(key_entry).collect::<Vec<_>>(),
        });
        match document {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    /// The `config.yaml` text.
    #[must_use]
    pub fn to_yaml(&self) -> String {
        to_yaml(&self.to_json())
    }
}

/// The Anthropic base URL without a trailing `/v1` or `/v1/`: the core appends it.
fn without_v1(base_url: &str) -> &str {
    base_url
        .strip_suffix("/v1/")
        .or_else(|| base_url.strip_suffix("/v1"))
        .unwrap_or(base_url)
}

/// Writes `profile`'s config for a core on `port` and answers it, in one store section: the
/// profile, keys and accounts are read and the file written with nothing in between, so
/// writes for one profile never interleave. The profile and credential directories become
/// owner-only. The file is rewritten in place, owner-only, because the core watches its
/// inode; it is never followed through a symbolic link.
///
/// # Errors
/// [`StoreError::App`] 404 for an unknown profile, before anything is created; a store or
/// file-system error.
pub async fn write_config(
    store: &Store,
    profile: ProfileId,
    port: u16,
) -> Result<CoreConfiguration, StoreError> {
    let directory = store.directory().clone();
    store
        .call(move |db| {
            let record = db.profile(&profile)?;
            let auth = directory.credentials(&profile);
            owner_only_directory(&directory.profile(&profile), || Ok::<(), StoreError>(()))?;
            owner_only_directory(&auth, || Ok::<(), StoreError>(()))?;
            let auth_dir = auth
                .to_str()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the data directory is not UTF-8",
                    )
                })?
                .to_owned();
            let keys = (db.core_key(&profile)?, db.management_key(&profile)?);
            let accounts = db.api_accounts(&profile)?;
            let config = CoreConfiguration::new(&record, port, auth_dir, keys, &accounts, |id| {
                db.api_key(id)
            })?;
            write_in_place(
                &directory.core_config(&profile),
                config.to_yaml().as_bytes(),
            )?;
            Ok(config)
        })
        .await
}

fn write_in_place(path: &Path, content: &[u8]) -> io::Result<()> {
    let descriptor = rustix::fs::open(
        path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    let mut file = File::from(descriptor);
    file.set_permissions(Permissions::from_mode(0o600))?;
    file.write_all(content)
}

/// The entries a running core reloads from a changed file; the rest apply at its next start.
const LIVE_FIELDS: [&str; 4] = [
    "routing",
    "openai-compatibility",
    "claude-api-key",
    "codex-api-key",
];

/// Whether a core's reported config shows every live entry of `expected`.
#[must_use]
pub fn applied(expected: &CoreConfiguration, actual: &Value) -> bool {
    let expected = expected.to_json();
    LIVE_FIELDS
        .iter()
        .all(|key| matches(expected.get(*key), js::field(Some(actual), key)))
}

/// The Go core omits zero values, so `""`, `false`, `0` and `[]` read back as absent.
fn matches(expected: Option<&Value>, actual: Option<&Value>) -> bool {
    match (expected, actual) {
        (Some(Value::Array(items)), _) if items.is_empty() => match actual {
            None | Some(Value::Null) => true,
            Some(Value::Array(reported)) => reported.is_empty(),
            Some(_) => false,
        },
        (Some(Value::Array(items)), Some(Value::Array(reported))) => {
            items.len() == reported.len()
                && items
                    .iter()
                    .zip(reported)
                    .all(|(item, value)| matches(Some(item), Some(value)))
        }
        (Some(Value::Array(_)), _) => false,
        (Some(Value::Object(entries)), _) => entries
            .iter()
            .all(|(key, value)| matches(Some(value), js::field(actual, key))),
        // JavaScript numbers: 1 and 1.0 are the same.
        (Some(Value::Number(a)), Some(Value::Number(b))) => a.as_f64() == b.as_f64(),
        (Some(scalar), None) => {
            scalar == &json!("") || scalar == &json!(false) || scalar.as_f64() == Some(0.0)
        }
        (Some(scalar), Some(reported)) => scalar == reported,
        (None, _) => actual.is_none(),
    }
}
