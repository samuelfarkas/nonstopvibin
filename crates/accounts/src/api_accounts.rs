//! The API-key account an input describes, its models entered or discovered
//! (`src/server/api-accounts.ts`). Nothing is saved here.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, InvalidId};
use nsv_domain::js;
use nsv_domain::random;
use nsv_domain::registry::{Protocol, Provider, RegistryError, Upstream, registry};
use nsv_domain::wire::{ApiAccount, ApiAccountInput, ApiModel, Extra};
use nsv_net::{Loopback, NetError, USER_AGENT};
use serde_json::Value;

use crate::fetch::{Fetch, Request};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);
const DISCOVERY_LIMIT: u64 = 16 * 1024 * 1024;
/// The most discovered models kept.
const DISCOVERED_MODELS: usize = 500;

#[derive(Debug, thiserror::Error)]
pub enum ApiAccountError {
    #[error(transparent)]
    App(#[from] AppError),
    /// `new URL(baseUrl)` threw.
    #[error("Invalid URL")]
    InvalidUrl,
    #[error("model discovery failed: {0}")]
    Net(#[from] NetError),
    #[error("model discovery answered invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("no random account id: {0}")]
    Random(#[from] std::io::Error),
    #[error(transparent)]
    Id(#[from] InvalidId),
}

impl ApiAccountError {
    /// The HTTP status the API answers with.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::App(error) => error.status,
            _ => 500,
        }
    }
}

/// What the API answers for an account it cannot add.
impl From<ApiAccountError> for AppError {
    fn from(error: ApiAccountError) -> Self {
        match error {
            ApiAccountError::App(error) => error,
            other => Self::new(other.to_string(), other.status()),
        }
    }
}

/// `OpenCode` Go publishes different wire protocols per model. Live endpoint metadata wins over
/// the model family.
#[must_use]
pub fn go_model_protocol(id: &str, endpoint: Option<&str>) -> Protocol {
    let endpoint = endpoint.unwrap_or_default();
    if endpoint.contains("/messages") {
        Protocol::Anthropic
    } else if endpoint.contains("/responses") {
        Protocol::Responses
    } else if endpoint.contains("/chat/completions") {
        Protocol::Openai
    } else if ["gpt-", "grok-", "muse-"]
        .iter()
        .any(|family| id.starts_with(family))
    {
        Protocol::Responses
    } else if ["minimax-", "qwen"]
        .iter()
        .any(|family| id.starts_with(family))
    {
        Protocol::Anthropic
    } else {
        Protocol::Openai
    }
}

/// How each model of a preset with an upstream picks its protocol.
fn model_protocol(upstream: Upstream, id: &str, endpoint: Option<&str>) -> Protocol {
    match upstream {
        Upstream::OpencodeGo => go_model_protocol(id, endpoint),
    }
}

/// Validates `input` and answers the account it describes with a new id. A preset with an
/// upstream sets each entered model's protocol; no entered models asks the provider's
/// `/models`.
///
/// # Errors
/// 400 for a base URL that is not HTTPS (or HTTP to a local provider) or carries credentials,
/// a query or a fragment, for a preset's other endpoint, and when no models are found; 502 for
/// a discovery answer that is not 2xx; [`ApiAccountError::InvalidUrl`] for text that is not a
/// URL; a network or JSON failure of discovery.
pub async fn api_account(
    input: &ApiAccountInput,
    fetch: &impl Fetch,
) -> Result<ApiAccount, ApiAccountError> {
    let url = ProviderUrl::parse(&input.base_url).ok_or(ApiAccountError::InvalidUrl)?;
    if !url.allowed() {
        return Err(AppError::new(
            "Use an HTTPS base URL, or HTTP for a local provider. Do not include credentials or query parameters in the URL.",
            400,
        )
        .into());
    }
    let base_url = input.base_url.strip_suffix('/').unwrap_or(&input.base_url);
    let preset = registry()?.providers.get(input.provider).preset.as_ref();
    if let Some(preset) =
        preset.filter(|preset| preset.fixed_endpoint && base_url != preset.base_url)
    {
        return Err(AppError::new(
            format!("{} must use its official endpoint.", preset.name),
            400,
        )
        .into());
    }
    let upstream = preset.and_then(|preset| preset.upstream);
    let mut models: Vec<ApiModel> = input
        .models
        .iter()
        .map(|model| ApiModel {
            id: model.id.clone(),
            protocol: upstream.map_or(model.protocol, |upstream| {
                model_protocol(upstream, &model.id, None)
            }),
            extra: Extra::new(),
        })
        .collect();
    if models.is_empty() {
        models = discover_models(input, base_url, upstream, fetch).await?;
    }
    if models.is_empty() {
        return Err(AppError::new(
            "No models were found. Enter the model IDs your provider supports.",
            400,
        )
        .into());
    }
    Ok(ApiAccount {
        id: AccountId::parse(&random::uuid()?)?,
        name: input.name.clone(),
        provider: input.provider,
        base_url: base_url.to_owned(),
        prefix: input.prefix.clone(),
        models,
        disabled: false,
        extra: Extra::new(),
    })
}

async fn discover_models(
    input: &ApiAccountInput,
    base_url: &str,
    upstream: Option<Upstream>,
    fetch: &impl Fetch,
) -> Result<Vec<ApiModel>, ApiAccountError> {
    let key = input.api_key.expose_secret();
    let anthropic = input.provider == Provider::Anthropic || input.protocol == Protocol::Anthropic;
    let mut headers = if anthropic {
        vec![
            ("x-api-key", key.to_owned()),
            ("anthropic-version", "2023-06-01".to_owned()),
        ]
    } else {
        vec![("Authorization", format!("Bearer {key}"))]
    };
    headers.push(("User-Agent", USER_AGENT.to_owned()));
    let url = format!("{base_url}/models");
    let parsed = ProviderUrl::parse(&url).ok_or(ApiAccountError::InvalidUrl)?;
    let local = parsed.loopback().map(|to| (to, parsed.target.to_owned()));
    let request = Request {
        url,
        headers,
        timeout: DISCOVERY_TIMEOUT,
        limit: DISCOVERY_LIMIT,
    };
    let response = match local {
        Some((to, target)) => fetch.loopback(to, target, request).await?,
        None => fetch.https(request).await?,
    };
    if !response.ok() {
        return Err(AppError::new(
            format!(
                "Model discovery returned HTTP {}. Verify the API key, or enter model IDs manually.",
                response.status.as_u16()
            ),
            502,
        )
        .into());
    }
    let body: Value = serde_json::from_slice(&response.body)?;
    let Some(Value::Array(data)) = js::field(Some(&body), "data") else {
        return Ok(Vec::new());
    };
    Ok(data
        .iter()
        .filter_map(|raw| {
            let id = js::text(js::field(Some(raw), "id"))?;
            let protocol = match upstream {
                Some(upstream) => {
                    model_protocol(upstream, id, js::text(js::field(Some(raw), "endpoint")))
                }
                None if input.provider == Provider::Anthropic => Protocol::Anthropic,
                None => input.protocol,
            };
            Some(ApiModel {
                id: id.to_owned(),
                protocol,
                extra: Extra::new(),
            })
        })
        .take(DISCOVERED_MODELS)
        .collect())
}

/// The parts of a WHATWG URL the base-URL rule reads. Hosts are compared as written (in
/// lowercase): IPv4 shorthands such as `127.1` and other spellings of `::1` are not local.
struct ProviderUrl<'a> {
    scheme: String,
    credentials: bool,
    host: String,
    port: Option<u16>,
    /// The path and query after the host, `/` when empty.
    target: &'a str,
    query: bool,
    fragment: bool,
}

impl<'a> ProviderUrl<'a> {
    /// `None` where `new URL(text)` throws.
    fn parse(text: &'a str) -> Option<Self> {
        let text = text.trim_matches(|c: char| c <= ' ');
        let (scheme, rest) = text.split_once(':')?;
        let mut chars = scheme.chars();
        if !chars.next()?.is_ascii_alphabetic()
            || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            return None;
        }
        let scheme = scheme.to_ascii_lowercase();
        let (query, fragment) = {
            let (before, fragment) = rest.split_once('#').unwrap_or((rest, ""));
            let query = before
                .split_once('?')
                .is_some_and(|(_, query)| !query.is_empty());
            (query, !fragment.is_empty())
        };
        if !matches!(scheme.as_str(), "http" | "https" | "ftp" | "ws" | "wss") {
            // A URL of another scheme; the rule refuses it whatever follows.
            return Some(Self {
                scheme,
                credentials: false,
                host: String::new(),
                port: None,
                target: "/",
                query,
                fragment,
            });
        }
        let rest = rest.trim_start_matches(['/', '\\']);
        let end = rest.find(['/', '\\', '?', '#']).unwrap_or(rest.len());
        let (authority, after) = rest.split_at(end);
        let (userinfo, host_port) = authority.rsplit_once('@').unwrap_or(("", authority));
        let (host, port) = if host_port.starts_with('[') {
            let close = host_port.find(']')?;
            let (host, port) = host_port.split_at(close + 1);
            (host, port)
        } else {
            host_port.split_at(host_port.find(':').unwrap_or(host_port.len()))
        };
        let port = match port.strip_prefix(':') {
            None if port.is_empty() => None,
            Some("") => None,
            Some(digits) if digits.bytes().all(|b| b.is_ascii_digit()) => {
                Some(digits.parse().ok()?)
            }
            _ => return None,
        };
        let forbidden = |c: char| " #%/:<>?@[\\]^|".contains(c) || c.is_control();
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty()
            || (!host.starts_with('[') && host.chars().any(forbidden))
            || bare.is_empty()
        {
            return None;
        }
        let target_end = after.find('#').unwrap_or(after.len());
        let target = after
            .get(..target_end)
            .filter(|path| path.starts_with('/'))
            .unwrap_or("/");
        Some(Self {
            scheme,
            credentials: !userinfo.is_empty() && userinfo != ":",
            host: host.to_ascii_lowercase(),
            port,
            target,
            query,
            fragment,
        })
    }

    fn local(&self) -> bool {
        self.scheme == "http" && matches!(self.host.as_str(), "127.0.0.1" | "localhost" | "[::1]")
    }

    fn allowed(&self) -> bool {
        !self.credentials
            && !self.query
            && !self.fragment
            && (self.scheme == "https" || self.local())
    }

    /// Where a local provider listens; `localhost` resolves to 127.0.0.1.
    fn loopback(&self) -> Option<Loopback> {
        if !self.local() {
            return None;
        }
        let ip = if self.host == "[::1]" {
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        } else {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        };
        Loopback::new(SocketAddr::new(ip, self.port.unwrap_or(80)))
    }
}
