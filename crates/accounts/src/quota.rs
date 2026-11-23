//! Subscription quota (`src/server/quota.ts`): each provider's usage endpoint, read into windows
//! of remaining headroom, and the refresh that stores one account's reading.
//!
//! - Account-wide windows decide headroom; a window that limits one model is `scoped`.
//! - A reading that is missing stays `null`; it never becomes 0% or 100%.
//! - No windows is `unavailable`. A failed check is `error` and keeps the last good windows and
//!   their time, so the UI can show how stale they are.
//! - The core's management API asks OAuth providers with the account's own token
//!   ([`QuotaSource::api_call`]); `OpenCode` Go is asked directly with the account's API key.

use std::borrow::Cow;
use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::registry::{Provider, RegistryError, registry};
use nsv_domain::time::iso_string;
use nsv_domain::wire::{Account, AccountKind, JsNumber, Quota, QuotaStatus, QuotaWindow, True};
use nsv_net::USER_AGENT;
use nsv_store::{Store, StoreError};
use serde_json::{Map, Value, json};

use crate::accounts::PoolSource;
use crate::fetch::{Fetch, Request};
use crate::js;

/// The beta flag Anthropic's OAuth account endpoints require.
pub const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";
/// `ChatGPT`'s backend for Codex subscriptions: quota and banked resets.
pub const CODEX_API: &str = "https://chatgpt.com/backend-api/wham";
const XAI_BILLING: &str = "https://cli-chat-proxy.grok.com/v1/billing";
const OPENCODE_TIMEOUT: Duration = Duration::from_secs(15);
const OPENCODE_LIMIT: u64 = 1024 * 1024;
const NOT_JSON: &str = "Quota response was not valid JSON.";

/// What the quota and reset features need from the core pool and the account list.
pub trait QuotaSource: Send + Sync {
    /// The profile's accounts, OAuth and API-key (`Accounts.list`).
    fn accounts(
        &self,
        profile: &ProfileId,
    ) -> impl Future<Output = Result<Vec<Account>, AppError>> + Send;

    /// The `ChatGPT` account id of a Codex credential (`Accounts.chatgptAccountId`).
    fn chatgpt_account_id(&self, profile: &ProfileId, account: &AccountId) -> Option<String>;

    /// Whether the profile's core can take management calls (`CorePool.usable`).
    fn usable(&self, profile: &ProfileId) -> bool;

    /// `POST /v0/management/api-call` on the profile's core with `body`; answers the core's
    /// JSON (`{ status_code, body }`).
    fn api_call(
        &self,
        profile: &ProfileId,
        body: Value,
    ) -> impl Future<Output = Result<Value, AppError>> + Send;
}

/// One pool shared by quotas and resets.
impl<T: QuotaSource> QuotaSource for Arc<T> {
    fn accounts(
        &self,
        profile: &ProfileId,
    ) -> impl Future<Output = Result<Vec<Account>, AppError>> + Send {
        T::accounts(self, profile)
    }

    fn chatgpt_account_id(&self, profile: &ProfileId, account: &AccountId) -> Option<String> {
        T::chatgpt_account_id(self, profile, account)
    }

    fn usable(&self, profile: &ProfileId) -> bool {
        T::usable(self, profile)
    }

    fn api_call(
        &self,
        profile: &ProfileId,
        body: Value,
    ) -> impl Future<Output = Result<Value, AppError>> + Send {
        T::api_call(self, profile, body)
    }
}

/// A request the core sends upstream, putting the account's token in place of `$TOKEN$`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamRequest {
    pub method: &'static str,
    pub url: String,
    pub header: Vec<(&'static str, String)>,
    pub data: Option<String>,
}

impl UpstreamRequest {
    fn bearer(method: &'static str, url: String) -> Self {
        Self {
            method,
            url,
            header: vec![
                ("Authorization", "Bearer $TOKEN$".to_owned()),
                ("Content-Type", "application/json".to_owned()),
            ],
            data: None,
        }
    }

    /// A call to `ChatGPT`'s Codex backend as the subscription `chatgpt_account_id`.
    pub(crate) fn codex(
        method: &'static str,
        url: String,
        chatgpt_account_id: Option<&str>,
    ) -> Self {
        let mut request = Self::bearer(method, url);
        if let Some(id) = chatgpt_account_id {
            request.header.push(("Chatgpt-Account-Id", id.to_owned()));
        }
        request.header.push(("User-Agent", USER_AGENT.to_owned()));
        request
    }

    /// The `/api-call` body: `{ auth_index, method, url, header, data }`.
    fn body(&self, auth_index: Option<&str>) -> Value {
        let mut body = Map::new();
        if let Some(index) = auth_index {
            body.insert("auth_index".to_owned(), json!(index));
        }
        body.insert("method".to_owned(), json!(self.method));
        body.insert("url".to_owned(), json!(self.url));
        let header: Map<String, Value> = self
            .header
            .iter()
            .map(|(name, value)| ((*name).to_owned(), json!(value)))
            .collect();
        body.insert("header".to_owned(), Value::Object(header));
        if let Some(data) = &self.data {
            body.insert("data".to_owned(), json!(data));
        }
        Value::Object(body)
    }
}

/// The core's answer to an upstream call: the upstream status and body.
pub(crate) struct Answer {
    pub(crate) status: Option<f64>,
    pub(crate) body: Value,
}

/// Sends `request` through the profile's core with the account's token.
pub(crate) async fn call_upstream(
    source: &impl QuotaSource,
    profile: &ProfileId,
    auth_index: Option<&str>,
    request: &UpstreamRequest,
) -> Result<Answer, AppError> {
    let response = source.api_call(profile, request.body(auth_index)).await?;
    Ok(Answer {
        status: js::number(js::field(Some(&response), "status_code")),
        body: js::field(Some(&response), "body")
            .cloned()
            .unwrap_or(Value::Null),
    })
}

/// The status as a message prints it.
pub(crate) fn status_text(status: Option<f64>) -> String {
    status.map_or_else(|| "unknown".to_owned(), js::number_string)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("Quota response was not valid JSON.")]
    Json,
    /// A relative reset past the range of a date (`toISOString` threw a `RangeError`).
    #[error("Invalid time value")]
    Time,
}

/// A provider whose quota the server reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuotaProvider {
    Claude,
    Codex,
    Kimi,
    Antigravity,
    Xai,
    OpencodeGo,
}

struct Reading {
    windows: Vec<QuotaWindow>,
    banked_resets: Option<u32>,
}

impl From<Vec<QuotaWindow>> for Reading {
    fn from(windows: Vec<QuotaWindow>) -> Self {
        Self {
            windows,
            banked_resets: None,
        }
    }
}

impl QuotaProvider {
    fn from_id(provider: &str) -> Option<Self> {
        Some(match Provider::from_id(provider)? {
            Provider::Claude => Self::Claude,
            Provider::Codex => Self::Codex,
            Provider::Kimi => Self::Kimi,
            Provider::Antigravity => Self::Antigravity,
            Provider::Xai => Self::Xai,
            Provider::OpencodeGo => Self::OpencodeGo,
            Provider::Openai | Provider::Anthropic | Provider::Gemini | Provider::Custom => {
                return None;
            }
        })
    }

    /// OAuth accounts are asked through the core; `OpenCode` Go with its own key.
    const fn oauth(self) -> bool {
        !matches!(self, Self::OpencodeGo)
    }

    fn read(self, body: &Value, now: i64) -> Result<Reading, ParseError> {
        match self {
            Self::Claude => claude_quota(body, now).map(Reading::from),
            Self::Codex => codex_quota(body, now),
            Self::Kimi => kimi_quota(body, now).map(Reading::from),
            Self::Antigravity => Ok(antigravity_quota(body).into()),
            Self::Xai => Ok(xai_quota(body).into()),
            Self::OpencodeGo => opencode_quota(body, now).map(Reading::from),
        }
    }
}

/// The source that reads an account's quota; `None` for API accounts of other providers and
/// for OAuth providers' accounts that are not OAuth.
fn quota_provider(account: &Account) -> Option<QuotaProvider> {
    QuotaProvider::from_id(&account.provider)
        .filter(|provider| !provider.oauth() || account.kind == AccountKind::OAuth)
}

fn percent(value: f64) -> JsNumber {
    JsNumber(value.clamp(0.0, 100.0))
}

/// The percentage left of a used percentage.
fn remaining(used: Option<&Value>) -> Option<JsNumber> {
    js::number(used).map(|used| percent(100.0 - used))
}

/// A reset time from seconds or milliseconds since the epoch, or date text.
fn timestamp(value: Option<&Value>) -> Option<String> {
    let value = js::nullish(value).filter(|value| value.as_str() != Some(""))?;
    let time = match js::number(Some(value)) {
        Some(number) => js::time_clip(if number < 1e12 {
            number * 1000.0
        } else {
            number
        }),
        None => js::parse_date(&js::string(Some(value))),
    };
    time.map(iso_string)
}

/// An absolute reset time, else one relative to `now`.
fn reset(raw: Option<&Value>, now: i64) -> Result<Option<String>, ParseError> {
    if let Some(absolute) = timestamp(js::first(
        raw,
        &["resets_at", "reset_at", "resetAt", "resetsAt"],
    )) {
        return Ok(Some(absolute));
    }
    let seconds = js::number(js::first(
        raw,
        &[
            "reset_after_seconds",
            "resetInSec",
            "resetInSeconds",
            "reset_in_sec",
        ],
    ));
    match seconds.filter(|seconds| *seconds >= 0.0) {
        #[expect(
            clippy::cast_precision_loss,
            reason = "JavaScript added milliseconds as doubles"
        )]
        Some(seconds) => js::time_clip(now as f64 + seconds * 1000.0)
            .map(|time| Some(iso_string(time)))
            .ok_or(ParseError::Time),
        None => Ok(None),
    }
}

/// A window's name from its length in seconds.
fn duration_label(value: Option<&Value>, fallback: &str) -> String {
    match js::number(value) {
        Some(18_000.0) => "5-hour".to_owned(),
        Some(604_800.0) => "Weekly".to_owned(),
        Some(seconds) if (2_419_200.0..=2_764_800.0).contains(&seconds) => "Monthly".to_owned(),
        Some(seconds) if seconds > 0.0 => {
            format!("{}-hour", js::number_string((seconds / 3600.0).round()))
        }
        _ => fallback.to_owned(),
    }
}

fn window(
    label: impl Into<String>,
    remaining_percent: Option<JsNumber>,
    resets_at: Option<String>,
) -> QuotaWindow {
    QuotaWindow {
        label: label.into(),
        remaining_percent,
        resets_at,
        scoped: None,
    }
}

/// Claude's account-wide and per-model windows: key, label, scoped.
const CLAUDE_WINDOWS: [(&str, &str, bool); 6] = [
    ("five_hour", "5-hour", false),
    ("seven_day", "Weekly", false),
    ("seven_day_opus", "Opus weekly", true),
    ("seven_day_sonnet", "Sonnet weekly", true),
    ("seven_day_oauth_apps", "OAuth weekly", true),
    ("seven_day_cowork", "Cowork weekly", true),
];

fn claude_quota(body: &Value, now: i64) -> Result<Vec<QuotaWindow>, ParseError> {
    let body = Some(body);
    let mut windows = Vec::new();
    for (key, label, scoped) in CLAUDE_WINDOWS {
        let raw = js::field(body, key);
        if !js::non_empty_record(raw) {
            continue;
        }
        let mut entry = window(
            label,
            remaining(js::field(raw, "utilization")),
            reset(raw, now)?,
        );
        entry.scoped = scoped.then_some(True);
        windows.push(entry);
    }
    if let Some(Value::Array(limits)) = js::field(body, "limits") {
        for item in limits {
            let item = Some(item);
            let model = js::text(js::field(
                js::field(js::field(item, "scope"), "model"),
                "display_name",
            ))
            .map(js::trim);
            let Some(model) = model.filter(|model| !model.is_empty()) else {
                continue;
            };
            if js::text(js::field(item, "kind")) != Some("weekly_scoped") {
                continue;
            }
            let label = format!("{model} weekly");
            if windows.iter().any(|window| window.label == label) {
                continue;
            }
            let mut entry = window(
                label,
                remaining(js::field(item, "percent")),
                reset(item, now)?,
            );
            entry.scoped = Some(True);
            windows.push(entry);
        }
    }
    Ok(windows)
}

fn codex_quota(body: &Value, now: i64) -> Result<Reading, ParseError> {
    const SAFE: f64 = 9_007_199_254_740_991.0;
    let body = Some(body);
    let mut windows = Vec::new();
    let mut add = |limit: Option<&Value>, prefix: &str| -> Result<(), ParseError> {
        for (key, fallback) in [
            ("primary_window", "Session"),
            ("secondary_window", "Long window"),
        ] {
            let raw = js::field(limit, key);
            if !js::non_empty_record(raw) {
                continue;
            }
            windows.push(window(
                format!(
                    "{prefix}{}",
                    duration_label(js::field(raw, "limit_window_seconds"), fallback)
                ),
                remaining(js::field(raw, "used_percent")),
                reset(raw, now)?,
            ));
        }
        Ok(())
    };
    add(js::field(body, "rate_limit"), "")?;
    add(js::field(body, "code_review_rate_limit"), "Code review · ")?;
    if let Some(Value::Array(additional)) = js::field(body, "additional_rate_limits") {
        for item in additional {
            let item = Some(item);
            let name = js::first(item, &["limit_name", "metered_feature"])
                .map_or(Cow::Borrowed("Additional"), |name| js::string(Some(name)));
            add(js::field(item, "rate_limit"), &format!("{name} · "))?;
        }
    }
    let banked = js::number(js::field(
        js::field(body, "rate_limit_reset_credits"),
        "available_count",
    ));
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a whole, non-negative safe integer"
    )]
    let banked_resets = banked
        .filter(|count| count.fract() == 0.0 && (0.0..=SAFE).contains(count))
        .and_then(|count| u32::try_from(count as u64).ok());
    Ok(Reading {
        windows,
        banked_resets,
    })
}

fn opencode_quota(body: &Value, now: i64) -> Result<Vec<QuotaWindow>, ParseError> {
    let usage = js::field(Some(body), "usage");
    let mut windows = Vec::new();
    for (key, label) in [
        ("rolling", "5-hour"),
        ("weekly", "Weekly"),
        ("monthly", "Monthly"),
    ] {
        let raw = js::field(usage, key);
        if !js::non_empty_record(raw) {
            continue;
        }
        let used = js::first(
            raw,
            &[
                "usagePercent",
                "usedPercent",
                "used_percent",
                "utilization",
                "percent",
            ],
        );
        // A reported percentage wins, even an unreadable one; else the count of the limit.
        let remaining_percent = if let Some(used) = used {
            remaining(Some(used))
        } else {
            let limit = js::number(js::field(raw, "limit"));
            let count = js::number(js::field(raw, "used"));
            match (limit, count) {
                (Some(limit), Some(count)) if limit > 0.0 => {
                    Some(percent(100.0 - count / limit * 100.0))
                }
                _ => None,
            }
        };
        windows.push(window(label, remaining_percent, reset(raw, now)?));
    }
    Ok(windows)
}

fn kimi_quota(body: &Value, now: i64) -> Result<Vec<QuotaWindow>, ParseError> {
    let body = Some(body);
    let mut entries: Vec<Cow<'_, Value>> = Vec::new();
    let usage = js::record(js::field(body, "usage"));
    if !usage.is_empty() {
        let mut usage = usage.clone();
        usage.insert("label".to_owned(), json!("Weekly"));
        entries.push(Cow::Owned(Value::Object(usage)));
    }
    if let Some(Value::Array(limits)) = js::field(body, "limits") {
        entries.extend(limits.iter().map(Cow::Borrowed));
    }
    let mut windows = Vec::new();
    for item in &entries {
        let item = Some(item.as_ref());
        let detail = if js::non_empty_record(js::field(item, "detail")) {
            js::field(item, "detail")
        } else {
            item
        };
        let limit = js::number(js::field(detail, "limit"));
        let left = js::number(js::field(detail, "remaining"));
        let used = js::number(js::field(detail, "used"));
        let remaining_percent = match (limit, left, used) {
            (Some(limit), Some(left), _) if limit > 0.0 => Some(percent(left / limit * 100.0)),
            (Some(limit), None, Some(used)) if limit > 0.0 => {
                Some(percent(100.0 - used / limit * 100.0))
            }
            _ => None,
        };
        let absolute = js::first(detail, &["resetTime", "reset_time"])
            .or_else(|| js::nullish(js::field(item, "resetTime")));
        let resets_at = match timestamp(absolute) {
            Some(time) => Some(time),
            None => reset(detail, now)?,
        };
        let label = js::nullish(js::field(item, "label"))
            .map_or(Cow::Borrowed("Rate limit"), |label| js::string(Some(label)));
        windows.push(window(label, remaining_percent, resets_at));
    }
    Ok(windows)
}

fn antigravity_quota(body: &Value) -> Vec<QuotaWindow> {
    let body = Some(body);
    let buckets = js::nullish(js::field(body, "buckets"))
        .or_else(|| js::field(js::field(body, "quotaSummary"), "buckets"));
    let Some(Value::Array(buckets)) = buckets else {
        return Vec::new();
    };
    buckets
        .iter()
        .map(|item| {
            let item = Some(item);
            let fraction = js::number(js::first(
                item,
                &["remainingFraction", "remaining_fraction"],
            ));
            let label = js::first(item, &["modelId", "model_id"])
                .map_or(Cow::Borrowed("Model quota"), |label| {
                    js::string(Some(label))
                });
            window(
                label,
                fraction.map(|fraction| percent(fraction * 100.0)),
                timestamp(js::first(item, &["resetTime", "reset_time"])),
            )
        })
        .collect()
}

/// JavaScript truthiness.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

fn xai_quota(body: &Value) -> Vec<QuotaWindow> {
    let body = Some(body);
    let (weekly, monthly) = (js::field(body, "weekly"), js::field(body, "monthly"));
    let payloads = if truthy(weekly) || truthy(monthly) {
        vec![weekly, monthly]
    } else {
        vec![body]
    };
    let mut windows = Vec::new();
    for raw in payloads {
        let config = if js::non_empty_record(js::field(raw, "config")) {
            js::field(raw, "config")
        } else {
            raw
        };
        let period = js::first(config, &["current_period", "currentPeriod"]);
        let period_end = || timestamp(js::field(period, "end"));
        let used_percent = js::number(js::first(
            config,
            &["credit_usage_percent", "creditUsagePercent"],
        ));
        if used_percent.is_some() || js::string(js::field(period, "type")).contains("week") {
            windows.push(window(
                "Weekly",
                used_percent.map(|used| percent(100.0 - used)),
                period_end(),
            ));
        }
        if let Some(Value::Array(products)) = js::first(config, &["product_usage", "productUsage"])
        {
            for product in products {
                let product = Some(product);
                let label = js::nullish(js::field(product, "product"))
                    .map_or(Cow::Borrowed("Product quota"), |label| {
                        js::string(Some(label))
                    });
                windows.push(window(
                    label,
                    remaining(js::first(product, &["usage_percent", "usagePercent"])),
                    period_end(),
                ));
            }
        }
        if let Some(limit) = js::number(js::first(config, &["monthly_limit", "monthlyLimit"])) {
            let used = js::number(js::field(config, "used"));
            windows.push(window(
                "Monthly",
                used.filter(|_| limit > 0.0)
                    .map(|used| percent(100.0 - used / limit * 100.0)),
                timestamp(js::first(
                    config,
                    &["billing_period_end", "billingPeriodEnd"],
                )),
            ));
        }
    }
    windows
}

/// Reads a provider's quota answer, which may be wrapped in a JSON string, at `now`
/// (milliseconds since the epoch). Unknown providers read no windows.
///
/// # Errors
/// [`ParseError::Json`] for a wrapped document that is not JSON, [`ParseError::Time`] for a
/// relative reset past the range of a date.
pub fn parse_quota(provider: &str, payload: &Value, now: i64) -> Result<Quota, ParseError> {
    let body = js::unwrap(payload).map_err(|_| ParseError::Json)?;
    let reading = match QuotaProvider::from_id(provider) {
        Some(source) => source.read(&body, now)?,
        None => Reading::from(Vec::new()),
    };
    Ok(Quota {
        status: if reading.windows.is_empty() {
            QuotaStatus::Unavailable
        } else {
            QuotaStatus::Available
        },
        windows: reading.windows,
        checked_at: iso_string(now),
        error: None,
        plan: js::text(js::field(Some(&body), "plan_type")).map(str::to_owned),
        banked_resets: reading.banked_resets,
    })
}

/// A provider's name as messages show it.
fn provider_label(provider: &str) -> Result<String, RegistryError> {
    Ok(match Provider::from_id(provider) {
        Some(known) => registry()?.providers.get(known).label.clone(),
        None if provider.starts_with("openai-compatible-") => "Compatible API".to_owned(),
        None => provider.to_owned(),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum QuotaError {
    #[error(transparent)]
    App(#[from] AppError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

impl QuotaError {
    /// The HTTP status the API answers with.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::App(error) | Self::Store(StoreError::App(error)) => error.status,
            Self::Store(_) | Self::Registry(_) => 500,
        }
    }
}

/// What the API answers for a failed check.
impl From<QuotaError> for AppError {
    fn from(error: QuotaError) -> Self {
        let status = error.status();
        match error {
            QuotaError::App(error) => error,
            QuotaError::Store(error) => error.into(),
            QuotaError::Registry(error) => Self::new(error.to_string(), status),
        }
    }
}

/// Refreshes one account's quota.
pub trait RefreshQuota: Send + Sync {
    fn refresh(
        &self,
        profile: &ProfileId,
        account: &AccountId,
    ) -> impl Future<Output = Result<Quota, QuotaError>> + Send;
}

/// Quota checks, one at a time per account.
pub struct Quotas<S, F> {
    store: Store,
    source: S,
    fetch: F,
    /// Milliseconds since the epoch.
    clock: fn() -> i64,
    refreshing: Mutex<HashSet<(ProfileId, AccountId)>>,
}

/// Removes an account from the running checks when its check ends, finished or dropped.
struct Running<'a> {
    set: &'a Mutex<HashSet<(ProfileId, AccountId)>>,
    key: (ProfileId, AccountId),
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

impl<S: QuotaSource, F: Fetch> Quotas<S, F> {
    #[must_use]
    pub fn new(store: Store, source: S, fetch: F, clock: fn() -> i64) -> Self {
        Self {
            store,
            source,
            fetch,
            clock,
            refreshing: Mutex::new(HashSet::new()),
        }
    }

    #[must_use]
    pub fn source(&self) -> &S {
        &self.source
    }

    /// Asks one account's quota and stores it. A check already running for the account answers
    /// the stored quota. A dropped call stores nothing.
    ///
    /// # Errors
    /// 404 for an account the profile does not list, 409 when a check is running and nothing
    /// is stored yet, or a store failure. Upstream failures are stored as an `error` quota.
    pub async fn refresh(
        &self,
        profile: &ProfileId,
        account_id: &AccountId,
    ) -> Result<Quota, QuotaError> {
        let accounts = self.source.accounts(profile).await?;
        let account = accounts
            .into_iter()
            .find(|account| &account.id == account_id)
            .ok_or_else(|| AppError::new("Account not found.", 404))?;
        let prior = {
            let (profile, account) = (profile.clone(), account_id.clone());
            self.store
                .call(move |db| db.quota(&profile, &account))
                .await?
        };
        let key = (profile.clone(), account_id.clone());
        let running = {
            let mut refreshing = self
                .refreshing
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if refreshing.contains(&key) {
                return prior.ok_or_else(|| {
                    AppError::new("Quota refresh is already in progress.", 409).into()
                });
            }
            refreshing.insert(key.clone());
            Running {
                set: &self.refreshing,
                key,
            }
        };
        let quota = match quota_provider(&account) {
            None => Quota {
                status: QuotaStatus::Unavailable,
                windows: Vec::new(),
                checked_at: iso_string((self.clock)()),
                error: None,
                plan: None,
                banked_resets: None,
            },
            Some(source) => match self.ask(profile, &account, source).await {
                Ok(quota) => quota,
                Err(message) => {
                    let (windows, checked_at) = prior.map_or_else(
                        || (Vec::new(), iso_string((self.clock)())),
                        |prior| (prior.windows, prior.checked_at),
                    );
                    Quota {
                        status: QuotaStatus::Error,
                        windows,
                        checked_at,
                        error: Some(message),
                        plan: None,
                        banked_resets: None,
                    }
                }
            },
        };
        drop(running);
        let (profile, account, stored) = (profile.clone(), account_id.clone(), quota.clone());
        self.store
            .call(move |db| db.save_quota(&profile, &account, &stored))
            .await?;
        Ok(quota)
    }

    /// The provider's reading, or the message of what failed.
    async fn ask(
        &self,
        profile: &ProfileId,
        account: &Account,
        source: QuotaProvider,
    ) -> Result<Quota, String> {
        let auth_index = account.auth_index.as_deref();
        let send =
            async |request: UpstreamRequest, failure: Option<&str>| -> Result<Value, String> {
                let answer = call_upstream(&self.source, profile, auth_index, &request)
                    .await
                    .map_err(|error| error.message)?;
                if answer.status == Some(200.0) {
                    return Ok(answer.body);
                }
                if let Some(failure) = failure {
                    return Err(failure.to_owned());
                }
                let advice = if matches!(answer.status, Some(401.0 | 403.0)) {
                    "Reconnect this subscription."
                } else {
                    "Try again later."
                };
                let label = provider_label(&account.provider).map_err(|error| error.to_string())?;
                Err(format!(
                    "{label} returned HTTP {}. {advice}",
                    status_text(answer.status)
                ))
            };
        let payload = match source {
            QuotaProvider::Claude => {
                let mut request = UpstreamRequest::bearer(
                    "GET",
                    "https://api.anthropic.com/api/oauth/usage".to_owned(),
                );
                request
                    .header
                    .push(("anthropic-beta", CLAUDE_OAUTH_BETA.to_owned()));
                send(request, None).await?
            }
            QuotaProvider::Codex => {
                let id = self.source.chatgpt_account_id(profile, &account.id);
                send(
                    UpstreamRequest::codex("GET", format!("{CODEX_API}/usage"), id.as_deref()),
                    None,
                )
                .await?
            }
            QuotaProvider::Kimi => {
                send(
                    UpstreamRequest::bearer(
                        "GET",
                        "https://api.kimi.com/coding/v1/usages".to_owned(),
                    ),
                    None,
                )
                .await?
            }
            QuotaProvider::Antigravity => {
                let mut request = UpstreamRequest::bearer(
                    "POST",
                    "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary"
                        .to_owned(),
                );
                request.data = Some("{}".to_owned());
                send(request, None).await?
            }
            QuotaProvider::Xai => {
                // The monthly limit and the weekly credits are two calls.
                let monthly =
                    send(UpstreamRequest::bearer("GET", XAI_BILLING.to_owned()), None).await?;
                let weekly = send(
                    UpstreamRequest::bearer("GET", format!("{XAI_BILLING}?format=credits")),
                    Some("Grok weekly quota check failed. Try again later."),
                )
                .await?;
                let unwrap = |value: &Value| {
                    js::unwrap(value)
                        .map(Cow::into_owned)
                        .map_err(|_| NOT_JSON.to_owned())
                };
                json!({ "monthly": unwrap(&monthly)?, "weekly": unwrap(&weekly)? })
            }
            QuotaProvider::OpencodeGo => self.opencode_usage(&account.id).await?,
        };
        parse_quota(&account.provider, &payload, (self.clock)()).map_err(|error| error.to_string())
    }

    /// `OpenCode` Go's usage, asked with the account's own key.
    async fn opencode_usage(&self, account: &AccountId) -> Result<Value, String> {
        let key = {
            let account = account.clone();
            self.store
                .call(move |db| db.api_key(&account))
                .await
                .map_err(|error| error.to_string())?
        };
        let preset = registry()
            .map_err(|error| error.to_string())?
            .providers
            .get(Provider::OpencodeGo)
            .preset
            .as_ref()
            .map_or_else(String::new, |preset| preset.base_url.clone());
        let response = self
            .fetch
            .https(Request {
                url: format!("{preset}/usage"),
                headers: vec![
                    ("Authorization", format!("Bearer {}", key.expose_secret())),
                    ("User-Agent", USER_AGENT.to_owned()),
                ],
                timeout: OPENCODE_TIMEOUT,
                limit: OPENCODE_LIMIT,
            })
            .await
            .map_err(|error| error.to_string())?;
        if !response.ok() {
            return Err(format!(
                "OpenCode Go returned HTTP {}. Check your API key or try again later.",
                response.status.as_u16()
            ));
        }
        serde_json::from_slice(&response.body).map_err(|_| NOT_JSON.to_owned())
    }
}

impl<F: Fetch> Quotas<PoolSource, F> {
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

impl<S: QuotaSource, F: Fetch> RefreshQuota for Quotas<S, F> {
    fn refresh(
        &self,
        profile: &ProfileId,
        account: &AccountId,
    ) -> impl Future<Output = Result<Quota, QuotaError>> + Send {
        Quotas::refresh(self, profile, account)
    }
}
