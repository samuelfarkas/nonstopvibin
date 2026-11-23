use serde::{Deserialize, Serialize};

use super::Account;
use crate::id::ProfileId;
use crate::registry::OAuthProvider;

/// A browser sign-in in progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct OAuthSession {
    pub profile_id: ProfileId,
    pub provider: OAuthProvider,
    pub url: String,
    pub state: String,
    /// Milliseconds since the Unix epoch.
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect_account_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub enum OAuthAction {
    Add,
    Reconnect,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct OAuthReview {
    pub account: Account,
    pub action: OAuthAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub enum OAuthStatus {
    Wait,
    Review {
        review: OAuthReview,
    },
    Ok {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(test, ts(optional))]
        account: Option<Account>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(test, ts(optional))]
        reconnected: Option<bool>,
    },
    Error {
        error: String,
    },
}
