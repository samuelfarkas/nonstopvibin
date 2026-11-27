//! Banked rate-limit resets (`src/server/reset-credits.ts`): listing a Codex subscription's
//! reset credits and redeeming one. A redemption keeps its request id in the store until the outcome is known, so a retry after
//! a failure, an ambiguous answer or a restart redeems the same reset once.

use std::borrow::Cow;
use std::sync::Arc;

use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::js;
use nsv_domain::registry::{Provider, ResetResultCode, registry};
use nsv_domain::time;
use nsv_domain::wire::{AccountKind, QuotaStatus, ResetCredit, ResetCredits, ResetResult};
use nsv_store::Store;
use serde_json::{Map, Value, json};

use crate::quota::{
    CODEX_API, QuotaError, QuotaSource, Quotas, RefreshQuota, UpstreamRequest, call_upstream,
    status_text,
};
use crate::source::PoolSource;
use crate::task::Exclusive;

/// A whole, non-negative count that fits a `u32`.
fn whole(value: &Value) -> Option<u32> {
    let count = value.as_f64()?;
    if count.fract() != 0.0 || count < 0.0 {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked whole and non-negative; u32 bounds it"
    )]
    u32::try_from(count as u64).ok()
}

/// zod's `resetCreditsSchema` (`OpenAI`'s `backend-client/src/types.rs`).
#[must_use]
pub fn reset_credits(value: &Value) -> Option<ResetCredits> {
    let object = value.as_object()?;
    let available_count = whole(object.get("available_count")?)?;
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
    // The wire keeps an absent field apart from a `null` one.
    let wire = |field| match field {
        Nullable::Absent => None,
        Nullable::Null => Some(None),
        Nullable::Text(text) => Some(Some(text)),
    };
    Some(ResetCredit {
        id: id.to_owned(),
        reset_type: object.get("reset_type")?.as_str()?.to_owned(),
        status: object.get("status")?.as_str()?.to_owned(),
        expires_at,
        title: wire(nullable_text(object, "title")?),
        description: wire(nullable_text(object, "description")?),
    })
}

/// An optional, nullable string field.
enum Nullable {
    Absent,
    Null,
    Text(String),
}

/// The field `key` as [`Nullable`]; `None` for anything else, which drops the credit.
fn nullable_text(object: &Map<String, Value>, key: &str) -> Option<Nullable> {
    match object.get(key) {
        None => Some(Nullable::Absent),
        Some(Value::Null) => Some(Nullable::Null),
        Some(Value::String(text)) => Some(Nullable::Text(text.clone())),
        Some(_) => None,
    }
}

/// zod's `resetResultSchema`: a known result code.
#[must_use]
pub fn reset_result(value: &Value) -> Option<ResetResultCode> {
    ResetResultCode::from_id(js::text(js::field(Some(value), "code"))?)
}

fn unknown_outcome() -> AppError {
    AppError::new("Reset outcome is unknown. Retry the same reset.", 502)
}

/// A subscription that can redeem resets.
struct Subscription {
    auth_index: String,
    /// The `ChatGPT` account id: redemptions run one at a time for it and keep their request
    /// ids under it.
    chatgpt_account_id: String,
    /// The provider's name in messages.
    label: String,
}

impl Subscription {
    /// The request that lists the subscription's resets.
    fn listing(&self) -> UpstreamRequest {
        UpstreamRequest::codex(
            "GET",
            format!("{CODEX_API}/rate-limit-reset-credits"),
            Some(&self.chatgpt_account_id),
        )
    }

    /// The request that redeems `credit`; the upstream redeems it once for `request_id`.
    fn redemption(&self, credit: &str, request_id: &str) -> UpstreamRequest {
        let mut request = UpstreamRequest::codex(
            "POST",
            format!("{CODEX_API}/rate-limit-reset-credits/consume"),
            Some(&self.chatgpt_account_id),
        );
        request.data =
            Some(json!({ "credit_id": credit, "redeem_request_id": request_id }).to_string());
        request
    }
}

/// Redemptions, one at a time per subscription, through `source`'s cores; a redemption refreshes the
/// account's quota through `quotas`.
pub struct Resets<S = PoolSource, Q = Quotas<PoolSource>> {
    store: Store,
    source: S,
    quotas: Arc<Q>,
    redeeming: Exclusive<String>,
}

impl<S: QuotaSource, Q: RefreshQuota> Resets<S, Q> {
    #[must_use]
    pub fn new(store: Store, source: S, quotas: Arc<Q>) -> Self {
        Self {
            store,
            source,
            quotas,
            redeeming: Exclusive::new(),
        }
    }

    async fn subscription(
        &self,
        profile: &ProfileId,
        account_id: &AccountId,
    ) -> Result<Subscription, QuotaError> {
        let source = &self.source;
        if !source.usable(profile) {
            return Err(AppError::not_started().into());
        }
        let account = source
            .accounts(profile)
            .await?
            .into_iter()
            .find(|account| account.kind == AccountKind::OAuth && &account.id == account_id)
            .ok_or_else(AppError::account_not_found)?;
        Provider::from_id(&account.provider)
            .filter(|provider| registry().providers.get(*provider).banked_resets)
            .ok_or_else(|| AppError::new("Banked resets require a Codex subscription.", 400))?;
        let label = registry().label(&account.provider);
        let chatgpt_account_id = source
            .chatgpt_account_id(profile, account_id)
            .filter(|id| !id.is_empty());
        match (
            chatgpt_account_id,
            account.auth_index.filter(|index| !index.is_empty()),
        ) {
            (Some(chatgpt_account_id), Some(auth_index)) => Ok(Subscription {
                auth_index,
                chatgpt_account_id,
                label,
            }),
            _ => Err(AppError::new(
                "Reconnect this subscription to verify its account identity.",
                409,
            )
            .into()),
        }
    }

    /// Sends `request` as the subscription and answers the unwrapped body of its 200.
    async fn send(
        &self,
        profile: &ProfileId,
        subscription: &Subscription,
        request: &UpstreamRequest,
    ) -> Result<Value, QuotaError> {
        let label = &subscription.label;
        let answer = call_upstream(
            &self.source,
            profile,
            Some(&subscription.auth_index),
            request,
        )
        .await?;
        if answer.status != Some(200.0) {
            return Err(AppError::new(
                format!(
                    "{label} returned HTTP {}. Retry the same reset; reconnect if authorization has expired.",
                    status_text(answer.status)
                ),
                502,
            )
            .into());
        }
        js::unwrap(&answer.body).map(Cow::into_owned).map_err(|_| {
            AppError::new(
                format!("{label} returned an unreadable reset response. Retry the same reset."),
                502,
            )
            .into()
        })
    }

    /// The subscription's banked resets.
    ///
    /// # Errors
    /// 409 when the profile is stopped or the subscription's identity is unknown, 404 for an
    /// unknown account, 400 for one that is not a Codex subscription, 502 for an
    /// upstream failure or an answer that does not match the schema.
    pub async fn credits(
        &self,
        profile: &ProfileId,
        account: &AccountId,
    ) -> Result<ResetCredits, QuotaError> {
        let subscription = self.subscription(profile, account).await?;
        let body = self
            .send(profile, &subscription, &subscription.listing())
            .await?;
        reset_credits(&body).ok_or_else(|| {
            AppError::new(
                format!("{} reset information is unavailable.", subscription.label),
                502,
            )
            .into()
        })
    }

    /// Redeems `credit_id`, then refreshes the account's quota. A request id is cleared only
    /// when the upstream says nothing was redeemed (`nothing_to_reset`, `no_credit`): a credit
    /// is used once. After an ambiguous answer a retry reuses it.
    ///
    /// # Errors
    /// As [`Resets::credits`], 409 while another redemption for the subscription runs, and 502
    /// for an unknown outcome.
    pub async fn consume(
        &self,
        profile: &ProfileId,
        account: &AccountId,
        credit_id: &str,
    ) -> Result<ResetResult, QuotaError> {
        let subscription = self.subscription(profile, account).await?;
        let key = subscription.chatgpt_account_id.clone();
        let Some(_redeeming) = self.redeeming.begin(key.clone()) else {
            return Err(AppError::new(
                "A reset is already in progress for this subscription.",
                409,
            )
            .into());
        };
        let request_id = {
            let (key, credit) = (key.clone(), credit_id.to_owned());
            self.store
                .call(move |db| db.reset_request_id(&key, &credit))
                .await?
        };
        let request = subscription.redemption(credit_id, &request_id);
        let body = self.send(profile, &subscription, &request).await?;
        let code = reset_result(&body).ok_or_else(unknown_outcome)?;
        if matches!(
            code,
            ResetResultCode::NothingToReset | ResetResultCode::NoCredit
        ) {
            let credit = credit_id.to_owned();
            self.store
                .call(move |db| db.clear_reset_request(&key, &credit))
                .await?;
        }
        // The redemption is settled even if a concurrent refresh or an account removal wins.
        let quota_refreshed = self
            .quotas
            .refresh(profile, account)
            .await
            .is_ok_and(|quota| quota.status == QuotaStatus::Available);
        Ok(ResetResult {
            code,
            quota_refreshed,
        })
    }
}
