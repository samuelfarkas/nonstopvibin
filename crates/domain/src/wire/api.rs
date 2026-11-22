//! Route bodies and the small responses of `src/shared/api.ts`. Request bodies reject unknown
//! fields like the TypeScript routes' `.strict()` schemas; the server checks their values.

use serde::{Deserialize, Serialize};

use super::True;
use crate::registry::{OAuthProvider, Protocol, Provider, RetentionDays, Strategy};
use crate::secret::ApiKey;

/// `{ ok: true }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Ok {
    pub ok: True,
}

/// `GET /profiles/:profileId/key`. TypeScript names it `ClientKey`; here that name is the
/// secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ClientKeyResponse {
    pub key: String,
}

impl std::fmt::Debug for ClientKeyResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientKeyResponse(<redacted>)")
    }
}

/// The body of every error the server answers, under `/api` and elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ApiError {
    pub error: ApiErrorBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ApiErrorBody {
    pub message: String,
    #[serde(rename = "type")]
    pub kind: ApiErrorType,
    /// The HTTP status.
    pub code: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub enum ApiErrorType {
    #[serde(rename = "nonstopvibin_error")]
    NonstopvibinError,
}

/// `PATCH /profiles/:profileId`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct ProfilePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<Strategy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_affinity: Option<bool>,
}

/// `PATCH /profiles/:profileId/accounts/:accountId`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct AccountPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// From `-priorityLimit` to `priorityLimit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `POST /profiles/:profileId/api-account`. It carries a key, so it only deserializes.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ApiAccountInput {
    pub name: String,
    pub provider: Provider,
    pub base_url: String,
    #[cfg_attr(test, ts(type = "string"))]
    pub api_key: ApiKey,
    /// The route defaulted a missing protocol to `openai`.
    #[serde(default = "openai")]
    pub protocol: Protocol,
    pub prefix: String,
    /// Empty asks the server to discover the provider's models.
    pub models: Vec<ApiModelInput>,
}

const fn openai() -> Protocol {
    Protocol::Openai
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ApiModelInput {
    pub id: String,
    pub protocol: Protocol,
}

/// `POST /profiles`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CreateProfileBody {
    pub name: String,
}

/// `PUT /usage-retention`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RetentionBody {
    pub days: RetentionDays,
}

/// `POST /core/install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CoreInstallBody {
    pub version: String,
}

/// `POST /profiles/:profileId/agent-check`: `{}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS), ts(type = "Record<string, never>"))]
pub struct EmptyBody {}

/// `POST /profiles/:profileId/oauth`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct OAuthStartBody {
    pub provider: OAuthProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect_account_id: Option<String>,
}

/// `POST /profiles/:profileId/oauth-confirm`, and the `state` query of the OAuth routes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StateBody {
    pub state: String,
}

/// `POST /profiles/:profileId/callback`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CallbackBody {
    pub url: String,
}

/// `POST /profiles/:profileId/import`: credential files by name.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ImportBody {
    #[cfg_attr(
        test,
        ts(type = "Record<string, import(\"../../../src/shared/types.ts\").Json>")
    )]
    pub contents: serde_json::Map<String, serde_json::Value>,
}

impl std::fmt::Debug for ImportBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportBody")
            .field("files", &self.contents.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// `POST /profiles/:profileId/import-source`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ImportSourceBody {
    pub source_id: String,
}

/// `POST /profiles/:profileId/accounts/:accountId/reset-credits`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CreditBody {
    pub credit_id: String,
}
