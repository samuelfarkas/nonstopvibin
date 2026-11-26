//! The quota and reset features' view of the core pool and the account list.

use std::sync::Arc;

use nsv_core::pool::Pool;
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::wire::Account;
use nsv_net::Method;
use serde_json::{Value, json};

use crate::accounts::Accounts;
use crate::quota::{QuotaSource, Quotas};

/// Quota and resets reach accounts through the pool's cores.
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
        self.pool.state().usable(profile)
    }

    fn quota_cooldown(&self, profile: &ProfileId, account: &AccountId) -> bool {
        self.accounts.quota_cooldown(profile, account.as_str())
    }

    async fn reset_cooldown(&self, profile: &ProfileId, auth_index: &str) -> Result<(), AppError> {
        self.pool
            .management(
                profile,
                &Method::POST,
                "/reset-quota",
                Some(&json!({ "auth_index": auth_index })),
            )
            .await
            .map(drop)
    }

    async fn api_call(&self, profile: &ProfileId, body: Value) -> Result<Value, AppError> {
        self.pool
            .management(profile, &Method::POST, "/api-call", Some(&body))
            .await
    }
}

impl Quotas<PoolSource> {
    /// Reads the profile's accounts from its core again, then refreshes the quota of each
    /// account `wanted` picks, in order, asking `wanted` as each one's turn comes
    /// (`Quotas.refreshAccounts`). Dropping the future abandons the check in flight: nothing is
    /// stored after.
    ///
    /// # Errors
    /// The sync's error, or the first check's that fails; see [`Quotas::refresh`].
    pub async fn refresh_accounts(
        &self,
        profile: &ProfileId,
        wanted: impl Fn(&Account) -> bool + Send,
    ) -> Result<(), AppError> {
        let source = self.source();
        source.accounts.sync(&source.pool, profile).await?;
        for account in source.accounts.list(profile).await? {
            if wanted(&account) {
                self.refresh(profile, &account.id).await?;
            }
        }
        Ok(())
    }
}
