//! The routes of `src/shared/api.ts`, their bodies and small responses. Request bodies reject
//! unknown fields like the TypeScript routes' `.strict()` schemas; the server checks their
//! values.

use serde::{Deserialize, Serialize};

use super::{Profile, True};
use crate::id::AccountId;
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

/// `GET` and `POST /profiles/:profileId/usage-hub`: where a usage hub client such as T3 Code
/// reads the profile's limits, and the hub key it sends.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct UsageHub {
    /// `http://127.0.0.1:<port>/p/<slug>`.
    pub url: String,
    pub key: String,
}

impl std::fmt::Debug for UsageHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UsageHub(<redacted>)")
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

impl ProfilePatch {
    /// Changes the fields it names.
    pub fn apply(self, profile: &mut Profile) {
        if let Some(name) = self.name {
            profile.name = name;
        }
        if let Some(strategy) = self.strategy {
            profile.strategy = strategy;
        }
        if let Some(affinity) = self.session_affinity {
            profile.session_affinity = affinity;
        }
    }
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CreateProfileBody {
    pub name: String,
}

/// `PUT /usage-retention`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RetentionBody {
    pub days: RetentionDays,
}

/// `POST /core/install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CoreInstallBody {
    pub version: String,
}

/// `POST /profiles/:profileId/oauth`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct OAuthStartBody {
    pub provider: OAuthProvider,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconnect_account_id: Option<AccountId>,
}

/// `POST /profiles/:profileId/oauth-confirm`, and the `state` query of the OAuth routes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StateBody {
    pub state: String,
}

/// `POST /profiles/:profileId/callback`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CallbackBody {
    pub url: String,
}

/// `POST /profiles/:profileId/import`: credential files by name.
#[derive(Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ImportBody {
    #[cfg_attr(test, ts(type = "Record<string, import(\"../../types.ts\").Json>"))]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ImportSourceBody {
    pub source_id: String,
}

/// `POST /profiles/:profileId/accounts/:accountId/reset-credits`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CreditBody {
    pub credit_id: String,
}

/// A route under `/api`, by the path parameters it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// No parameters: the path matches exactly.
    Plain(PlainRoute),
    /// `/profiles/:profileId…`.
    Profile(ProfileRoute),
    /// `/profiles/:profileId/accounts/:accountId…`.
    Account(AccountRoute),
}

/// A route whose path names no parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlainRoute {
    State,
    Retention,
    CoreCheck,
    CoreInstall,
    CoreBundled,
    ImportSources,
    CreateProfile,
    DesktopShow,
}

/// A route under one profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileRoute {
    Patch,
    Start,
    Refresh,
    Key,
    UsageHub,
    UsageHubReplace,
    Models,
    Usage,
    AgentStatus,
    AgentInstall,
    AgentRemove,
    AgentCheck,
    SignIn,
    SignInStatus,
    SignInCancel,
    SignInConfirm,
    SignInSession,
    Callback,
    Import,
    ImportSource,
    ApiAccount,
}

/// A route under one account of a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountRoute {
    Set,
    Remove,
    ResetCredits,
    ConsumeReset,
}

/// `server.ts`'s route table, in its order: a 405 lists methods in this order. Each entry is
/// `(method, path, route)`; `:name` path segments are parameters. `src/shared/api.ts` types
/// every key (`bun run wire:check`).
pub const ROUTES: &[(&str, &str, Route)] = &[
    ("GET", "/state", Route::Plain(PlainRoute::State)),
    (
        "PUT",
        "/usage-retention",
        Route::Plain(PlainRoute::Retention),
    ),
    ("POST", "/core/check", Route::Plain(PlainRoute::CoreCheck)),
    (
        "POST",
        "/core/install",
        Route::Plain(PlainRoute::CoreInstall),
    ),
    (
        "DELETE",
        "/core/install",
        Route::Plain(PlainRoute::CoreBundled),
    ),
    (
        "GET",
        "/import-sources",
        Route::Plain(PlainRoute::ImportSources),
    ),
    ("POST", "/profiles", Route::Plain(PlainRoute::CreateProfile)),
    (
        "PATCH",
        "/profiles/:profileId",
        Route::Profile(ProfileRoute::Patch),
    ),
    (
        "POST",
        "/profiles/:profileId/start",
        Route::Profile(ProfileRoute::Start),
    ),
    (
        "POST",
        "/profiles/:profileId/refresh",
        Route::Profile(ProfileRoute::Refresh),
    ),
    (
        "GET",
        "/profiles/:profileId/key",
        Route::Profile(ProfileRoute::Key),
    ),
    (
        "GET",
        "/profiles/:profileId/usage-hub",
        Route::Profile(ProfileRoute::UsageHub),
    ),
    (
        "POST",
        "/profiles/:profileId/usage-hub",
        Route::Profile(ProfileRoute::UsageHubReplace),
    ),
    (
        "GET",
        "/profiles/:profileId/models",
        Route::Profile(ProfileRoute::Models),
    ),
    (
        "GET",
        "/profiles/:profileId/usage",
        Route::Profile(ProfileRoute::Usage),
    ),
    (
        "GET",
        "/profiles/:profileId/agent-setup",
        Route::Profile(ProfileRoute::AgentStatus),
    ),
    (
        "POST",
        "/profiles/:profileId/agent-setup",
        Route::Profile(ProfileRoute::AgentInstall),
    ),
    (
        "DELETE",
        "/profiles/:profileId/agent-setup",
        Route::Profile(ProfileRoute::AgentRemove),
    ),
    (
        "POST",
        "/profiles/:profileId/agent-check",
        Route::Profile(ProfileRoute::AgentCheck),
    ),
    (
        "POST",
        "/profiles/:profileId/oauth",
        Route::Profile(ProfileRoute::SignIn),
    ),
    (
        "GET",
        "/profiles/:profileId/oauth",
        Route::Profile(ProfileRoute::SignInStatus),
    ),
    (
        "DELETE",
        "/profiles/:profileId/oauth",
        Route::Profile(ProfileRoute::SignInCancel),
    ),
    (
        "POST",
        "/profiles/:profileId/oauth-confirm",
        Route::Profile(ProfileRoute::SignInConfirm),
    ),
    (
        "GET",
        "/profiles/:profileId/oauth-session",
        Route::Profile(ProfileRoute::SignInSession),
    ),
    (
        "POST",
        "/profiles/:profileId/callback",
        Route::Profile(ProfileRoute::Callback),
    ),
    (
        "POST",
        "/profiles/:profileId/import",
        Route::Profile(ProfileRoute::Import),
    ),
    (
        "POST",
        "/profiles/:profileId/import-source",
        Route::Profile(ProfileRoute::ImportSource),
    ),
    (
        "PATCH",
        "/profiles/:profileId/accounts/:accountId",
        Route::Account(AccountRoute::Set),
    ),
    (
        "DELETE",
        "/profiles/:profileId/accounts/:accountId",
        Route::Account(AccountRoute::Remove),
    ),
    (
        "GET",
        "/profiles/:profileId/accounts/:accountId/reset-credits",
        Route::Account(AccountRoute::ResetCredits),
    ),
    (
        "POST",
        "/profiles/:profileId/accounts/:accountId/reset-credits",
        Route::Account(AccountRoute::ConsumeReset),
    ),
    (
        "POST",
        "/profiles/:profileId/api-account",
        Route::Profile(ProfileRoute::ApiAccount),
    ),
    // A second desktop launch; only the desktop app serves it.
    (
        "POST",
        "/desktop/show",
        Route::Plain(PlainRoute::DesktopShow),
    ),
];
