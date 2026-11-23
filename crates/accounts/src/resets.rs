//! Codex's banked rate-limit resets (`src/server/reset-credits.ts`): listing them and redeeming
//! one. A redemption keeps its request id in the store until the outcome is known, so a retry
//! after a failure, an ambiguous answer or a restart redeems the same credit once.

use std::collections::HashSet;
use std::sync::Mutex;

use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::js;
use nsv_domain::registry::{Provider, ResetResultCode, registry};
use nsv_domain::sync::locked;
use nsv_domain::time;
use nsv_domain::wire::{AccountKind, QuotaStatus, ResetCredit, ResetCredits, ResetResult};
use nsv_store::Store;
use serde_json::{Map, Value, json};

use crate::quota::{
    CODEX_API, QuotaError, QuotaSource, RefreshQuota, UpstreamRequest, call_upstream, status_text,
};

/// zod's `resetCreditsSchema` (`OpenAI`'s `backend-client/src/types.rs`).
#[must_use]
pub fn reset_credits(value: &Value) -> Option<ResetCredits> {
    let object = value.as_object()?;
    let count = object.get("available_count")?.as_f64()?;
    if count.fract() != 0.0 || count < 0.0 {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked whole and non-negative; u32 bounds it"
    )]
    let available_count = u32::try_from(count as u64).ok()?;
    let credits = object
        .get("credits")?
        .as_array()?
        .iter()
        .map(reset_credit)
        .collect::<Option<Vec<_>>>()?;
    Some(ResetCredits {
        available_count,
        credits,
    })
}

fn reset_credit(value: &Value) -> Option<ResetCredit> {
    let object = value.as_object()?;
    let id = object.get("id")?.as_str()?;
    if !(1..=500).contains(&js::utf16_len(id)) {
        return None;
    }
    let expires_at = match object.get("expires_at")? {
        Value::Null => None,
        Value::String(time) if time::is_offset_datetime(time) => Some(time.clone()),
        _ => return None,
    };
    Some(ResetCredit {
        id: id.to_owned(),
        reset_type: object.get("reset_type")?.as_str()?.to_owned(),
        status: object.get("status")?.as_str()?.to_owned(),
        expires_at,
        title: nullable_text(object, "title")?,
        description: nullable_text(object, "description")?,
    })
}

/// An optional, nullable string: absent, `null` or text; `None` for anything else.
#[expect(
    clippy::option_option,
    reason = "invalid, absent, null and text are four outcomes"
)]
fn nullable_text(object: &Map<String, Value>, key: &str) -> Option<Option<Option<String>>> {
    match object.get(key) {
        None => Some(None),
        Some(Value::Null) => Some(Some(None)),
        Some(Value::String(text)) => Some(Some(Some(text.clone()))),
        Some(_) => None,
    }
}

/// zod's `resetResultSchema`: a known result code.
#[must_use]
pub fn reset_result(value: &Value) -> Option<ResetResultCode> {
    ResetResultCode::from_id(js::text(js::field(Some(value), "code"))?)
}

/// A Codex subscription that can redeem resets.
struct Subscription {
    auth_index: String,
    identity: String,
}

/// Redemptions, one at a time per `ChatGPT` account.
pub struct CodexResets {
    store: Store,
    redeeming: Mutex<HashSet<String>>,
}

/// Ends a subscription's redemption when the call ends, finished or dropped.
struct Redeeming<'a> {
    set: &'a Mutex<HashSet<String>>,
    identity: String,
}

impl Drop for Redeeming<'_> {
    fn drop(&mut self) {
        locked(self.set).remove(&self.identity);
    }
}

impl CodexResets {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self {
            store,
            redeeming: Mutex::new(HashSet::new()),
        }
    }

    async fn subscription(
        source: &impl QuotaSource,
        profile: &ProfileId,
        account_id: &AccountId,
    ) -> Result<Subscription, QuotaError> {
        if !source.usable(profile) {
            return Err(AppError::new("Start this profile first.", 409).into());
        }
        let account = source
            .accounts(profile)
            .await?
            .into_iter()
            .find(|account| account.kind == AccountKind::OAuth && &account.id == account_id)
            .ok_or_else(|| AppError::new("Account not found in this profile.", 404))?;
        let banked = match Provider::from_id(&account.provider) {
            Some(provider) => registry().providers.get(provider).banked_resets,
            None => false,
        };
        if !banked {
            return Err(AppError::new("Banked resets require a Codex subscription.", 400).into());
        }
        let identity = source
            .chatgpt_account_id(profile, account_id)
            .filter(|id| !id.is_empty());
        match (
            identity,
            account.auth_index.filter(|index| !index.is_empty()),
        ) {
            (Some(identity), Some(auth_index)) => Ok(Subscription {
                auth_index,
                identity,
            }),
            _ => Err(AppError::new(
                "Reconnect this subscription to verify its account identity.",
                409,
            )
            .into()),
        }
    }

    /// Lists (no credit) or redeems a credit, and answers the unwrapped body.
    async fn call(
        &self,
        source: &impl QuotaSource,
        profile: &ProfileId,
        subscription: &Subscription,
        credit_id: Option<&str>,
    ) -> Result<Value, QuotaError> {
        let identity = subscription.identity.as_str();
        let request = match credit_id {
            None => UpstreamRequest::codex(
                "GET",
                format!("{CODEX_API}/rate-limit-reset-credits"),
                Some(identity),
            ),
            Some(credit) => {
                let (key, credit_key) = (identity.to_owned(), credit.to_owned());
                let request_id = self
                    .store
                    .call(move |db| db.reset_request_id(&key, &credit_key))
                    .await?;
                let mut request = UpstreamRequest::codex(
                    "POST",
                    format!("{CODEX_API}/rate-limit-reset-credits/consume"),
                    Some(identity),
                );
                request.data = Some(
                    json!({ "credit_id": credit, "redeem_request_id": request_id }).to_string(),
                );
                request
            }
        };
        let answer =
            call_upstream(source, profile, Some(&subscription.auth_index), &request).await?;
        if answer.status != Some(200.0) {
            return Err(AppError::new(
                format!(
                    "Codex returned HTTP {}. Retry the same reset; reconnect if authorization has expired.",
                    status_text(answer.status)
                ),
                502,
            )
            .into());
        }
        js::unwrap(&answer.body)
            .map(std::borrow::Cow::into_owned)
            .map_err(|_| {
                AppError::new(
                    "Codex returned an unreadable reset response. Retry the same reset.",
                    502,
                )
                .into()
            })
    }

    /// The subscription's banked resets.
    ///
    /// # Errors
    /// 409 when the profile is stopped or the subscription's identity is unknown, 404 for an
    /// unknown account, 400 for one that is not a Codex subscription, 502 for an upstream
    /// failure or an answer that does not match the schema.
    pub async fn credits(
        &self,
        source: &impl QuotaSource,
        profile: &ProfileId,
        account: &AccountId,
    ) -> Result<ResetCredits, QuotaError> {
        let subscription = Self::subscription(source, profile, account).await?;
        let body = self.call(source, profile, &subscription, None).await?;
        reset_credits(&body)
            .ok_or_else(|| AppError::new("Codex reset information is unavailable.", 502).into())
    }

    /// Redeems `credit_id`, then refreshes the account's quota. The request id is cleared only
    /// when the upstream says nothing was redeemed (`nothing_to_reset`, `no_credit`); after a
    /// success or an ambiguous answer a retry reuses it.
    ///
    /// # Errors
    /// As [`CodexResets::credits`], 409 while another redemption for the subscription runs,
    /// and 502 for an unknown outcome.
    pub async fn consume(
        &self,
        source: &impl QuotaSource,
        quotas: &impl RefreshQuota,
        profile: &ProfileId,
        account: &AccountId,
        credit_id: &str,
    ) -> Result<ResetResult, QuotaError> {
        let subscription = Self::subscription(source, profile, account).await?;
        let _redeeming = {
            let mut redeeming = locked(&self.redeeming);
            if !redeeming.insert(subscription.identity.clone()) {
                return Err(AppError::new(
                    "A reset is already in progress for this subscription.",
                    409,
                )
                .into());
            }
            Redeeming {
                set: &self.redeeming,
                identity: subscription.identity.clone(),
            }
        };
        let body = self
            .call(source, profile, &subscription, Some(credit_id))
            .await?;
        let code = reset_result(&body)
            .ok_or_else(|| AppError::new("Reset outcome is unknown. Retry the same reset.", 502))?;
        if matches!(
            code,
            ResetResultCode::NothingToReset | ResetResultCode::NoCredit
        ) {
            let (identity, credit) = (subscription.identity.clone(), credit_id.to_owned());
            self.store
                .call(move |db| db.clear_reset_request(&identity, &credit))
                .await?;
        }
        // The redemption is settled even if a concurrent refresh or an account removal wins.
        let quota_refreshed = quotas
            .refresh(profile, account)
            .await
            .is_ok_and(|quota| quota.status == QuotaStatus::Available);
        Ok(ResetResult {
            code,
            quota_refreshed,
        })
    }
}
