//! A profile's usage hub: the slice of `CLIProxyAPI`'s management API that T3 Code reads
//! subscription limits through, answered from the profile's stored accounts and its running
//! core. Only the upstream calls [`Upstream`] names reach the core, built with this app's own
//! headers: of what the client sends, only that choice, the auth index and a redemption's two
//! ids get through. Nothing here starts a core.

use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::js;
use nsv_domain::registry::Provider;
use nsv_domain::wire::{Account, JsNumber};
use nsv_net::Method;
use serde_json::{Map, Value, json};

use super::Cores;
use crate::quota::{QuotaSource, UpstreamRequest, call_upstream};

/// An upstream request a usage hub client may have a profile's core send: T3 Code's calls,
/// exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Upstream {
    CodexUsage,
    CodexResetCredits,
    CodexRedeemReset,
    ClaudeUsage,
    XaiCredits,
}

impl Upstream {
    const ALL: [Self; 5] = [
        Self::CodexUsage,
        Self::CodexResetCredits,
        Self::CodexRedeemReset,
        Self::ClaudeUsage,
        Self::XaiCredits,
    ];

    /// Its method, its exact URL, and the provider whose token it carries.
    const fn spec(self) -> (&'static str, &'static str, Provider) {
        match self {
            Self::CodexUsage => (
                "GET",
                "https://chatgpt.com/backend-api/wham/usage",
                Provider::Codex,
            ),
            Self::CodexResetCredits => (
                "GET",
                "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits",
                Provider::Codex,
            ),
            Self::CodexRedeemReset => (
                "POST",
                "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume",
                Provider::Codex,
            ),
            Self::ClaudeUsage => (
                "GET",
                "https://api.anthropic.com/api/oauth/usage",
                Provider::Claude,
            ),
            Self::XaiCredits => (
                "GET",
                "https://cli-chat-proxy.grok.com/v1/billing?format=credits",
                Provider::Xai,
            ),
        }
    }

    /// The call `method` and `url` name, compared exactly: another case, query or trailing
    /// slash names none.
    fn parse(method: &str, url: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|upstream| {
            let (allowed_method, allowed_url, _) = upstream.spec();
            allowed_method == method && allowed_url == url
        })
    }

    const fn provider(self) -> Provider {
        self.spec().2
    }

    /// The request with the headers this app's own quota and reset reads send.
    fn request(self, chatgpt_account_id: Option<&str>, data: Option<String>) -> UpstreamRequest {
        let (method, url, _) = self.spec();
        let url = url.to_owned();
        let mut request = match self {
            Self::CodexUsage | Self::CodexResetCredits | Self::CodexRedeemReset => {
                UpstreamRequest::codex(method, url, chatgpt_account_id)
            }
            Self::ClaudeUsage => UpstreamRequest::claude(method, url),
            Self::XaiCredits => UpstreamRequest::bearer(method, url),
        };
        request.data = data;
        request
    }
}

/// An `api-call` body as the hub reads it; `header` and any other field are ignored.
struct ApiCall {
    auth_index: String,
    upstream: Upstream,
    /// A redemption's ids, re-encoded.
    data: Option<String>,
}

impl ApiCall {
    /// # Errors
    /// 400 for a body without `auth_index`, `method` and `url` strings, or with data the call
    /// does not take; 403 for a call [`Upstream`] does not name.
    fn parse(body: &Value) -> Result<Self, AppError> {
        let text = |key| body.get(key).and_then(Value::as_str);
        let (Some(auth_index), Some(method), Some(url)) =
            (text("auth_index"), text("method"), text("url"))
        else {
            return Err(AppError::new(
                "The api-call body needs auth_index, method and url strings.",
                400,
            ));
        };
        let upstream = Upstream::parse(method, url)
            .ok_or_else(|| AppError::new("This upstream request is not allowed.", 403))?;
        let data = match body.get("data") {
            None => None,
            Some(Value::String(data)) => Some(data.as_str()),
            Some(_) => return Err(AppError::new("The api-call data must be a string.", 400)),
        };
        let data = match (upstream, data) {
            (Upstream::CodexRedeemReset, data) => Some(redemption(data)?),
            (_, Some(_)) => {
                return Err(AppError::new("A GET upstream request takes no data.", 400));
            }
            (_, None) => None,
        };
        Ok(Self {
            auth_index: auth_index.to_owned(),
            upstream,
            data,
        })
    }
}

/// A Codex reset redemption with exactly its two ids, encoded again from them alone.
fn redemption(data: Option<&str>) -> Result<String, AppError> {
    let invalid = || {
        AppError::new(
            "A reset redemption takes only a redeem_request_id and a credit_id.",
            400,
        )
    };
    let value: Value = serde_json::from_str(data.ok_or_else(invalid)?).map_err(|_| invalid())?;
    let fields = value
        .as_object()
        .filter(|fields| fields.len() == 2)
        .ok_or_else(invalid)?;
    let id = |key| fields.get(key).and_then(Value::as_str).ok_or_else(invalid);
    Ok(
        json!({ "redeem_request_id": id("redeem_request_id")?, "credit_id": id("credit_id")? })
            .to_string(),
    )
}

/// An account the hub lists: one of a provider an [`Upstream`] reads, with an auth index.
/// Stored account metadata holds only OAuth accounts.
fn listed(account: &Account) -> bool {
    account
        .auth_index
        .as_deref()
        .is_some_and(|index| !index.is_empty())
        && Upstream::ALL
            .iter()
            .any(|upstream| upstream.provider().as_str() == account.provider)
}

impl Cores {
    /// The profile's hub accounts as its last account sync stored them.
    async fn hub_accounts(&self, profile: &ProfileId) -> Result<Vec<Account>, AppError> {
        let id = profile.clone();
        let accounts = self.store.call(move |db| db.account_metadata(&id)).await?;
        Ok(accounts.into_iter().filter(listed).collect())
    }

    /// The hub account with `auth_index`.
    async fn hub_account(
        &self,
        profile: &ProfileId,
        auth_index: &str,
    ) -> Result<Account, AppError> {
        self.hub_accounts(profile)
            .await?
            .into_iter()
            .find(|account| account.auth_index.as_deref() == Some(auth_index))
            .ok_or_else(AppError::account_not_found)
    }

    /// `GET auth-files`: each hub account's id, auth index, provider, email, whether it is
    /// paused, and for Codex its `ChatGPT` account id once a sync read it. Answered from the
    /// store, so a stopped profile lists its accounts too.
    ///
    /// # Errors
    /// A store failure.
    pub async fn hub_files(&self, profile: &ProfileId) -> Result<Value, AppError> {
        let files: Vec<Value> = self
            .hub_accounts(profile)
            .await?
            .into_iter()
            .map(|account| {
                let chatgpt_account_id = (account.provider == Provider::Codex.as_str())
                    .then(|| {
                        self.accounts
                            .chatgpt_account_id(profile, account.id.as_str())
                    })
                    .flatten();
                let mut file = Map::new();
                file.insert("id".to_owned(), json!(account.id));
                file.insert("auth_index".to_owned(), json!(account.auth_index));
                file.insert("provider".to_owned(), json!(account.provider));
                if let Some(email) = account.email {
                    file.insert("email".to_owned(), json!(email));
                }
                file.insert("disabled".to_owned(), json!(account.disabled));
                if let Some(id) = chatgpt_account_id {
                    file.insert("id_token".to_owned(), json!({ "chatgpt_account_id": id }));
                }
                Value::Object(file)
            })
            .collect();
        Ok(json!({ "files": files }))
    }

    /// `GET auth-files/models?name=`: the ids of the models the profile's core routes to the
    /// hub account `name`.
    ///
    /// # Errors
    /// 404 when `name` is no hub account, 409 while the core is not running, 502 for a core
    /// answer without a model list.
    pub async fn hub_models(&self, profile: &ProfileId, name: &str) -> Result<Value, AppError> {
        if !self
            .hub_accounts(profile)
            .await?
            .iter()
            .any(|account| account.id.as_str() == name)
        {
            return Err(AppError::account_not_found());
        }
        let path = format!("/auth-files/models?name={}", js::encode_uri_component(name));
        let answer = self
            .pool
            .management(profile, &Method::GET, &path, None)
            .await?;
        let models: Vec<Value> = answer
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| AppError::new("The proxy returned an unexpected model list.", 502))?
            .iter()
            .filter_map(|model| model.get("id").and_then(Value::as_str))
            .map(|id| json!({ "id": id }))
            .collect();
        Ok(json!({ "models": models }))
    }

    /// `POST api-call`: one of T3 Code's upstream calls through the profile's core with a hub
    /// account's token. A non-2xx upstream answer comes back with an empty body, so no provider
    /// error body leaves the daemon.
    ///
    /// # Errors
    /// 400 for a body without `auth_index`, `method` and `url` strings or with data the call
    /// does not take, 403 for any other call, 404 for an auth index no hub account has, 409 for
    /// a paused account or while the core is not running, 403 when the call is for another
    /// provider than the account's, 502 for a core answer without a status.
    pub async fn hub_api_call(&self, profile: &ProfileId, body: &Value) -> Result<Value, AppError> {
        let call = ApiCall::parse(body)?;
        let account = self.hub_account(profile, &call.auth_index).await?;
        if account.disabled {
            return Err(AppError::new("This account is paused.", 409));
        }
        let provider = call.upstream.provider();
        if provider.as_str() != account.provider {
            return Err(AppError::new(
                "This upstream request is for another provider than the account's.",
                403,
            ));
        }
        let chatgpt_account_id = (provider == Provider::Codex)
            .then(|| {
                self.accounts
                    .chatgpt_account_id(profile, account.id.as_str())
            })
            .flatten();
        let request = call
            .upstream
            .request(chatgpt_account_id.as_deref(), call.data);
        let answer = call_upstream(
            self.quotas.source(),
            profile,
            Some(&call.auth_index),
            &request,
        )
        .await?;
        let status = answer.status.ok_or_else(|| {
            AppError::new("The proxy returned an unexpected upstream answer.", 502)
        })?;
        let body = if (200.0..300.0).contains(&status) {
            match answer.body {
                Value::String(text) => text,
                other => other.to_string(),
            }
        } else {
            String::new()
        };
        Ok(json!({ "status_code": JsNumber(status), "body": body }))
    }

    /// `POST reset-quota`: the core forgets a hub account's quota cooldown, as after this
    /// app's own reset redemption.
    ///
    /// # Errors
    /// 400 for a body without an `auth_index` string, 404 for one no hub account has, 409
    /// while the core is not running.
    pub async fn hub_reset_quota(
        &self,
        profile: &ProfileId,
        body: &Value,
    ) -> Result<Value, AppError> {
        let auth_index = body
            .get("auth_index")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AppError::new("The reset-quota body needs an auth_index string.", 400)
            })?;
        self.hub_account(profile, auth_index).await?;
        self.quotas
            .source()
            .reset_cooldown(profile, auth_index)
            .await?;
        Ok(json!({ "status": "ok" }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_the_calls_t3_code_sends_are_allowed() {
        let allowed = [
            (
                "GET",
                "https://chatgpt.com/backend-api/wham/usage",
                Upstream::CodexUsage,
            ),
            (
                "GET",
                "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits",
                Upstream::CodexResetCredits,
            ),
            (
                "POST",
                "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume",
                Upstream::CodexRedeemReset,
            ),
            (
                "GET",
                "https://api.anthropic.com/api/oauth/usage",
                Upstream::ClaudeUsage,
            ),
            (
                "GET",
                "https://cli-chat-proxy.grok.com/v1/billing?format=credits",
                Upstream::XaiCredits,
            ),
        ];
        for (method, url, upstream) in allowed {
            assert_eq!(
                Upstream::parse(method, url),
                Some(upstream),
                "{method} {url}"
            );
        }
        let refused = [
            ("GET", "https://chatgpt.com/backend-api/wham/tasks"),
            ("GET", "https://chatgpt.com/backend-api/wham/usage?x=1"),
            ("GET", "https://chatgpt.com/backend-api/wham/usage/"),
            ("GET", "http://chatgpt.com/backend-api/wham/usage"),
            ("GET", "https://CHATGPT.com/backend-api/wham/usage"),
            ("POST", "https://chatgpt.com/backend-api/wham/usage"),
            (
                "GET",
                "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume",
            ),
            ("get", "https://chatgpt.com/backend-api/wham/usage"),
            (
                "GET",
                "https://api.anthropic.com/api/oauth/usage?cedar_ember=1",
            ),
            ("GET", "https://cli-chat-proxy.grok.com/v1/billing"),
            (
                "GET",
                "https://cli-chat-proxy.grok.com/v1/billing?format=credits&x=1",
            ),
            ("GET", ""),
        ];
        for (method, url) in refused {
            assert_eq!(Upstream::parse(method, url), None, "{method} {url}");
        }
    }
}
