//! Credential changes (`src/server/credentials.ts`): imported and reconnected OAuth credentials,
//! API-key accounts, and account edits and removals. They run one at a time across all
//! profiles, each to its end even if its caller stops waiting.
//!
//! - Duplicates are found within one profile only: the same refresh or access token, the same
//!   Claude seat, or the same API key at the same base URL. Several profiles may share one.
//! - A reconnect replaces the credential file with the profile's core stopped, so the core's own
//!   token refresh cannot overwrite it, and keeps the file name, label, priority, prefix and
//!   paused state. A stopped profile stays stopped.
//! - A new credential goes through the core's management upload, then the account list is read
//!   again and the account's quota checked.

use std::future::Future;
use std::sync::Arc;

use nsv_core::pool::{MANAGEMENT_TIMEOUT, Pool};
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::js::{encode_uri_component, text};
use nsv_domain::registry::{OAuthProvider, Provider, registry};
use nsv_domain::secret::same_secret;
use nsv_domain::wire::{Account, AccountKind, AccountPatch, ApiAccountInput, OAuthAction};
use nsv_net::Method;
use nsv_store::Store;
use nsv_store::files::write_atomic;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::accounts::{Accounts, CredentialFile, PoolSource, credential_path};
use crate::api_accounts::api_account;
use crate::fetch::Fetch;
use crate::identity::{claude_identity, same_seat};
use crate::quota::Quotas;
use crate::scheduler::Scheduler;

const NOT_FOUND: &str = "Account not found in this profile.";

/// A non-empty token field of a credential.
fn token_of<'a>(credential: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    text(credential.get(key)).filter(|token| !token.is_empty())
}

/// How a signed-in seat relates to the profile's saved ones (`matchSeat`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatMatch {
    /// No saved seat matches: it becomes a new account.
    Add,
    /// It replaces this account's credential.
    Reconnect(AccountId),
    /// It cannot be connected; the message says why.
    Blocked(&'static str),
}

impl SeatMatch {
    /// The review's action and message.
    #[must_use]
    pub fn review(&self) -> (OAuthAction, Option<String>) {
        match self {
            Self::Add => (OAuthAction::Add, None),
            Self::Reconnect(_) => (OAuthAction::Reconnect, None),
            Self::Blocked(message) => (OAuthAction::Blocked, Some((*message).to_owned())),
        }
    }
}

/// The app's credential changes. Clones share one queue.
pub struct Credentials<F>(Arc<Inner<F>>);

impl<F> Clone for Credentials<F> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

struct Inner<F> {
    store: Store,
    scheduler: Scheduler,
    accounts: Arc<Accounts>,
    quotas: Arc<Quotas<PoolSource, F>>,
    queue: Arc<Mutex<()>>,
}

/// Runs `work` in a task of its own once `turn` is held, so it ends even if its caller stops
/// waiting; the turn ends with it.
pub(crate) async fn detached<T, G>(
    turn: G,
    work: impl Future<Output = Result<T, AppError>> + Send + 'static,
) -> Result<T, AppError>
where
    T: Send + 'static,
    G: Send + 'static,
{
    tokio::spawn(async move {
        let result = work.await;
        drop(turn);
        result
    })
    .await
    .map_err(|error| AppError::new(error.to_string(), 500))?
}

impl<F: Fetch + 'static> Credentials<F> {
    #[must_use]
    pub fn new(
        store: Store,
        scheduler: Scheduler,
        accounts: Arc<Accounts>,
        quotas: Arc<Quotas<PoolSource, F>>,
    ) -> Self {
        Self(Arc::new(Inner {
            store,
            scheduler,
            accounts,
            quotas,
            queue: Arc::new(Mutex::new(())),
        }))
    }

    fn pool(&self) -> &Pool {
        self.0.scheduler.pool()
    }

    /// Runs `change` after every change queued before it.
    ///
    /// # Errors
    /// What `change` answers.
    pub async fn queue<T: Send + 'static>(
        &self,
        change: impl Future<Output = Result<T, AppError>> + Send + 'static,
    ) -> Result<T, AppError> {
        let turn = Arc::clone(&self.0.queue).lock_owned().await;
        detached(turn, change).await
    }

    /// Waits for the changes queued so far.
    pub async fn settled(&self) {
        drop(self.0.queue.lock().await);
    }

    /// `POST …/import` and `…/import-source`: adds a `CLIProxyAPI` OAuth credential file.
    ///
    /// # Errors
    /// See [`Credentials::import_credential`].
    pub async fn import(
        &self,
        profile: &ProfileId,
        raw: Map<String, Value>,
    ) -> Result<Account, AppError> {
        let (this, profile) = (self.clone(), profile.clone());
        self.queue(async move { this.import_credential(&profile, raw, None).await })
            .await
    }

    /// How `credential`, a seat of `provider`, relates to the profile's saved seats; with
    /// `reconnect`, it must be that account's seat.
    ///
    /// # Errors
    /// A saved credential that cannot be read, or 404 for an unknown profile.
    pub async fn match_seat(
        &self,
        profile: &ProfileId,
        provider: OAuthProvider,
        credential: &Map<String, Value>,
        reconnect: Option<&AccountId>,
    ) -> Result<SeatMatch, AppError> {
        let identity = claude_identity(&Value::Object(credential.clone()));
        let files = self.0.accounts.credential_files(profile).await?;
        let of_provider =
            |file: &CredentialFile| text(file.raw.get("type")) == Some(provider.as_str());
        let seat = |file: &CredentialFile| {
            same_seat(
                &identity,
                &claude_identity(&Value::Object(file.raw.clone())),
            )
        };
        if let Some(reconnect) = reconnect {
            let Some(target) = files
                .iter()
                .find(|file| file.id == reconnect.as_str() && of_provider(file))
            else {
                return Ok(SeatMatch::Blocked(
                    "This subscription was removed. Start a new connection.",
                ));
            };
            if !seat(target) {
                return Ok(SeatMatch::Blocked(
                    "This is not the organization and account saved for this subscription, or its old identity is unavailable. Choose the original organization, or connect this seat separately.",
                ));
            }
        }
        let matches: Vec<&CredentialFile> = files
            .iter()
            .filter(|file| of_provider(file) && seat(file))
            .collect();
        Ok(match matches.as_slice() {
            [] => SeatMatch::Add,
            [existing] => SeatMatch::Reconnect(
                AccountId::parse(&existing.id)
                    .map_err(|_| AppError::new("Unsupported credential filename.", 400))?,
            ),
            _ => SeatMatch::Blocked(
                "This profile already has multiple copies of this seat. Remove the extra copies before reconnecting.",
            ),
        })
    }

    /// Adds a credential, or replaces `reconnect`'s. The caller holds the queue.
    ///
    /// # Errors
    /// 400 for a file without a supported type and token, 409 for a duplicate within the
    /// profile or a reconnect whose account is gone, 503 when the core does not list the new
    /// account, or the core's, store's or quota check's error.
    pub async fn import_credential(
        &self,
        profile: &ProfileId,
        raw: Map<String, Value>,
        reconnect: Option<AccountId>,
    ) -> Result<Account, AppError> {
        let registry = registry().map_err(|error| AppError::new(error.to_string(), 500))?;
        let provider = text(raw.get("type")).unwrap_or_default().to_owned();
        let entry = Provider::from_id(&provider).map(|known| registry.providers.get(known));
        let access = text(raw.get("access_token"));
        let token = raw
            .get("token")
            .is_some_and(|token| token.is_object() || token.is_array());
        if !entry.is_some_and(|entry| entry.credential_file) || (access.is_none() && !token) {
            return Err(AppError::new(
                "Choose a CLIProxyAPI OAuth JSON file. It must contain a supported type and token.",
                400,
            ));
        }
        let files = self.0.accounts.credential_files(profile).await?;
        let prior = match &reconnect {
            Some(id) => Some(
                files
                    .iter()
                    .find(|file| file.id == id.as_str())
                    .ok_or_else(|| {
                        AppError::new("The subscription to reconnect no longer exists.", 409)
                    })?,
            ),
            None => None,
        };
        let organizations = entry.is_some_and(|entry| entry.organizations);
        self.refuse_duplicate(profile, &provider, organizations, &raw, &files, prior)
            .await?;
        let name = if let Some(id) = &reconnect {
            id.clone()
        } else {
            let uuid = nsv_domain::random::uuid()
                .map_err(|error| AppError::new(error.to_string(), 500))?;
            AccountId::parse(&format!("{provider}-{uuid}.json"))
                .map_err(|error| AppError::new(error.to_string(), 500))?
        };
        let mut payload = raw.clone();
        let disabled = prior.map_or(&raw, |prior| &prior.raw).get("disabled") == Some(&json!(true));
        payload.insert("disabled".to_owned(), disabled.into());
        // Reconnecting keeps the label, priority and prefix the user set on the old file.
        if let Some(prior) = prior {
            for key in ["note", "priority", "prefix"] {
                match prior.raw.get(key) {
                    Some(value) => payload.insert(key.to_owned(), value.clone()),
                    None => payload.shift_remove(key),
                };
            }
        }
        let pool = self.pool();
        if prior.is_some() {
            let path = credential_path(self.0.store.directory(), profile, name.as_str())?;
            let accounts = Arc::clone(&self.0.accounts);
            let (id, account) = (profile.clone(), name.clone());
            // A core's own token refresh can overwrite a management upload, so the file is
            // swapped with the core stopped; its name stays for stable accounting.
            pool.while_stopped(profile, async move {
                let content = serde_json::to_vec(&payload)
                    .map_err(|error| AppError::new(error.to_string(), 500))?;
                tokio::task::spawn_blocking(move || write_atomic(&path, &content, 0o600))
                    .await
                    .map_err(|error| AppError::new(error.to_string(), 500))?
                    .map_err(|error| AppError::new(error.to_string(), 500))?;
                accounts.replaced(&id, &account, &payload).await
            })
            .await?;
        } else {
            pool.start(profile, false).await?;
            pool.management(
                profile,
                &Method::POST,
                &format!("/auth-files?name={}", encode_uri_component(name.as_str())),
                Some(&Value::Object(payload)),
                MANAGEMENT_TIMEOUT,
            )
            .await?;
            self.0.accounts.sync(pool, profile).await?;
        }
        self.0.scheduler.changed().await?;
        let mut account = self
            .0
            .accounts
            .list(profile)
            .await?
            .into_iter()
            .find(|account| account.id == name)
            .ok_or_else(|| {
                AppError::new(
                    "The proxy has not registered this subscription yet. Refresh the profile.",
                    503,
                )
            })?;
        if !account.disabled {
            account.quota = Some(self.0.quotas.refresh(profile, &account.id).await?);
        }
        Ok(account)
    }

    /// 409 when the profile already holds this credential: the same refresh or access token,
    /// or, for a provider with organizations, the same seat. `prior` is the one it replaces.
    async fn refuse_duplicate(
        &self,
        profile: &ProfileId,
        provider: &str,
        organizations: bool,
        raw: &Map<String, Value>,
        files: &[CredentialFile],
        prior: Option<&CredentialFile>,
    ) -> Result<(), AppError> {
        let identity = organizations.then(|| claude_identity(&Value::Object(raw.clone())));
        let (access, refresh) = (
            token_of(raw, "access_token"),
            token_of(raw, "refresh_token"),
        );
        let same = |mine: Option<&str>, theirs: Option<&str>| {
            mine.zip(theirs)
                .is_some_and(|(mine, theirs)| same_secret(mine, theirs))
        };
        let duplicate = files
            .iter()
            .filter(|file| prior.is_none_or(|prior| prior.id != file.id))
            .any(|file| {
                text(file.raw.get("type")) == Some(provider)
                    && (same(refresh, token_of(&file.raw, "refresh_token"))
                        || same(access, token_of(&file.raw, "access_token"))
                        || identity.as_ref().is_some_and(|identity| {
                            same_seat(identity, &claude_identity(&Value::Object(file.raw.clone())))
                        }))
            });
        if !duplicate {
            return Ok(());
        }
        let id = profile.clone();
        let name = self.0.store.call(move |db| db.profile(&id)).await?.name;
        Err(AppError::new(
            format!(
                "This subscription is already connected to {name}. Use its existing connection instead of importing another copy."
            ),
            409,
        ))
    }

    /// `POST …/api-account`: adds an API-key account. Its models are found before the queue
    /// and its quota is read after it: neither changes credentials, and both wait on the
    /// provider.
    ///
    /// # Errors
    /// The input's validation or model discovery error, 409 for the same key at the same base
    /// URL in this profile, the start's or configuration's error, or the quota check's.
    pub async fn add_api_account(
        &self,
        profile: &ProfileId,
        input: ApiAccountInput,
    ) -> Result<(), AppError> {
        let account = api_account(&input, self.0.quotas.fetch()).await?;
        let key = input.api_key;
        let (this, id, saved) = (self.clone(), profile.clone(), account.clone());
        self.queue(async move {
            let (profile, store_key) = (id.clone(), key.clone());
            this.0
                .store
                .call(move |db| {
                    for existing in db.api_accounts(&profile)? {
                        if existing.base_url == saved.base_url
                            && db.api_key(&existing.id)?.matches(store_key.expose_secret())
                        {
                            return Err(AppError::new(
                                "This API key is already connected to this profile.",
                                409,
                            )
                            .into());
                        }
                    }
                    db.save_api_account(&profile, &saved, Some(&store_key))
                })
                .await?;
            this.0.scheduler.changed().await?;
            this.pool().start(&id, false).await?;
            this.pool().configure(&id).await
        })
        .await?;
        self.0.quotas.refresh(profile, &account.id).await?;
        Ok(())
    }

    /// `PATCH …/accounts/:accountId`: pauses or resumes an account, or changes its priority or
    /// name. An API-key account changes in the store; an OAuth one through its running core.
    ///
    /// # Errors
    /// 404 for an account the profile does not list, 409 when its core is not usable, or the
    /// core's or store's error.
    pub async fn set_account(
        &self,
        profile: &ProfileId,
        account: &AccountId,
        patch: AccountPatch,
    ) -> Result<(), AppError> {
        let (this, profile, account) = (self.clone(), profile.clone(), account.clone());
        self.queue(async move { this.edit(&profile, &account, patch).await })
            .await
    }

    async fn edit(
        &self,
        profile: &ProfileId,
        account: &AccountId,
        patch: AccountPatch,
    ) -> Result<(), AppError> {
        let (id, account_id, change) = (profile.clone(), account.clone(), patch.clone());
        let api = self
            .0
            .store
            .call(move |db| {
                let Some(mut api) = db
                    .api_accounts(&id)?
                    .into_iter()
                    .find(|api| api.id == account_id)
                else {
                    return Ok(false);
                };
                api.disabled = change.disabled.unwrap_or(api.disabled);
                if let Some(name) = change.name.filter(|name| !name.is_empty()) {
                    api.name = name;
                }
                db.save_api_account(&id, &api, None)?;
                Ok(true)
            })
            .await?;
        let pool = self.pool();
        if api {
            return pool.configure(profile).await;
        }
        self.listed(profile, account).await?;
        // A stopped or exited core keeps its record and port; nothing answers there.
        if !pool.usable(profile) {
            return Err(AppError::new("Start this profile first.", 409));
        }
        if let Some(disabled) = patch.disabled {
            let body = json!({ "name": account.as_str(), "disabled": disabled });
            pool.management(
                profile,
                &Method::PATCH,
                "/auth-files/status",
                Some(&body),
                MANAGEMENT_TIMEOUT,
            )
            .await?;
        }
        if patch.priority.is_some() || patch.name.is_some() {
            let mut body = Map::new();
            body.insert("name".to_owned(), account.as_str().into());
            if let Some(priority) = patch.priority {
                body.insert("priority".to_owned(), priority.into());
            }
            if let Some(name) = patch.name {
                body.insert("note".to_owned(), name.into());
            }
            pool.management(
                profile,
                &Method::PATCH,
                "/auth-files/fields",
                Some(&Value::Object(body)),
                MANAGEMENT_TIMEOUT,
            )
            .await?;
        }
        self.0.accounts.sync(pool, profile).await?;
        Ok(())
    }

    /// `DELETE …/accounts/:accountId`: removes an account; an OAuth one's core starts first.
    ///
    /// # Errors
    /// 404 for an account the profile does not list, or the core's or store's error.
    pub async fn remove_account(
        &self,
        profile: &ProfileId,
        account: &AccountId,
    ) -> Result<(), AppError> {
        let (this, profile, account) = (self.clone(), profile.clone(), account.clone());
        self.queue(async move { this.remove(&profile, &account).await })
            .await
    }

    async fn remove(&self, profile: &ProfileId, account: &AccountId) -> Result<(), AppError> {
        let listed = self.listed(profile, account).await?;
        let pool = self.pool();
        if listed.kind == AccountKind::ApiKey {
            let (id, account) = (profile.clone(), account.clone());
            self.0
                .store
                .call(move |db| db.remove_api_account(&id, &account))
                .await?;
            pool.configure(profile).await?;
        } else {
            pool.start(profile, false).await?;
            pool.management(
                profile,
                &Method::DELETE,
                &format!(
                    "/auth-files?name={}",
                    encode_uri_component(account.as_str())
                ),
                None,
                MANAGEMENT_TIMEOUT,
            )
            .await?;
            self.0.accounts.sync(pool, profile).await?;
        }
        self.0.scheduler.changed().await
    }

    async fn listed(&self, profile: &ProfileId, account: &AccountId) -> Result<Account, AppError> {
        self.0
            .accounts
            .list(profile)
            .await?
            .into_iter()
            .find(|listed| listed.id == *account)
            .ok_or_else(|| AppError::new(NOT_FOUND, 404))
    }
}
