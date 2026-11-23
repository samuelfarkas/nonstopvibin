//! A profile's OAuth accounts as its core lists them in `/auth-files`, with the seat identity
//! read from Claude credential files. A port of `accounts.ts` (sync and credential reads) and
//! `identity.ts` (`claudeIdentity`, `sameSeat`).

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use nsv_core::pool::{MANAGEMENT_TIMEOUT, Pool};
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId, is_uuid};
use nsv_domain::registry::{Provider, Registry, registry};
use nsv_domain::wire::{Account, AccountKind, AccountStatus, Extra};
use nsv_net::Method;
use nsv_store::Store;
use nsv_store::layout::credential_directory;
use serde_json::{Map, Value};

use crate::js::{defined, number, or, record, string, text, truthy};

/// Who a Claude credential belongs to. Missing identity never matches another seat.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeatIdentity {
    pub account_uuid: Option<String>,
    pub organization_uuid: Option<String>,
    pub organization_name: Option<String>,
}

/// `claudeIdentity`: the trimmed, lowercased UUIDs and a trimmed name of 1–200 UTF-16 units
/// without control characters, each only when valid.
#[must_use]
pub fn claude_identity(value: &Value) -> SeatIdentity {
    let raw = record(Some(value));
    let uuid = |key: &str| {
        let value = js_trim(text(raw.get(key))?);
        is_uuid(value).then(|| value.to_lowercase())
    };
    let name = text(raw.get("organization_name"))
        .map(js_trim)
        .filter(|name| {
            let length = name.encode_utf16().count();
            (1..=200).contains(&length) && !name.chars().any(char::is_control)
        });
    SeatIdentity {
        account_uuid: uuid("account_uuid"),
        organization_uuid: uuid("organization_uuid"),
        organization_name: name.map(str::to_owned),
    }
}

/// `sameSeat`: both UUIDs present and equal.
#[must_use]
pub fn same_seat(a: &SeatIdentity, b: &SeatIdentity) -> bool {
    a.account_uuid.is_some()
        && a.organization_uuid.is_some()
        && a.account_uuid == b.account_uuid
        && a.organization_uuid == b.organization_uuid
}

/// JavaScript's `trim`: white space, line terminators and the byte order mark.
fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// A profile's credential file `name`.
///
/// # Errors
/// 400 "Unsupported credential filename." unless `name` is a `.json` name without a path.
pub fn credential_path(data: &Path, profile: &ProfileId, name: &str) -> Result<PathBuf, AppError> {
    credential_file(&credential_directory(data, profile), name)
}

#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "accounts.ts refused `x.JSON`, and the core reads only `.json`"
)]
fn credential_file(directory: &Path, name: &str) -> Result<PathBuf, AppError> {
    if !name.ends_with(".json") || name.contains(['/', '\\']) || name == ".json" {
        return Err(AppError::new("Unsupported credential filename.", 400));
    }
    Ok(directory.join(name))
}

/// Reads a credential file: a regular file of at most `limit` bytes holding JSON. Blocking.
///
/// # Errors
/// 400 for anything else, or when it cannot be read.
pub fn read_credential(path: &Path, limit: u64) -> Result<Map<String, Value>, AppError> {
    let unsupported = || AppError::new("The credential is not a supported regular file.", 400);
    let metadata = fs::symlink_metadata(path).map_err(|_| unsupported())?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(unsupported());
    }
    let unreadable = || {
        AppError::new(
            "A saved credential could not be read. Reconnect that subscription.",
            400,
        )
    };
    let content = fs::read_to_string(path).map_err(|_| unreadable())?;
    let value: Value = serde_json::from_str(&content).map_err(|_| unreadable())?;
    Ok(record(Some(&value)).clone())
}

/// A Claude credential file's identity, keyed by the file's version.
#[derive(Debug, Clone)]
struct Version {
    modified: Option<SystemTime>,
    len: u64,
    identity: SeatIdentity,
}

#[derive(Debug, Default)]
struct Profile {
    claude: HashMap<String, Version>,
    chatgpt: HashMap<String, String>,
    oauth: Option<usize>,
}

/// The accounts of every profile: syncs from cores and what the last sync learned.
pub struct Accounts {
    store: Store,
    profiles: Mutex<HashMap<ProfileId, Profile>>,
}

impl Accounts {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self {
            store,
            profiles: Mutex::new(HashMap::new()),
        }
    }

    fn with<T>(&self, profile: &ProfileId, read: impl FnOnce(&mut Profile) -> T) -> T {
        let mut profiles = self.profiles.lock().unwrap_or_else(PoisonError::into_inner);
        read(profiles.entry(profile.clone()).or_default())
    }

    /// The `ChatGPT` account id of a Codex credential file, which Codex quota and reset calls
    /// need, as the last sync read it.
    #[must_use]
    pub fn chatgpt_account_id(&self, profile: &ProfileId, account: &str) -> Option<String> {
        self.with(profile, |state| state.chatgpt.get(account).cloned())
    }

    /// How many OAuth accounts the profile has, as last synced or recorded.
    #[must_use]
    pub fn oauth_count(&self, profile: &ProfileId) -> Option<usize> {
        self.with(profile, |state| state.oauth)
    }

    /// Records how many OAuth accounts the profile has, as stored.
    pub fn set_oauth_count(&self, profile: &ProfileId, count: usize) {
        self.with(profile, |state| state.oauth = Some(count));
    }

    /// Reads the profile's account list from its usable core and stores it; unchanged metadata
    /// is not rewritten. Claude identities are read again only from credential files that
    /// changed, and credential files are made owner-only where they are not. A listing from a
    /// core that was replaced meanwhile is dropped. Answers how many OAuth accounts were stored,
    /// or `None` when nothing was.
    ///
    /// # Errors
    /// The core's error, 502 for a listing without a `files` array, or a file-system error
    /// from the credential directory.
    pub async fn sync(&self, pool: &Pool, profile: &ProfileId) -> Result<Option<usize>, AppError> {
        let before = pool.state();
        if !before.usable(profile) {
            return Ok(None);
        }
        let core = before.core(profile).map(|core| core.id);
        let registry = registry().map_err(|error| AppError::new(error.to_string(), 500))?;
        let response = pool
            .management(
                profile,
                &Method::GET,
                "/auth-files",
                None,
                MANAGEMENT_TIMEOUT,
            )
            .await?;
        let files = record(Some(&response))
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| AppError::new("The proxy returned an unexpected account list.", 502))?;
        let named: Vec<String> = files
            .iter()
            .filter_map(|value| {
                let file = record(Some(value));
                let provider = provider(file, "undefined");
                let name = text(file.get("name")).filter(|name| !name.is_empty())?;
                let organizations =
                    entry(registry, &provider).is_some_and(|entry| entry.organizations);
                (organizations && !truthy(file.get("runtime_only"))).then(|| name.to_owned())
            })
            .collect();
        let directory = credential_directory(self.store.directory(), profile);
        let known = self.with(profile, |state| std::mem::take(&mut state.claude));
        let limit = registry.limits.credential_file_bytes;
        let (identities, parsed, repaired) =
            tokio::task::spawn_blocking(move || scan(&directory, &named, &known, limit))
                .await
                .map_err(|error| AppError::new(error.to_string(), 500))?;
        self.with(profile, |state| state.claude = parsed);
        repaired.map_err(|error| AppError::new(error.to_string(), 500))?;
        let mut chatgpt = HashMap::new();
        let accounts: Vec<Account> = files
            .iter()
            .filter_map(|value| {
                let file = record(Some(value));
                let account = oauth_account(registry, file, &identities)?;
                let id_token = record(file.get("id_token"));
                if let Some(id) =
                    text(id_token.get("chatgpt_account_id")).filter(|id| !id.is_empty())
                {
                    chatgpt.insert(account.id.as_str().to_owned(), id.to_owned());
                }
                Some(account)
            })
            .collect();
        // A listing from a core that was replaced meanwhile must not overwrite its successor's.
        if pool.state().core(profile).map(|core| core.id) != core {
            return Ok(None);
        }
        let count = accounts.len();
        self.with(profile, |state| {
            state.chatgpt = chatgpt;
            state.oauth = Some(count);
        });
        let id = profile.clone();
        self.store
            .call(move |db| db.save_account_metadata(&id, &accounts))
            .await
            .map_err(|error| match error {
                nsv_store::StoreError::App(error) => error,
                other => AppError::new(other.to_string(), 500),
            })?;
        Ok(Some(count))
    }
}

fn entry<'a>(
    registry: &'a Registry,
    provider: &str,
) -> Option<&'a nsv_domain::registry::ProviderEntry> {
    Provider::from_id(provider).map(|provider| registry.providers.get(provider))
}

/// `String(file.provider ?? file.type ?? fallback)`.
fn provider(file: &Map<String, Value>, fallback: &str) -> String {
    defined(file.get("provider"))
        .or_else(|| defined(file.get("type")))
        .map_or_else(|| fallback.to_owned(), string)
}

/// `providerLabel`: the registry's label, else a compatible provider's, else the id.
fn label(registry: &Registry, provider: &str) -> String {
    entry(registry, provider).map_or_else(
        || {
            if provider.starts_with("openai-compatible-") {
                "Compatible API".to_owned()
            } else {
                provider.to_owned()
            }
        },
        |entry| entry.label.clone(),
    )
}

/// One entry of the listing as a stored OAuth account, or nothing for a runtime-only file, an
/// API key or an entry without a name.
fn oauth_account(
    registry: &Registry,
    file: &Map<String, Value>,
    identities: &HashMap<String, SeatIdentity>,
) -> Option<Account> {
    if truthy(file.get("runtime_only")) || text(file.get("account_type")) == Some("api_key") {
        return None;
    }
    let id = defined(file.get("name"))
        .or_else(|| defined(file.get("id")))
        .map_or_else(String::new, string);
    let id = AccountId::parse(&id).ok()?;
    let provider = provider(file, "unknown");
    let identity = identities.get(id.as_str()).cloned().unwrap_or_default();
    // Core status messages: "disabled (invalid grant)", "invalid grant (retrying)",
    // "unauthorized", "token expired".
    let status = if truthy(file.get("disabled")) {
        AccountStatus::Paused
    } else if text(file.get("status")) == Some("error") {
        let message = defined(file.get("status_message"))
            .map_or_else(String::new, string)
            .to_ascii_lowercase();
        if message.contains("invalid grant") || message.contains("unauthorized") {
            AccountStatus::ReconnectNeeded
        } else {
            AccountStatus::NeedsAttention
        }
    } else if truthy(file.get("unavailable")) {
        AccountStatus::CoolingDown
    } else {
        AccountStatus::Ready
    };
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a routing priority; a fraction is dropped and anything larger saturates"
    )]
    let priority = number(file.get("priority")).unwrap_or(0.0) as i64;
    Some(Account {
        id,
        name: or(file.get("note"), None).map_or_else(|| label(registry, &provider), string),
        account_uuid: identity.account_uuid,
        organization_uuid: identity.organization_uuid,
        organization_name: identity.organization_name,
        provider,
        kind: AccountKind::OAuth,
        email: text(file.get("email")).map(str::to_owned),
        disabled: file.get("disabled") == Some(&Value::Bool(true)),
        status,
        priority,
        auth_index: Some(defined(file.get("auth_index")).map_or_else(String::new, string)),
        quota: None,
        prefix: None,
        model_count: None,
        extra: Extra::new(),
    })
}

/// Reads the identity of each named Claude credential whose file changed since `known`, then
/// makes every credential file owner-only where it is not: a no-op chmod still writes inode
/// metadata. Blocking.
fn scan(
    directory: &Path,
    names: &[String],
    known: &HashMap<String, Version>,
    limit: u64,
) -> (
    HashMap<String, SeatIdentity>,
    HashMap<String, Version>,
    io::Result<()>,
) {
    let mut identities = HashMap::new();
    let mut parsed = HashMap::new();
    for name in names {
        let read = || -> Result<Version, AppError> {
            let path = credential_file(directory, name)?;
            // Stat before reading: a later rewrite always shows a newer version.
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| AppError::new(error.to_string(), 500))?;
            let (modified, len) = (metadata.modified().ok(), metadata.len());
            if let Some(cached) = known.get(name)
                && cached.modified == modified
                && cached.len == len
            {
                return Ok(cached.clone());
            }
            let identity = claude_identity(&Value::Object(read_credential(&path, limit)?));
            Ok(Version {
                modified,
                len,
                identity,
            })
        };
        match read() {
            Ok(version) => {
                identities.insert(name.clone(), version.identity.clone());
                parsed.insert(name.clone(), version);
            }
            // Display enrichment is optional: a refresh can replace a file during a read. The
            // row shows an unverified identity until a later sync succeeds.
            Err(_) => {
                identities.insert(name.clone(), SeatIdentity::default());
            }
        }
    }
    (identities, parsed, repair_modes(directory))
}

fn repair_modes(directory: &Path) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if fs::symlink_metadata(&path)?.permissions().mode() & 0o777 != 0o600 {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}
