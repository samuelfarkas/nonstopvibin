//! Credential changes (`src/server/credentials.ts`): imported OAuth credentials, API-key
//! accounts, and account edits and removals. They run one at a time across all profiles, each
//! to its end even if its caller stops waiting.
//!
//! - Duplicates are found within one profile only: the same refresh or access token, or the
//!   same API key at the same base URL. Several profiles may share one.
//! - A new credential goes through the core's management upload, then the account list is read
//!   again and the account's quota checked.

use std::future::Future;
use std::sync::Arc;

use nsv_core::pool::Pool;
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::js::{encode_uri_component, text};
use nsv_domain::registry::{Protocol, Provider, registry};
use nsv_domain::secret::{ApiKey, same_secret};
use nsv_domain::wire::{
    Account, AccountKind, AccountPatch, ApiAccount, ApiAccountInput, ApiModel, Extra,
};
use nsv_net::Method;
use nsv_store::Store;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::accounts::{Accounts, CredentialFile};
use crate::api_accounts::api_account;
use crate::fetch::Outside;
use crate::quota::Quotas;
use crate::source::PoolSource;
use crate::task::detached;

/// A non-empty token field of a credential.
fn token_of<'a>(credential: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    text(credential.get(key)).filter(|token| !token.is_empty())
}

/// The app's credential changes. Clones share one queue.
pub(crate) struct Credentials(Arc<Inner>);

impl Clone for Credentials {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

struct Inner {
    store: Store,
    pool: Pool,
    accounts: Arc<Accounts>,
    quotas: Arc<Quotas<PoolSource>>,
    outside: Arc<dyn Outside>,
    queue: Arc<Mutex<()>>,
}

impl Credentials {
    #[must_use]
    pub(crate) fn new(
        store: Store,
        pool: Pool,
        accounts: Arc<Accounts>,
        quotas: Arc<Quotas<PoolSource>>,
        outside: Arc<dyn Outside>,
    ) -> Self {
        Self(Arc::new(Inner {
            store,
            pool,
            accounts,
            quotas,
            outside,
            queue: Arc::new(Mutex::new(())),
        }))
    }

    /// Runs `change` after every change queued before it.
    ///
    /// # Errors
    /// What `change` answers.
    pub(crate) async fn queue<T: Send + 'static>(
        &self,
        change: impl Future<Output = Result<T, AppError>> + Send + 'static,
    ) -> Result<T, AppError> {
        let turn = Arc::clone(&self.0.queue).lock_owned().await;
        detached(turn, change).await
    }

    /// Waits for the changes queued so far.
    pub(crate) async fn settled(&self) {
        drop(self.0.queue.lock().await);
    }

    /// `POST …/import` and `…/import-source`: adds a `CLIProxyAPI` OAuth credential file.
    ///
    /// # Errors
    /// See [`Credentials::import_credential`].
    pub(crate) async fn import(
        &self,
        profile: &ProfileId,
        raw: Map<String, Value>,
    ) -> Result<Account, AppError> {
        let (this, profile) = (self.clone(), profile.clone());
        self.queue(async move { this.import_credential(&profile, raw).await })
            .await
    }

    /// Adds a credential. The caller holds the queue.
    ///
    /// # Errors
    /// 400 for a file without a supported type and token, 409 for a duplicate within the
    /// profile, 503 when the core does not list the new
    /// account, or the core's, store's or quota check's error.
    async fn import_credential(
        &self,
        profile: &ProfileId,
        raw: Map<String, Value>,
    ) -> Result<Account, AppError> {
        let provider = text(raw.get("type")).unwrap_or_default().to_owned();
        let entry = Provider::from_id(&provider).map(|known| registry().providers.get(known));
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
        self.refuse_duplicate(profile, &provider, &raw, &files)
            .await?;
        let uuid = nsv_domain::random::uuid().map_err(AppError::internal)?;
        let name =
            AccountId::parse(&format!("{provider}-{uuid}.json")).map_err(AppError::internal)?;
        let mut payload = raw;
        let disabled = payload.get("disabled") == Some(&json!(true));
        payload.insert("disabled".to_owned(), disabled.into());
        let pool = &self.0.pool;
        pool.start(profile, false).await?;
        pool.management(
            profile,
            &Method::POST,
            &format!("/auth-files?name={}", encode_uri_component(name.as_str())),
            Some(&Value::Object(payload)),
        )
        .await?;
        self.0.accounts.sync(pool, profile).await?;
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

    /// 409 when the profile already holds this credential: the same refresh or access token.
    async fn refuse_duplicate(
        &self,
        profile: &ProfileId,
        provider: &str,
        raw: &Map<String, Value>,
        files: &[CredentialFile],
    ) -> Result<(), AppError> {
        let (access, refresh) = (
            token_of(raw, "access_token"),
            token_of(raw, "refresh_token"),
        );
        let same = |mine: Option<&str>, theirs: Option<&str>| {
            mine.zip(theirs)
                .is_some_and(|(mine, theirs)| same_secret(mine, theirs))
        };
        let duplicate = files.iter().any(|file| {
            text(file.raw.get("type")) == Some(provider)
                && (same(refresh, token_of(&file.raw, "refresh_token"))
                    || same(access, token_of(&file.raw, "access_token")))
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
    pub(crate) async fn add_api_account(
        &self,
        profile: &ProfileId,
        input: ApiAccountInput,
    ) -> Result<(), AppError> {
        let account = api_account(&input, &*self.0.outside).await?;
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
            this.0.pool.start(&id, false).await?;
            this.0.pool.configure(&id).await
        })
        .await?;
        self.0.quotas.refresh(profile, &account.id).await?;
        Ok(())
    }

    /// `POST …/claude-agent`: adds the profile's Claude Agent subscription, served by the
    /// user's own Claude Code with `models`. Its core presents a new random key to the engine.
    ///
    /// # Errors
    /// 409 when the profile already has one, or the store's, start's or
    /// configuration's error.
    pub(crate) async fn add_claude_agent(
        &self,
        profile: &ProfileId,
        name: String,
        models: Vec<String>,
    ) -> Result<(), AppError> {
        let (this, id) = (self.clone(), profile.clone());
        self.queue(async move {
            let key = ApiKey::generate().map_err(AppError::internal)?;
            let account = ApiAccount {
                id: AccountId::parse(&nsv_domain::random::uuid().map_err(AppError::internal)?)
                    .map_err(AppError::internal)?,
                name,
                provider: Provider::ClaudeAgent,
                base_url: String::new(),
                prefix: String::new(),
                models: models
                    .into_iter()
                    .map(|model| ApiModel {
                        id: model,
                        protocol: Protocol::Anthropic,
                        extra: Extra::new(),
                    })
                    .collect(),
                disabled: false,
                extra: Extra::new(),
            };
            let profile = id.clone();
            this.0
                .store
                .call(move |db| {
                    let existing = db.api_accounts(&profile)?;
                    if existing
                        .iter()
                        .any(|known| known.provider == Provider::ClaudeAgent)
                    {
                        return Err(AppError::new(
                            "This profile already has a Claude Agent subscription.",
                            409,
                        )
                        .into());
                    }
                    db.save_api_account(&profile, &account, Some(&key))
                })
                .await?;
            this.0.pool.start(&id, false).await?;
            this.0.pool.configure(&id).await
        })
        .await
    }

    /// `PATCH …/accounts/:accountId`: pauses or resumes an account, or changes its priority or
    /// name. An API-key account changes in the store; an OAuth one through its running core.
    ///
    /// # Errors
    /// 404 for an account the profile does not list, 409 when its core is not usable, or the
    /// core's or store's error.
    pub(crate) async fn set_account(
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
        let pool = &self.0.pool;
        if api {
            return pool.configure(profile).await;
        }
        self.listed(profile, account).await?;
        // A stopped or exited core keeps its record and port; nothing answers there.
        if !pool.state().usable(profile) {
            return Err(AppError::not_started());
        }
        if let Some(disabled) = patch.disabled {
            let body = json!({ "name": account.as_str(), "disabled": disabled });
            pool.management(profile, &Method::PATCH, "/auth-files/status", Some(&body))
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
    pub(crate) async fn remove_account(
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
        let pool = &self.0.pool;
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
            )
            .await?;
            self.0.accounts.sync(pool, profile).await?;
        }
        Ok(())
    }

    async fn listed(&self, profile: &ProfileId, account: &AccountId) -> Result<Account, AppError> {
        self.0
            .accounts
            .list(profile)
            .await?
            .into_iter()
            .find(|listed| listed.id == *account)
            .ok_or_else(AppError::account_not_found)
    }
}
