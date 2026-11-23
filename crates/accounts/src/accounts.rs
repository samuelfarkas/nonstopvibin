//! A profile's OAuth accounts as its core lists them in `/auth-files`, with the seat identity
//! read from Claude credential files (`crate::identity`), and the account list the UI, quota
//! and resets read. A port of `accounts.ts`.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use nsv_core::pool::{MANAGEMENT_TIMEOUT, Pool};
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::js::{nullish, number, or, record, string, text, truthy};
use nsv_domain::registry::{Provider, ProviderEntry, registry};
use nsv_domain::sync::locked;
use nsv_domain::wire::ApiAccount;
use nsv_domain::wire::{Account, AccountKind, AccountStatus, Extra};
use nsv_net::Method;
use nsv_store::layout::credential_directory;
use nsv_store::{Db, Store, StoreError};
use serde_json::{Map, Value};

use crate::identity::{SeatIdentity, claude_identity};
use crate::quota::QuotaSource;

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

/// A credential file of a profile, as `credentialFiles` read it.
#[derive(Debug, Clone)]
pub struct CredentialFile {
    /// The file name, which is the account id.
    pub id: String,
    pub raw: Map<String, Value>,
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
        let mut profiles = locked(&self.profiles);
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
                let organizations = entry(&provider).is_some_and(|entry| entry.organizations);
                (organizations && !truthy(file.get("runtime_only"))).then(|| name.to_owned())
            })
            .collect();
        let directory = credential_directory(self.store.directory(), profile);
        let known = self.with(profile, |state| std::mem::take(&mut state.claude));
        let limit = registry().limits.credential_file_bytes;
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
                let account = oauth_account(file, &identities)?;
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
            .await?;
        Ok(Some(count))
    }

    /// The profile's accounts as the UI lists them: its OAuth accounts as last synced, then its
    /// API-key accounts, each with its stored quota (`Accounts.list`).
    ///
    /// # Errors
    /// A store failure.
    pub async fn list(&self, profile: &ProfileId) -> Result<Vec<Account>, AppError> {
        let profile = profile.clone();
        Ok(self.store.call(move |db| listed(db, &profile)).await?)
    }

    /// A credential file was replaced without the core: its account shows the file's seat and
    /// email from now on.
    ///
    /// # Errors
    /// A store failure.
    pub async fn replaced(
        &self,
        profile: &ProfileId,
        account: &AccountId,
        credential: &Map<String, Value>,
    ) -> Result<(), AppError> {
        let identity = claude_identity(&Value::Object(credential.clone()));
        let email = text(credential.get("email")).map(str::to_owned);
        let (profile, account) = (profile.clone(), account.clone());
        Ok(self
            .store
            .call(move |db| {
                let accounts: Vec<Account> = db
                    .account_metadata(&profile)?
                    .into_iter()
                    .map(|mut listed| {
                        if listed.id == account {
                            listed.account_uuid.clone_from(&identity.account_uuid);
                            listed
                                .organization_uuid
                                .clone_from(&identity.organization_uuid);
                            listed
                                .organization_name
                                .clone_from(&identity.organization_name);
                            listed.email = email.clone().or(listed.email);
                        }
                        listed
                    })
                    .collect();
                db.save_account_metadata(&profile, &accounts)
            })
            .await?)
    }

    /// Every regular `.json` file in the profile's credential directory, read.
    ///
    /// # Errors
    /// 404 for an unknown profile, 400 for a file that cannot be read as a credential, or a
    /// file-system error.
    pub async fn credential_files(
        &self,
        profile: &ProfileId,
    ) -> Result<Vec<CredentialFile>, AppError> {
        let id = profile.clone();
        self.store.call(move |db| db.profile(&id)).await?;
        let directory = credential_directory(self.store.directory(), profile);
        let limit = registry().limits.credential_file_bytes;
        tokio::task::spawn_blocking(move || {
            let listing = match fs::read_dir(&directory) {
                Ok(listing) => listing,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(AppError::new(error.to_string(), 500)),
            };
            let mut files = Vec::new();
            for entry in listing {
                let entry = entry.map_err(|error| AppError::new(error.to_string(), 500))?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let file = entry
                    .file_type()
                    .map_err(|error| AppError::new(error.to_string(), 500))?
                    .is_file();
                if file && has_json_extension(&name) {
                    let raw = read_credential(&credential_file(&directory, &name)?, limit)?;
                    files.push(CredentialFile { id: name, raw });
                }
            }
            Ok(files)
        })
        .await
        .map_err(|error| AppError::new(error.to_string(), 500))?
    }
}

#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "accounts.ts listed only `.json`, as the core does"
)]
fn has_json_extension(name: &str) -> bool {
    name.ends_with(".json")
}

/// An API-key account as `list` shows it.
/// [`Accounts::list`] inside a store call, for a caller that reads more in the same call.
///
/// # Errors
/// A store failure.
pub fn listed(db: &Db, profile: &ProfileId) -> Result<Vec<Account>, StoreError> {
    let oauth = db.account_metadata(profile)?;
    let api = db.api_accounts(profile)?.into_iter().map(api_listing);
    oauth
        .into_iter()
        .chain(api)
        .map(|mut account| {
            account.quota = db.quota(profile, &account.id)?;
            Ok(account)
        })
        .collect()
}

fn api_listing(account: ApiAccount) -> Account {
    Account {
        id: account.id,
        name: account.name,
        account_uuid: None,
        organization_uuid: None,
        organization_name: None,
        provider: account.provider.as_str().to_owned(),
        kind: AccountKind::ApiKey,
        email: None,
        disabled: account.disabled,
        status: if account.disabled {
            AccountStatus::Paused
        } else {
            AccountStatus::Configured
        },
        priority: 0,
        auth_index: None,
        quota: None,
        prefix: Some(account.prefix),
        model_count: Some(u32::try_from(account.models.len()).unwrap_or(u32::MAX)),
        extra: Extra::new(),
    }
}

/// The quota and reset features' view of the core pool and the account list.
#[derive(Clone)]
pub struct PoolSource {
    pub pool: Pool,
    pub accounts: Arc<Accounts>,
}

impl QuotaSource for PoolSource {
    async fn accounts(&self, profile: &ProfileId) -> Result<Vec<Account>, AppError> {
        self.accounts.list(profile).await
    }

    fn chatgpt_account_id(&self, profile: &ProfileId, account: &AccountId) -> Option<String> {
        self.accounts.chatgpt_account_id(profile, account.as_str())
    }

    fn usable(&self, profile: &ProfileId) -> bool {
        self.pool.usable(profile)
    }

    async fn api_call(&self, profile: &ProfileId, body: Value) -> Result<Value, AppError> {
        self.pool
            .management(
                profile,
                &Method::POST,
                "/api-call",
                Some(&body),
                MANAGEMENT_TIMEOUT,
            )
            .await
    }
}

fn entry(provider: &str) -> Option<&'static ProviderEntry> {
    Provider::from_id(provider).map(|provider| registry().providers.get(provider))
}

/// `String(file.provider ?? file.type ?? fallback)`.
fn provider(file: &Map<String, Value>, fallback: &str) -> String {
    nullish(file.get("provider"))
        .or_else(|| nullish(file.get("type")))
        .map_or_else(
            || fallback.to_owned(),
            |value| string(Some(value)).into_owned(),
        )
}

/// One entry of the listing as a stored OAuth account, or nothing for a runtime-only file, an
/// API key or an entry without a name.
fn oauth_account(
    file: &Map<String, Value>,
    identities: &HashMap<String, SeatIdentity>,
) -> Option<Account> {
    if truthy(file.get("runtime_only")) || text(file.get("account_type")) == Some("api_key") {
        return None;
    }
    let id = nullish(file.get("name"))
        .or_else(|| nullish(file.get("id")))
        .map_or_else(String::new, |value| string(Some(value)).into_owned());
    let id = AccountId::parse(&id).ok()?;
    let provider = provider(file, "unknown");
    let identity = identities.get(id.as_str()).cloned().unwrap_or_default();
    // Core status messages: "disabled (invalid grant)", "invalid grant (retrying)",
    // "unauthorized", "token expired".
    let status = if truthy(file.get("disabled")) {
        AccountStatus::Paused
    } else if text(file.get("status")) == Some("error") {
        let message = nullish(file.get("status_message"))
            .map_or_else(String::new, |value| string(Some(value)).into_owned())
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
        name: or(file.get("note"), None).map_or_else(
            || registry().label(&provider),
            |value| string(Some(value)).into_owned(),
        ),
        account_uuid: identity.account_uuid,
        organization_uuid: identity.organization_uuid,
        organization_name: identity.organization_name,
        provider,
        kind: AccountKind::OAuth,
        email: text(file.get("email")).map(str::to_owned),
        disabled: file.get("disabled") == Some(&Value::Bool(true)),
        status,
        priority,
        auth_index: Some(
            nullish(file.get("auth_index"))
                .map_or_else(String::new, |value| string(Some(value)).into_owned()),
        ),
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
