use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{JsNumber, present};
use crate::id::ProfileId;

/// One request's accounting, as the `usage.body` column stores it: metadata only, never keys,
/// prompts or upstream error bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct UsageRecord {
    pub id: String,
    pub profile_id: ProfileId,
    /// The core's timestamp, verbatim.
    pub timestamp: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_model: Option<String>,
    /// Non-overlapping core accounting; `Some(None)` (`null`) means explicitly incomplete,
    /// `None` (absent) is a row from before the field existed.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[expect(
        clippy::option_option,
        reason = "absent and null are different stored states"
    )]
    pub pricing_tokens: Option<Option<PricingTokens>>,
    /// The core's auth index; empty when it named none.
    pub account: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    pub latency_ms: u64,
    pub failed: bool,
    pub status_code: u16,
    pub stream: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct PricingTokens {
    pub input: u64,
    pub cached: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct UsageTotals {
    pub requests: u64,
    pub priced_requests: u64,
    pub usd: JsNumber,
    pub normalized_requests: u64,
    pub tokens: PricingTokens,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct UsagePricing {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totals: Option<UsageTotals>,
    /// Price per record id; `null` when the model has no price.
    pub usd: BTreeMap<String, Option<JsNumber>>,
    #[cfg_attr(test, ts(optional = false))]
    pub fetched_at: Option<String>,
    pub refreshing: bool,
    #[cfg_attr(test, ts(optional = false))]
    pub eur: Option<Eur>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Eur {
    pub rate: JsNumber,
    pub date: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct UsageSummary {
    pub requests: u64,
    pub failed: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

/// `GET /profiles/:profileId/usage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct UsageHistory {
    pub records: Vec<UsageRecord>,
    pub summary: UsageSummary,
    pub pricing: UsagePricing,
}
