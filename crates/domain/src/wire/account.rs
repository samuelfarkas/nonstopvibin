use serde::{Deserialize, Serialize};

use super::{Extra, JsNumber, present};
use crate::id::AccountId;
use crate::registry::{Protocol, Provider, ResetResultCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub enum AccountKind {
    #[serde(rename = "oauth")]
    OAuth,
    ApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub enum AccountStatus {
    Ready,
    #[serde(rename = "Needs attention")]
    NeedsAttention,
    #[serde(rename = "Reconnect needed")]
    ReconnectNeeded,
    #[serde(rename = "Cooling down")]
    CoolingDown,
    Paused,
    Configured,
}

/// An account in a profile's list. OAuth accounts are stored like this, without `quota`,
/// `prefix` and `modelCount`, as a JSON array in `settings.accounts:<profileId>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct Account {
    /// A credential file name, or an API-key account's id.
    pub id: AccountId,
    pub name: String,
    /// The core's provider string, which may be one this build does not know.
    pub provider: String,
    pub kind: AccountKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub disabled: bool,
    pub status: AccountStatus,
    pub priority: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_index: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<Quota>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_count: Option<u32>,
    #[serde(flatten)]
    #[cfg_attr(test, ts(skip))]
    pub extra: Extra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub enum QuotaStatus {
    Available,
    Unavailable,
    Error,
}

/// The `quotas.body` column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct Quota {
    pub status: QuotaStatus,
    pub windows: Vec<QuotaWindow>,
    pub checked_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banked_resets: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct QuotaWindow {
    pub label: String,
    /// Fractional: `39.5` is a real stored value.
    #[cfg_attr(test, ts(optional = false))]
    pub remaining_percent: Option<JsNumber>,
    #[cfg_attr(test, ts(optional = false))]
    pub resets_at: Option<String>,
}

/// An API-key account as the `api_accounts.body` column stores it (`src/server/store.ts`).
/// Its key lives in the vault, never here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ApiAccount {
    pub id: AccountId,
    pub name: String,
    pub provider: Provider,
    pub base_url: String,
    pub prefix: String,
    pub models: Vec<ApiModel>,
    pub disabled: bool,
    #[serde(flatten)]
    #[cfg_attr(test, ts(skip))]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ApiModel {
    pub id: String,
    pub protocol: Protocol,
    #[serde(flatten)]
    #[cfg_attr(test, ts(skip))]
    pub extra: Extra,
}

/// A credential file another app left that can be imported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct ImportSource {
    pub id: String,
    pub file_name: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub source: String,
}

/// A subscription's banked rate-limit resets, in Codex's snake case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ResetCredits {
    pub available_count: u32,
    pub credits: Vec<ResetCredit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct ResetCredit {
    pub id: String,
    pub reset_type: String,
    pub status: String,
    #[cfg_attr(test, ts(optional = false))]
    pub expires_at: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[expect(
        clippy::option_option,
        reason = "absent and null are different stored states"
    )]
    pub title: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[expect(
        clippy::option_option,
        reason = "absent and null are different stored states"
    )]
    pub description: Option<Option<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ResetResult {
    pub code: ResetResultCode,
    pub quota_refreshed: bool,
}
