//! A profile's OAuth accounts as its core lists them in `/auth-files`, and the account list the
//! UI, quota and resets read. A port of `accounts.ts`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use nsv_core::pool::Pool;
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::js::{nullish, number, or, record, string, text, truthy};
use nsv_domain::registry::registry;
use nsv_domain::sync::locked;
use nsv_domain::wire::ApiAccount;
use nsv_domain::wire::{Account, AccountKind, AccountStatus, Extra};
use nsv_files::DataDir;
use nsv_net::Method;
use nsv_store::{Db, Store, StoreError};
use serde_json::{Map, Value};

/// A profile's credential file `name`.
///
/// # Errors
/// 400 "Unsupported credential filename." unless `name` is a `.json` name without a path.
pub fn credential_path(
    data: &DataDir,
    profile: &ProfileId,
    name: &str,
) -> Result<PathBuf, AppError> {
    credential_file(&data.credentials(profile), name)
}

pub(crate) fn unsupported_filename() -> AppError {
    AppError::new("Unsupported credential filename.", 400)
}

#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "accounts.ts refused `x.JSON`, and the core reads only `.json`"
)]
fn credential_file(directory: &Path, name: &str) -> Result<PathBuf, AppError> {
    if !name.ends_with(".json") || name.contains(['/', '\\']) || name == ".json" {
        return Err(unsupported_filename());
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
#[derive(Clone)]
pub struct CredentialFile {
    /// The file name, which is the account id.
    pub id: String,
    pub raw: Map<String, Value>,
}

/// Only the id: `raw` holds the account's OAuth tokens.
impl std::fmt::Debug for CredentialFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialFile")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct Profile {
    chatgpt: HashMap<String, String>,
    /// Accounts the core keeps from requests for a quota reason.
    quota_cooldowns: HashSet<String>,
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

    /// Whether the core held a quota cooldown for the account when the last sync read it.
    #[must_use]
    pub fn quota_cooldown(&self, profile: &ProfileId, account: &str) -> bool {
        self.with(profile, |state| state.quota_cooldowns.contains(account))
    }

    /// Reads the profile's account list from its usable core and stores it; unchanged metadata
    /// is not rewritten, and credential files are made owner-only where they are not. A listing
    /// from a core that was replaced meanwhile is dropped. Answers how many OAuth accounts were stored,
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
            .management(profile, &Method::GET, "/auth-files", None)
            .await?;
        let files = record(Some(&response))
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| AppError::new("The proxy returned an unexpected account list.", 502))?;
        let directory = self.store.directory().credentials(profile);
        tokio::task::spawn_blocking(move || repair_modes(&directory))
            .await
            .map_err(AppError::internal)?
            .map_err(AppError::internal)?;
        let mut chatgpt = HashMap::new();
        let mut quota_cooldowns = HashSet::new();
        let accounts: Vec<Account> = files
            .iter()
            .filter_map(|value| {
                let file = record(Some(value));
                let account = oauth_account(file)?;
                let id_token = record(file.get("id_token"));
                if let Some(id) =
                    text(id_token.get("chatgpt_account_id")).filter(|id| !id.is_empty())
                {
                    chatgpt.insert(account.id.as_str().to_owned(), id.to_owned());
                }
                if quota_cooldown(file) {
                    quota_cooldowns.insert(account.id.as_str().to_owned());
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
            state.quota_cooldowns = quota_cooldowns;
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
        let directory = self.store.directory().credentials(profile);
        let limit = registry().limits.credential_file_bytes;
        tokio::task::spawn_blocking(move || {
            let listing = match fs::read_dir(&directory) {
                Ok(listing) => listing,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(AppError::internal(error)),
            };
            let mut files = Vec::new();
            for entry in listing {
                let entry = entry.map_err(AppError::internal)?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let file = entry.file_type().map_err(AppError::internal)?.is_file();
                if file && has_json_extension(&name) {
                    let raw = read_credential(&credential_file(&directory, &name)?, limit)?;
                    files.push(CredentialFile { id: name, raw });
                }
            }
            Ok(files)
        })
        .await
        .map_err(AppError::internal)?
    }
}

/// The credential type of a Claude sign-in. Claude subscriptions are served by Claude Agent
/// (`docs/decisions/claude-agent.md`): a core that loaded one would present it to Anthropic
/// dressed up as Claude Code.
const RETIRED_CLAUDE: &str = "claude";

/// Deletes every profile's Claude sign-in credentials and drops their accounts from the
/// stored lists. Startup runs it before any core starts.
///
/// # Errors
/// A store failure, or a credential directory or file that cannot be read or deleted.
pub(crate) async fn retire_claude_sign_ins(store: &Store) -> Result<(), AppError> {
    for profile in store.call(|db| db.profiles()).await? {
        let directory = store.directory().credentials(&profile.id);
        tokio::task::spawn_blocking(move || delete_claude_credentials(&directory))
            .await
            .map_err(AppError::internal)??;
        let id = profile.id;
        store
            .call(move |db| {
                let mut accounts = db.account_metadata(&id)?;
                accounts.retain(|account| account.provider != RETIRED_CLAUDE);
                db.save_account_metadata(&id, &accounts)
            })
            .await?;
    }
    Ok(())
}

/// Blocking.
fn delete_claude_credentials(directory: &Path) -> Result<(), AppError> {
    let listing = match fs::read_dir(directory) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(AppError::internal(error)),
    };
    for entry in listing {
        let entry = entry.map_err(AppError::internal)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type().map_err(AppError::internal)?.is_file() || !has_json_extension(&name) {
            continue;
        }
        let path = entry.path();
        let content = fs::read(&path).map_err(AppError::internal)?;
        // A file that is not JSON is one the core cannot load either.
        let Ok(raw) = serde_json::from_slice::<Value>(&content) else {
            continue;
        };
        if text(raw.get("type")) == Some(RETIRED_CLAUDE) {
            fs::remove_file(&path).map_err(AppError::internal)?;
        }
    }
    Ok(())
}

#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "accounts.ts listed only `.json`, as the core does"
)]
fn has_json_extension(name: &str) -> bool {
    name.ends_with(".json")
}

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

/// An API-key account as `list` shows it.
fn api_listing(account: ApiAccount) -> Account {
    Account {
        id: account.id,
        name: account.name,
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

/// `String(file.provider ?? file.type ?? fallback)`.
fn provider(file: &Map<String, Value>, fallback: &str) -> String {
    nullish(file.get("provider"))
        .or_else(|| nullish(file.get("type")))
        .map_or_else(
            || fallback.to_owned(),
            |value| string(Some(value)).into_owned(),
        )
}

/// Whether a listing entry's `cooldowns` hold one for a quota reason, the credential's or a
/// model's (`CooldownSnapshotForAuth` in the core).
fn quota_cooldown(file: &Map<String, Value>) -> bool {
    file.get("cooldowns")
        .and_then(Value::as_array)
        .is_some_and(|cooldowns| {
            cooldowns.iter().any(|cooldown| {
                matches!(
                    text(record(Some(cooldown)).get("reason")),
                    Some("quota" | "credential_quota")
                )
            })
        })
}

/// One entry of the listing as a stored OAuth account, or nothing for a runtime-only file, an
/// API key or an entry without a name.
fn oauth_account(file: &Map<String, Value>) -> Option<Account> {
    if truthy(file.get("runtime_only")) || text(file.get("account_type")) == Some("api_key") {
        return None;
    }
    let id = nullish(file.get("name"))
        .or_else(|| nullish(file.get("id")))
        .map_or_else(String::new, |value| string(Some(value)).into_owned());
    let id = AccountId::parse(&id).ok()?;
    let provider = provider(file, "unknown");
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

/// Makes every credential file owner-only where it is not: a no-op chmod still writes inode
/// metadata. Blocking.
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_credential_file_debugs_as_its_id_without_its_tokens() {
        let raw =
            json!({ "access_token": "synthetic-access", "refresh_token": "synthetic-refresh" });
        let file = CredentialFile {
            id: "claude.json".to_owned(),
            raw: record(Some(&raw)).clone(),
        };
        assert_eq!(
            format!("{file:?}"),
            r#"CredentialFile { id: "claude.json", .. }"#
        );
    }
}
