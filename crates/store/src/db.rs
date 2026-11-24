//! The SQLite store, as the TypeScript `Store` kept it: the same schema, PRAGMAs, row ids, SQL
//! and compact JSON bodies, so a TypeScript build opens what this one wrote. [`Db`] lives on
//! the store thread; callers reach it through [`crate::Store::call`].

use std::collections::{HashMap, HashSet};
use std::fs::{self, Permissions};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Arc;
use std::time::SystemTime;

use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId, ProfileSlug};
use nsv_domain::random;
use nsv_domain::registry::{RetentionDays, Strategy, registry};
use nsv_domain::secret::{ApiKey, ClientKey, CoreKey, KeyDigest, ManagementKey};
use nsv_domain::time::{epoch_ms, iso_string};
use nsv_domain::wire::{Account, ApiAccount, Extra, Profile, Quota, UsageRecord};
use nsv_files::{DataDir, EntryKind, own_entry, owner_only_directory};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use rusqlite::{Connection, OptionalExtension, Params, params};
use serde::de::DeserializeOwned;
use tokio::sync::watch;
use unicode_normalization::UnicodeNormalization;

use crate::store::StoreError;
use crate::vault::Vault;

const SCHEMA: &str = "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;
      CREATE TABLE IF NOT EXISTS profiles (id TEXT PRIMARY KEY, slug TEXT UNIQUE NOT NULL, body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS secrets (id TEXT PRIMARY KEY, encrypted TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS api_accounts (id TEXT PRIMARY KEY, profile_id TEXT NOT NULL REFERENCES profiles(id), body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS quotas (profile_id TEXT NOT NULL, account_id TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(profile_id,account_id));
      CREATE TABLE IF NOT EXISTS usage (id TEXT PRIMARY KEY, profile_id TEXT NOT NULL, timestamp TEXT NOT NULL, body TEXT NOT NULL);
      CREATE INDEX IF NOT EXISTS usage_profile_sequence ON usage(profile_id);
      CREATE INDEX IF NOT EXISTS usage_profile_time ON usage(profile_id,timestamp);
      CREATE TABLE IF NOT EXISTS settings (id TEXT PRIMARY KEY, value TEXT NOT NULL);
    ";

// Row ids in the secrets and settings tables. Stored data uses these exact names: a rename
// here loses the owner's keys and settings.
fn secret_id(owner: &str, kind: &str) -> String {
    format!("{owner}:{kind}")
}
fn accounts_setting(profile: &ProfileId) -> String {
    format!("accounts:{profile}")
}
fn reset_request_setting(identity: &str, credit_id: &str) -> Result<String, StoreError> {
    Ok(format!(
        "codex-reset:{}",
        serde_json::to_string(&[identity, credit_id])?
    ))
}
const RETENTION_SETTING: &str = "usage-retention-days";

/// What the scheduler and the tray know of a profile: the store publishes every profile's,
/// in creation order, before it acknowledges a write that changes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileFact {
    pub id: ProfileId,
    pub name: String,
    pub enabled: bool,
    /// API-key accounts.
    pub api: usize,
    /// OAuth accounts, as the last account sync stored them.
    pub oauth: usize,
}

impl ProfileFact {
    /// It has a subscription, whose quota and routing need the core, so the app keeps its core
    /// running and starts it again when it is not. Every way to add one starts the profile, so
    /// a profile with one is a profile the user meant to run.
    #[must_use]
    pub const fn kept(&self) -> bool {
        self.api + self.oauth > 0
    }
}

/// Every profile's facts, in creation order.
pub type ProfileFacts = Arc<[ProfileFact]>;

/// One page of usage rows; `next` continues after it and is `None` after the last page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsagePage<Cursor> {
    pub records: Vec<UsageRecord>,
    pub next: Option<Cursor>,
}

/// A position in time order: rows after `sequence` at `timestamp`, then later timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageCursor {
    pub timestamp: String,
    pub sequence: i64,
}

/// Rows stored up to sequence `through` with `since <= timestamp < before`.
#[derive(Debug, Clone, Copy)]
pub struct UsageRange<'a> {
    pub since: &'a str,
    pub before: &'a str,
    pub through: i64,
}

/// What the gateway needs to accept a client key and forward its request.
#[derive(Debug, Clone)]
pub struct AuthEntry {
    pub profile: ProfileId,
    pub slug: ProfileSlug,
    pub client_key: ClientKey,
    pub core_key: CoreKey,
}

/// Every profile's keys by client-key digest. The store publishes a new snapshot before it
/// acknowledges any profile or secret write, so the gateway never reads SQLite and a deleted
/// or rotated key fails closed at once.
#[derive(Debug)]
pub struct AuthSnapshot(Result<HashMap<KeyDigest, AuthEntry>, Arc<StoreError>>);

impl AuthSnapshot {
    /// Every entry by its client-key digest. An entry's live key still has to be compared with
    /// the presented one.
    ///
    /// # Errors
    /// The error that kept the store from reading the keys.
    pub fn entries(&self) -> Result<&HashMap<KeyDigest, AuthEntry>, &StoreError> {
        self.0.as_ref().map_err(|error| &**error)
    }

    /// The profile's entry: its live client key.
    ///
    /// # Errors
    /// The error that kept the store from reading the keys.
    pub fn profile(&self, profile: &ProfileId) -> Result<Option<&AuthEntry>, &StoreError> {
        Ok(self
            .0
            .as_ref()
            .map_err(|error| &**error)?
            .values()
            .find(|entry| entry.profile == *profile))
    }
}

/// A body column, parsed as the type its reader asks for.
struct Json<T>(T);

impl<T: DeserializeOwned> FromSql for Json<T> {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        serde_json::from_str(value.as_str()?)
            .map(Json)
            .map_err(|error| FromSqlError::Other(Box::new(error)))
    }
}

/// The store's connection and caches. Only the store thread holds it, so each
/// [`crate::Store::call`] closure is one atomic section, as one synchronous run of the
/// TypeScript store was.
pub struct Db {
    connection: Connection,
    vault: Vault,
    page_size: u32,
    usage_generation: u64,
    auth: watch::Sender<Arc<AuthSnapshot>>,
    auth_stale: bool,
    facts: watch::Sender<ProfileFacts>,
    facts_stale: bool,
}

/// What the store publishes, for [`crate::Store`] to hand out.
pub(crate) struct Published {
    pub(crate) auth: watch::Receiver<Arc<AuthSnapshot>>,
    pub(crate) facts: watch::Receiver<ProfileFacts>,
}

impl Db {
    pub(crate) fn open(directory: &DataDir, vault: Vault) -> Result<(Self, Published), StoreError> {
        owner_only_directory(directory.path(), || Ok::<(), StoreError>(()))?;
        let path = directory.database();
        // chmod and SQLite follow a symbolic link, so refuse one before either runs.
        match fs::symlink_metadata(&path) {
            Ok(status) if !own_entry(&status, EntryKind::File, 0) || status.nlink() != 1 => {
                return Err(StoreError::UnsafeDatabase);
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let connection = Connection::open(&path)?;
        fs::set_permissions(&path, Permissions::from_mode(0o600))?;
        connection.execute_batch(SCHEMA)?;
        // Room for every statement below, so none is compiled again (the default keeps 16).
        connection.set_prepared_statement_cache_capacity(32);
        let page_size = registry().usage_page_size;
        let (auth, auth_receiver) = watch::channel(Arc::new(AuthSnapshot(Ok(HashMap::new()))));
        let (facts, facts_receiver) = watch::channel(ProfileFacts::from([]));
        let mut db = Self {
            connection,
            vault,
            page_size,
            usage_generation: 0,
            auth,
            auth_stale: true,
            facts,
            facts_stale: false,
        };
        // The scheduler restores profiles from the first facts: facts it cannot read must not
        // pass for no profiles.
        db.publish_facts(db.read_facts()?);
        db.publish_if_stale();
        Ok((
            db,
            Published {
                auth: auth_receiver,
                facts: facts_receiver,
            },
        ))
    }

    /// Rebuilds the auth snapshot after a profile or secret write, and the profile facts after
    /// a profile or account write. A failure to read the keys publishes the error, so the
    /// gateway refuses every key rather than trusting a stale one; facts that cannot be read
    /// keep their last value, and the next write tries again. Unchanged facts wake no one.
    pub(crate) fn publish_if_stale(&mut self) {
        if self.auth_stale {
            let entries = self.read_auth().map_err(Arc::new);
            self.auth.send_replace(Arc::new(AuthSnapshot(entries)));
            self.auth_stale = false;
        }
        if self.facts_stale {
            self.facts_stale = false;
            match self.read_facts() {
                Ok(facts) => self.publish_facts(facts),
                Err(error) => tracing::error!(%error, "the profile facts could not be read"),
            }
        }
    }

    fn publish_facts(&self, facts: ProfileFacts) {
        self.facts.send_if_modified(|current| {
            let changed = **current != *facts;
            if changed {
                *current = facts;
            }
            changed
        });
    }

    fn read_facts(&self) -> Result<ProfileFacts, StoreError> {
        self.profiles()?
            .into_iter()
            .map(|profile| {
                Ok(ProfileFact {
                    oauth: self.account_metadata(&profile.id)?.len(),
                    api: self.api_accounts(&profile.id)?.len(),
                    id: profile.id,
                    name: profile.name,
                    enabled: profile.enabled,
                })
            })
            .collect()
    }

    fn read_auth(&self) -> Result<HashMap<KeyDigest, AuthEntry>, StoreError> {
        let mut entries = HashMap::new();
        for profile in self.profiles()? {
            let client_key = self.client_key(&profile.id)?;
            let core_key = self.core_key(&profile.id)?;
            entries.insert(
                client_key.digest(),
                AuthEntry {
                    profile: profile.id,
                    slug: profile.slug,
                    client_key,
                    core_key,
                },
            );
        }
        Ok(entries)
    }

    fn transaction<T>(
        &mut self,
        work: impl FnOnce(&mut Self) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = work(self).and_then(|value| {
            self.connection.execute_batch("COMMIT")?;
            Ok(value)
        });
        if result.is_err()
            && let Err(error) = self.connection.execute_batch("ROLLBACK")
        {
            tracing::error!(%error, "a store transaction could not roll back");
        }
        result
    }

    fn page_full(&self, rows: usize) -> bool {
        usize::try_from(self.page_size).is_ok_and(|size| rows >= size)
    }

    fn bodies<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: impl Params,
    ) -> Result<Vec<T>, StoreError> {
        let mut statement = self.connection.prepare_cached(sql)?;
        let rows = statement.query_map(params, |row| row.get::<_, Json<T>>(0))?;
        rows.map(|row| Ok(row?.0)).collect()
    }

    fn value<T: FromSql>(&self, sql: &str, params: impl Params) -> Result<Option<T>, StoreError> {
        Ok(self
            .connection
            .prepare_cached(sql)?
            .query_row(params, |row| row.get(0))
            .optional()?)
    }

    fn run(&self, sql: &str, params: impl Params) -> Result<usize, StoreError> {
        Ok(self.connection.prepare_cached(sql)?.execute(params)?)
    }

    /// # Errors
    /// A database error or a body that is not a profile.
    pub fn profiles(&self) -> Result<Vec<Profile>, StoreError> {
        self.bodies("SELECT body FROM profiles ORDER BY rowid", [])
    }

    /// # Errors
    /// [`AppError`] 404 "Profile not found." for an unknown id.
    pub fn profile(&self, id: &ProfileId) -> Result<Profile, StoreError> {
        self.value::<Json<Profile>>("SELECT body FROM profiles WHERE id=?", [id.as_str()])?
            .map(|Json(profile)| profile)
            .ok_or_else(|| AppError::profile_not_found().into())
    }

    /// Creates a disabled fill-first profile with a unique slug and new client, core and
    /// management keys.
    ///
    /// # Errors
    /// A database, vault or randomness error; nothing is written then.
    pub fn create_profile(&mut self, name: &str) -> Result<Profile, StoreError> {
        let stem = slug_stem(name);
        let profiles = self.profiles()?;
        let taken: HashSet<&str> = profiles.iter().map(|p| p.slug.as_str()).collect();
        let mut slug = stem.clone();
        let mut suffix = 2_u32;
        while taken.contains(slug.as_str()) {
            slug = format!("{stem}-{suffix}");
            suffix += 1;
        }
        let profile = Profile {
            id: ProfileId::parse(&random::uuid()?)?,
            slug: ProfileSlug::parse(&slug)?,
            name: name.to_owned(),
            strategy: Strategy::FillFirst,
            session_affinity: true,
            enabled: false,
            created_at: iso_string(epoch_ms(SystemTime::now())),
            extra: Extra::new(),
        };
        let body = serde_json::to_string(&profile)?;
        let id = profile.id.as_str();
        self.transaction(|db| {
            db.run(
                "INSERT INTO profiles VALUES (?,?,?)",
                params![id, profile.slug.as_str(), body],
            )?;
            let client = ClientKey::generate()?;
            db.set_secret(&secret_id(id, "client"), client.expose_secret())?;
            let core = CoreKey::generate()?;
            db.set_secret(&secret_id(id, "core"), core.expose_secret())?;
            let management = ManagementKey::generate()?;
            db.set_secret(&secret_id(id, "management"), management.expose_secret())?;
            Ok(())
        })?;
        self.facts_stale = true;
        Ok(profile)
    }

    /// Replaces the stored body of `profile.id`.
    ///
    /// # Errors
    /// A database error.
    pub fn save_profile(&mut self, profile: &Profile) -> Result<(), StoreError> {
        self.auth_stale = true;
        self.facts_stale = true;
        self.run(
            "UPDATE profiles SET body=? WHERE id=?",
            params![serde_json::to_string(profile)?, profile.id.as_str()],
        )?;
        Ok(())
    }

    /// Stores a profile's account list; an unchanged list writes nothing.
    ///
    /// # Errors
    /// A database error.
    pub fn save_account_metadata(
        &mut self,
        profile: &ProfileId,
        accounts: &[Account],
    ) -> Result<(), StoreError> {
        let written = self.run(
            "INSERT INTO settings VALUES (?,?) ON CONFLICT(id) DO UPDATE
        SET value=excluded.value WHERE value!=excluded.value",
            params![accounts_setting(profile), serde_json::to_string(accounts)?],
        )?;
        self.facts_stale |= written > 0;
        Ok(())
    }

    /// # Errors
    /// A database error or a stored list that does not parse.
    pub fn account_metadata(&self, profile: &ProfileId) -> Result<Vec<Account>, StoreError> {
        Ok(self
            .value::<Json<Vec<Account>>>(
                "SELECT value FROM settings WHERE id=?",
                [accounts_setting(profile)],
            )?
            .map_or_else(Vec::new, |Json(accounts)| accounts))
    }

    fn secret(&self, id: &str) -> Result<String, StoreError> {
        let encrypted = self
            .value::<String>("SELECT encrypted FROM secrets WHERE id=?", [id])?
            .ok_or_else(|| AppError::new("Credential not found.", 404))?;
        Ok(self.vault.decrypt(&encrypted)?)
    }

    fn set_secret(&mut self, id: &str, value: &str) -> Result<(), StoreError> {
        self.auth_stale = true;
        let encrypted = self.vault.encrypt(value)?;
        self.run(
            "INSERT OR REPLACE INTO secrets VALUES (?,?)",
            params![id, encrypted],
        )?;
        Ok(())
    }

    /// The key agents send for this profile. Never grants management access.
    ///
    /// # Errors
    /// [`AppError`] 404 "Credential not found.", or a value the vault cannot decrypt.
    pub fn client_key(&self, profile: &ProfileId) -> Result<ClientKey, StoreError> {
        Ok(ClientKey::new(
            self.secret(&secret_id(profile.as_str(), "client"))?,
        ))
    }

    /// The key the gateway sends to this profile's core.
    ///
    /// # Errors
    /// As [`Db::client_key`].
    pub fn core_key(&self, profile: &ProfileId) -> Result<CoreKey, StoreError> {
        Ok(CoreKey::new(
            self.secret(&secret_id(profile.as_str(), "core"))?,
        ))
    }

    /// # Errors
    /// As [`Db::client_key`].
    pub fn management_key(&self, profile: &ProfileId) -> Result<ManagementKey, StoreError> {
        Ok(ManagementKey::new(
            self.secret(&secret_id(profile.as_str(), "management"))?,
        ))
    }

    /// # Errors
    /// As [`Db::client_key`].
    pub fn api_key(&self, account: &AccountId) -> Result<ApiKey, StoreError> {
        Ok(ApiKey::new(
            self.secret(&secret_id(account.as_str(), "api"))?,
        ))
    }

    /// The request id of a Codex reset redemption, kept until cleared so a retry reuses it.
    ///
    /// # Errors
    /// A database error.
    pub fn reset_request_id(&self, identity: &str, credit_id: &str) -> Result<String, StoreError> {
        let key = reset_request_setting(identity, credit_id)?;
        self.run(
            "INSERT OR IGNORE INTO settings VALUES (?,?)",
            params![key, random::uuid()?],
        )?;
        Ok(self
            .connection
            .prepare_cached("SELECT value FROM settings WHERE id=?")?
            .query_row([key], |row| row.get(0))?)
    }

    /// # Errors
    /// A database error.
    pub fn clear_reset_request(&self, identity: &str, credit_id: &str) -> Result<(), StoreError> {
        self.run(
            "DELETE FROM settings WHERE id=?",
            [reset_request_setting(identity, credit_id)?],
        )?;
        Ok(())
    }

    /// # Errors
    /// A database error or a body that is not an API-key account.
    pub fn api_accounts(&self, profile: &ProfileId) -> Result<Vec<ApiAccount>, StoreError> {
        self.bodies(
            "SELECT body FROM api_accounts WHERE profile_id=? ORDER BY rowid",
            [profile.as_str()],
        )
    }

    /// Stores an API-key account and, when given, its key, in one transaction. Replacing an
    /// account moves it to the end of the profile's list, as the TypeScript store did.
    ///
    /// # Errors
    /// A database or vault error; nothing is written then.
    pub fn save_api_account(
        &mut self,
        profile: &ProfileId,
        account: &ApiAccount,
        key: Option<&ApiKey>,
    ) -> Result<(), StoreError> {
        let body = serde_json::to_string(account)?;
        self.facts_stale = true;
        self.transaction(|db| {
            db.run(
                "INSERT OR REPLACE INTO api_accounts VALUES (?,?,?)",
                params![account.id.as_str(), profile.as_str(), body],
            )?;
            if let Some(key) = key {
                db.set_secret(&secret_id(account.id.as_str(), "api"), key.expose_secret())?;
            }
            Ok(())
        })
    }

    /// Removes an API-key account with its key and quota.
    ///
    /// # Errors
    /// A database error; nothing is removed then.
    pub fn remove_api_account(
        &mut self,
        profile: &ProfileId,
        account: &AccountId,
    ) -> Result<(), StoreError> {
        self.facts_stale = true;
        self.transaction(|db| {
            db.run(
                "DELETE FROM api_accounts WHERE profile_id=? AND id=?",
                [profile.as_str(), account.as_str()],
            )?;
            db.run(
                "DELETE FROM secrets WHERE id=?",
                [secret_id(account.as_str(), "api")],
            )?;
            db.run(
                "DELETE FROM quotas WHERE profile_id=? AND account_id=?",
                [profile.as_str(), account.as_str()],
            )?;
            Ok(())
        })
    }

    /// # Errors
    /// A database error or a body that is not a quota.
    pub fn quota(
        &self,
        profile: &ProfileId,
        account: &AccountId,
    ) -> Result<Option<Quota>, StoreError> {
        Ok(self
            .value::<Json<Quota>>(
                "SELECT body FROM quotas WHERE profile_id=? AND account_id=?",
                [profile.as_str(), account.as_str()],
            )?
            .map(|Json(quota)| quota))
    }

    /// # Errors
    /// A database error.
    pub fn save_quota(
        &self,
        profile: &ProfileId,
        account: &AccountId,
        quota: &Quota,
    ) -> Result<(), StoreError> {
        self.run(
            "INSERT OR REPLACE INTO quotas VALUES (?,?,?)",
            params![
                profile.as_str(),
                account.as_str(),
                serde_json::to_string(quota)?
            ],
        )?;
        Ok(())
    }

    /// Stores usage rows in one transaction; a row whose id is already stored is skipped.
    ///
    /// # Errors
    /// A database error; no row of `records` is stored then.
    pub fn add_usage(&mut self, records: &[UsageRecord]) -> Result<(), StoreError> {
        if records.is_empty() {
            return Ok(());
        }
        self.transaction(|db| {
            let mut insert = db
                .connection
                .prepare_cached("INSERT OR IGNORE INTO usage VALUES (?,?,?,?)")?;
            for record in records {
                insert.execute(params![
                    record.id,
                    record.profile_id.as_str(),
                    record.timestamp,
                    serde_json::to_string(record)?
                ])?;
            }
            Ok(())
        })
    }

    /// The newest `limit` rows at or after `since`, newest first.
    ///
    /// # Errors
    /// A database error or a body that is not a usage record.
    pub fn usage(
        &self,
        profile: &ProfileId,
        since: &str,
        limit: u32,
    ) -> Result<Vec<UsageRecord>, StoreError> {
        self.bodies(
            "SELECT body FROM usage WHERE profile_id=? AND timestamp>=? ORDER BY timestamp DESC LIMIT ?",
            params![profile.as_str(), since, limit],
        )
    }

    /// The sequence of the newest stored row, 0 when there is none.
    ///
    /// # Errors
    /// A database error.
    pub fn usage_sequence(&self) -> Result<i64, StoreError> {
        Ok(self
            .connection
            .prepare_cached("SELECT coalesce(max(rowid),0) AS sequence FROM usage")?
            .query_row([], |row| row.get(0))?)
    }

    /// Rows stored after sequence `after` and up to `through`, in sequence order.
    ///
    /// # Errors
    /// A database error or a body that is not a usage record.
    pub fn usage_after(
        &self,
        profile: &ProfileId,
        after: i64,
        through: i64,
    ) -> Result<UsagePage<i64>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT rowid AS sequence, body FROM usage INDEXED BY usage_profile_sequence
      WHERE profile_id=? AND rowid>? AND rowid<=? ORDER BY rowid LIMIT ?",
        )?;
        let rows = statement.query_map(
            params![profile.as_str(), after, through, self.page_size],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Json<UsageRecord>>(1)?.0)),
        )?;
        let mut records = Vec::new();
        let mut last = None;
        for row in rows {
            let (sequence, record) = row?;
            last = Some(sequence);
            records.push(record);
        }
        let next = if self.page_full(records.len()) {
            last
        } else {
            None
        };
        Ok(UsagePage { records, next })
    }

    /// Rows in `range`, in time order, starting after `from` (the range's start when `None`).
    ///
    /// # Errors
    /// A database error or a body that is not a usage record.
    pub fn usage_between(
        &self,
        profile: &ProfileId,
        range: UsageRange<'_>,
        from: Option<&UsageCursor>,
    ) -> Result<UsagePage<UsageCursor>, StoreError> {
        let UsageRange {
            since,
            before,
            through,
        } = range;
        let (timestamp, sequence) = match from {
            Some(cursor) if cursor.timestamp.as_str() >= since => {
                (cursor.timestamp.as_str(), cursor.sequence)
            }
            _ => (since, 0),
        };
        // Seek ties by rowid, then later timestamps; a tuple filter rescans the prefix on each page.
        let mut statement = self.connection.prepare_cached(
            "SELECT rowid AS sequence, body, timestamp FROM usage INDEXED BY usage_profile_time
      WHERE profile_id=? AND timestamp=? AND timestamp<? AND rowid>? AND rowid<=?
      UNION ALL
      SELECT rowid AS sequence, body, timestamp FROM usage INDEXED BY usage_profile_time
      WHERE profile_id=? AND timestamp>? AND timestamp<? AND rowid<=?
      ORDER BY timestamp,sequence LIMIT ?",
        )?;
        let rows = statement.query_map(
            params![
                profile.as_str(),
                timestamp,
                before,
                sequence,
                through,
                profile.as_str(),
                timestamp,
                before,
                through,
                self.page_size
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Json<UsageRecord>>(1)?.0,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        let mut records = Vec::new();
        let mut last = None;
        for row in rows {
            let (sequence, record, timestamp) = row?;
            last = Some(UsageCursor {
                timestamp,
                sequence,
            });
            records.push(record);
        }
        let next = if self.page_full(records.len()) {
            last
        } else {
            None
        };
        Ok(UsagePage { records, next })
    }

    /// The history retention; a value [`Db::set_usage_retention_days`] did not write reads as
    /// keeping everything.
    ///
    /// # Errors
    /// A database error.
    pub fn usage_retention_days(&self) -> Result<RetentionDays, StoreError> {
        let value =
            self.value::<String>("SELECT value FROM settings WHERE id=?", [RETENTION_SETTING])?;
        Ok(value
            .and_then(|value| value.trim().parse::<u16>().ok())
            .and_then(|days| RetentionDays::try_from(days).ok())
            .unwrap_or(RetentionDays::All))
    }

    /// Stores the retention and prunes by it now, in one transaction.
    ///
    /// # Errors
    /// A database error; nothing changes then.
    pub fn set_usage_retention_days(&mut self, days: RetentionDays) -> Result<(), StoreError> {
        self.transaction(|db| {
            db.run(
                "INSERT OR REPLACE INTO settings VALUES (?,?)",
                params![RETENTION_SETTING, days.days().to_string()],
            )?;
            db.prune_usage(SystemTime::now())
        })
    }

    /// Deletes rows older than the retention before `now`, and advances
    /// [`Db::usage_generation`] once for each profile that lost rows.
    ///
    /// # Errors
    /// A database error.
    pub fn prune_usage(&mut self, now: SystemTime) -> Result<(), StoreError> {
        let days = self.usage_retention_days()?;
        if days == RetentionDays::All {
            return Ok(());
        }
        let cutoff = iso_string(epoch_ms(now) - i64::from(days.days()) * 86_400_000);
        for profile in self.profiles()? {
            if self.run(
                "DELETE FROM usage WHERE profile_id=? AND timestamp<?",
                params![profile.id.as_str(), cutoff],
            )? > 0
            {
                self.usage_generation += 1;
            }
        }
        Ok(())
    }

    /// Changes whenever retention deletes rows, so a running total can tell it is stale.
    #[must_use]
    pub fn usage_generation(&self) -> u64 {
        self.usage_generation
    }
}

/// A profile name as a URL slug stem, as the TypeScript store derived it: NFKD, combining
/// diacritics removed, lowercase, runs of anything but `a-z0-9` as `-`, the first 40
/// characters, one edge `-` trimmed, `profile` when nothing is left.
fn slug_stem(name: &str) -> String {
    let lowered = name
        .nfkd()
        .filter(|character| !('\u{300}'..='\u{36f}').contains(character))
        .collect::<String>()
        .to_lowercase();
    let mut slug = String::with_capacity(lowered.len());
    let mut in_run = false;
    for character in lowered.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            slug.push(character);
            in_run = false;
        } else if !in_run {
            slug.push('-');
            in_run = true;
        }
    }
    // ASCII only, so 40 bytes are 40 UTF-16 code units, as `slice(0, 40)` counted.
    slug.truncate(40);
    let trimmed = slug.strip_prefix('-').unwrap_or(&slug);
    let trimmed = trimmed.strip_suffix('-').unwrap_or(trimmed);
    if trimmed.is_empty() {
        "profile".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_rollback_keeps_the_transaction_s_own_error() {
        let directory = std::env::temp_dir().join(format!("nsv-rollback-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let data = DataDir::new(directory.as_path());
        let vault = Vault::open(&data).unwrap();
        let (mut db, _auth) = Db::open(&data, vault).unwrap();
        let failed = db.transaction(|db| -> Result<(), StoreError> {
            // Ends the transaction, so the store's own ROLLBACK fails.
            db.connection.execute_batch("ROLLBACK")?;
            Err(StoreError::Closed)
        });
        assert!(matches!(failed, Err(StoreError::Closed)));
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn slugs_follow_the_typescript_derivation() {
        // Names from the TypeScript fixture and their stored slugs.
        assert_eq!(slug_stem("Équipe Café: R&D (été)"), "equipe-cafe-r-d-ete");
        assert_eq!(slug_stem("Personal"), "personal");
        assert_eq!(slug_stem("★ ★ ★"), "profile");
        assert_eq!(slug_stem("-"), "profile");
        assert_eq!(slug_stem("  Ångström 2 "), "angstrom-2");
        assert_eq!(slug_stem("ｆｕｌｌ　ｗｉｄｔｈ"), "full-width");
    }
}
