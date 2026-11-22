//! The JSON the management API answers and accepts, and the bodies the store keeps in SQLite.
//! Each type mirrors its twin in `src/shared/types.ts` or `src/shared/api.ts`: field names,
//! enum strings and optionality are the TypeScript ones, and a type test proves the two stay
//! mutually assignable (`tests/types/wire.ts`).
//!
//! # Shape rules
//!
//! - An optional field (`field?: T`) is `Option<T>`, omitted when `None`. A field TypeScript
//!   types `T | null` is `Option<T>` and always written. A field that may be absent *or*
//!   `null` (`pricingTokens?: PricingTokens | null`) is `Option<Option<T>>`, read with
//!   [`present`] so the two round-trip apart.
//! - Counts, status codes, milliseconds and priorities are integers. Percentages, prices and
//!   exchange rates are [`JsNumber`]: an `f64` that writes a whole value as `75`, not `75.0`,
//!   as `JSON.stringify` did.
//! - Timestamps stay `String`: the store compares `toISOString()` text, and usage rows keep
//!   the core's timestamp verbatim (nanoseconds included).
//!
//! # Unknown fields
//!
//! - **Bodies the server reads, changes and writes back keep them** ([`Profile`], [`Account`],
//!   [`ApiAccount`], [`ApiModel`]): an `extra` map collects fields this version does not
//!   know and writes them out again. The TypeScript server did the same through object
//!   spreads (`{ ...profile, ...patch }`), so a body written by a newer or older version keeps
//!   its data across a read-modify-write here.
//! - **Bodies written once from fresh data ignore them** ([`Quota`], [`UsageRecord`] and what
//!   they contain): a quota is replaced whole on every refresh and a usage row is never
//!   rewritten, so nothing is lost, and usage pages avoid the cost of collecting extras.
//! - **Request bodies reject them,** as the TypeScript routes' `.strict()` schemas did.

mod account;
mod agent;
mod api;
mod model;
mod oauth;
mod profile;
mod usage;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use account::{
    Account, AccountKind, AccountStatus, ApiAccount, ApiModel, ImportSource, Quota, QuotaStatus,
    QuotaWindow, ResetCredit, ResetCredits, ResetResult,
};
pub use agent::{AgentSelection, AgentSetup, AgentSetupInput};
pub use api::{
    AccountPatch, ApiAccountInput, ApiError, ApiErrorBody, ApiErrorType, ApiModelInput,
    CallbackBody, ClientKeyResponse, CoreInstallBody, CreateProfileBody, CreditBody, EmptyBody,
    ImportBody, ImportSourceBody, OAuthStartBody, Ok, ProfilePatch, RetentionBody, StateBody,
};
pub use model::{
    CatalogModel, ContextTier, Cost, CostTier, Modality, Model, ModelMetadata, TierType,
};
pub use oauth::{OAuthAction, OAuthReview, OAuthSession, OAuthStatus};
pub use profile::{AppState, CorePhase, CoreState, Profile, ProfileState, Runtime};
pub use usage::{
    Eur, PricingTokens, UsageHistory, UsagePricing, UsageRecord, UsageSummary, UsageTotals,
};

/// Fields a stored body had that this version does not model, written back unchanged.
pub type Extra = serde_json::Map<String, serde_json::Value>;

/// A string's length as JavaScript's `.length` counts it: UTF-16 code units. The name and
/// prefix limits were checked this way, so `"★"` counts 1 and `"😀"` counts 2.
#[must_use]
pub fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// Reads a field that is present, as `Some(value)`, where `value` may be `None` for `null`.
/// With `#[serde(default)]`, an absent field stays `None`, so `Option<Option<T>>` keeps
/// absent and `null` apart.
///
/// # Errors
/// When the present value is neither `null` nor a `T`.
#[expect(
    clippy::option_option,
    reason = "absent and null are different stored states"
)]
pub fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A fractional JSON number, written the way JavaScript writes it: a whole value in the
/// safe-integer range has no fraction (`75`, not `75.0`), so stored and answered JSON stays
/// what TypeScript wrote and compares equal as a `serde_json::Value`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default, Deserialize)]
#[serde(transparent)]
#[cfg_attr(test, derive(ts_rs::TS), ts(type = "number"))]
pub struct JsNumber(pub f64);

impl Serialize for JsNumber {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // 2^53: past it, f64 no longer holds every integer, and JavaScript writes exponents.
        const SAFE: f64 = 9_007_199_254_740_992.0;
        let value = self.0;
        if value.fract() == 0.0 && value.abs() < SAFE {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a whole value below 2^53 converts exactly"
            )]
            let whole = value as i64;
            serializer.serialize_i64(whole)
        } else {
            serializer.serialize_f64(value)
        }
    }
}

impl From<f64> for JsNumber {
    fn from(value: f64) -> Self {
        Self(value)
    }
}

/// The literal `true`, for fields TypeScript types as `true` (`Ok.ok`, `QuotaWindow.scoped`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(test, derive(ts_rs::TS), ts(type = "true"))]
pub struct True;

impl Serialize for True {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(true)
    }
}

impl<'de> Deserialize<'de> for True {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if bool::deserialize(deserializer)? {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("expected `true`"))
        }
    }
}

#[cfg(test)]
mod tests;
