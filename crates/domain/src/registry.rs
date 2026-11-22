//! The provider, agent and vocabulary data the server shares with the renderer.
//!
//! `src/shared/providers.ts`, `agents.ts` and `vocabulary.ts` stay the source.
//! `scripts/export-registry.ts` writes them to `data/registry.json`, which this module embeds
//! and parses once. Every registry type denies unknown fields, so a field added in TypeScript
//! fails the parse test until Rust models it. The closed enums below are hand-written; the
//! tests prove they equal the JSON keys and lists in both directions.

use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

const SOURCE: &str = include_str!("../data/registry.json");

/// The embedded `registry.json` does not match the registry types.
#[derive(Debug, Clone, thiserror::Error)]
#[error("the embedded registry.json does not match the registry types: {0}")]
pub struct RegistryError(Arc<serde_json::Error>);

/// The registry, parsed on first use.
///
/// # Errors
/// When the embedded JSON does not match the types; the same error on every call.
pub fn registry() -> Result<&'static Registry, RegistryError> {
    static REGISTRY: OnceLock<Result<Registry, RegistryError>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            serde_json::from_str(SOURCE).map_err(|error| RegistryError(Arc::new(error)))
        })
        .as_ref()
        .map_err(Clone::clone)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Registry {
    pub providers: Providers,
    pub agents: Agents,
    /// Equal to [`Protocol::ALL`].
    pub protocols: Vec<Protocol>,
    /// Equal to [`Strategy::ALL`].
    pub strategies: Vec<Strategy>,
    /// Equal to [`RetentionDays::ALL`].
    pub retention_days: Vec<RetentionDays>,
    pub limits: Limits,
    /// Usage records per page: the core's drain, history pages and the Activity sample.
    pub usage_page_size: u32,
    pub quota: QuotaTiming,
    /// Equal to [`ResetResultCode::ALL`].
    pub reset_result_codes: Vec<ResetResultCode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    /// Profile and account names, in UTF-16 code units ([`crate::wire::utf16_len`]).
    pub name_length: usize,
    /// API-key model prefixes, in UTF-16 code units.
    pub prefix_length: usize,
    /// Routing priority runs from `-priority_limit` to `priority_limit`.
    pub priority_limit: i64,
    /// Largest credential file accepted for import or read from a profile.
    pub credential_file_bytes: u64,
    /// The usage range that means the whole history.
    pub all_history_days: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuotaTiming {
    /// How often the server refreshes every account's quota.
    pub refresh_ms: u64,
    pub stale_ms: u64,
}

/// Defines a closed string enum with `ALL` in source order and `as_str`/`from_id`.
macro_rules! closed_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $id:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[cfg_attr(test, derive(ts_rs::TS))]
        pub enum $name {
            $(#[serde(rename = $id)] $variant),+
        }

        impl $name {
            pub const ALL: &[Self] = &[$(Self::$variant),+];

            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $id),+
                }
            }

            /// The variant whose wire string is `id`.
            #[must_use]
            pub fn from_id(id: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|variant| variant.as_str() == id)
            }
        }
    };
}

closed_enum!(
    /// A provider the server supports, in the order of the app's pickers. Accounts and usage
    /// records carry the core's provider strings, which include unknown ones, so they stay
    /// `String`.
    Provider {
        Claude = "claude",
        Codex = "codex",
        Antigravity = "antigravity",
        Kimi = "kimi",
        Xai = "xai",
        OpencodeGo = "opencode-go",
        Openai = "openai",
        Anthropic = "anthropic",
        Gemini = "gemini",
        Custom = "custom",
    }
);
closed_enum!(
    /// A provider with browser sign-in through the core.
    OAuthProvider {
        Claude = "claude",
        Codex = "codex",
        Antigravity = "antigravity",
        Kimi = "kimi",
        Xai = "xai",
    }
);
closed_enum!(
    Protocol {
        Openai = "openai",
        Anthropic = "anthropic",
        Responses = "responses",
    }
);
closed_enum!(
    /// A coding agent nonstopvibin connects, in the app's tab order.
    Agent {
        Claude = "claude",
        Codex = "codex",
        Pi = "pi",
        Opencode = "opencode",
    }
);
closed_enum!(
    Strategy {
        RoundRobin = "round-robin",
        FillFirst = "fill-first",
    }
);
closed_enum!(
    ResetResultCode {
        Reset = "reset",
        AlreadyRedeemed = "already_redeemed",
        NothingToReset = "nothing_to_reset",
        NoCredit = "no_credit",
    }
);
closed_enum!(
    /// Server rules an API-key preset can name.
    Upstream { OpencodeGo = "opencode-go" }
);
closed_enum!(
    /// How an agent asks the gateway for its model list.
    AgentCatalog {
        Header = "header",
        Query = "query",
    }
);

impl From<OAuthProvider> for Provider {
    fn from(provider: OAuthProvider) -> Self {
        match provider {
            OAuthProvider::Claude => Self::Claude,
            OAuthProvider::Codex => Self::Codex,
            OAuthProvider::Antigravity => Self::Antigravity,
            OAuthProvider::Kimi => Self::Kimi,
            OAuthProvider::Xai => Self::Xai,
        }
    }
}

/// Days of usage history kept, a JSON number on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
#[cfg_attr(test, derive(ts_rs::TS), ts(type = "0 | 30 | 90 | 365"))]
pub enum RetentionDays {
    /// 0: keeps everything.
    All,
    Days30,
    Days90,
    Days365,
}

impl RetentionDays {
    pub const ALL: &[Self] = &[Self::All, Self::Days30, Self::Days90, Self::Days365];

    #[must_use]
    pub const fn days(self) -> u16 {
        match self {
            Self::All => 0,
            Self::Days30 => 30,
            Self::Days90 => 90,
            Self::Days365 => 365,
        }
    }
}

/// A day count that is not one of [`RetentionDays::ALL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a usage retention period")]
pub struct InvalidRetention(pub u16);

impl TryFrom<u16> for RetentionDays {
    type Error = InvalidRetention;
    fn try_from(days: u16) -> Result<Self, InvalidRetention> {
        Self::ALL
            .iter()
            .copied()
            .find(|value| value.days() == days)
            .ok_or(InvalidRetention(days))
    }
}

impl From<RetentionDays> for u16 {
    fn from(value: RetentionDays) -> Self {
        value.days()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags of the TypeScript registry entry"
)]
pub struct ProviderEntry {
    /// Short name in account lists, quotas and history.
    pub label: String,
    /// The first release shows it; the server supports every entry.
    pub visible: bool,
    /// `public/providers/<icon>.svg`.
    pub icon: Option<String>,
    /// models.dev provider ID, where it differs.
    pub catalog: Option<String>,
    /// Its models are this agent's vendor's own and offered to it by default.
    pub native_agent: Option<Agent>,
    /// Pages the desktop app may open for it: sign-in, account and key pages.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// `CLIProxyAPI` credential files of this type can be imported.
    #[serde(default)]
    pub credential_file: bool,
    pub sign_in: Option<SignIn>,
    /// Accepted as an API-key account.
    #[serde(default)]
    pub api_key: bool,
    pub preset: Option<Preset>,
    /// Banked rate-limit resets can be redeemed for its signed-in accounts.
    #[serde(default)]
    pub banked_resets: bool,
    /// One email can hold seats in several organizations, each its own account.
    #[serde(default)]
    pub organizations: bool,
    /// A sign-in can replace an existing account's credential in place (organizations only).
    #[serde(default)]
    pub reconnect: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignIn {
    pub name: String,
    pub detail: String,
    /// The core's name for the provider, where it differs.
    pub core: Option<String>,
    /// Quota windows it reports, shown before anything is connected.
    pub limits: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Preset {
    pub name: String,
    pub detail: String,
    pub base_url: String,
    pub prefix: String,
    pub protocol: Protocol,
    /// Only the official endpoint is accepted.
    #[serde(default)]
    pub fixed_endpoint: bool,
    pub upstream: Option<Upstream>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentEntry {
    pub title: String,
    /// Offered in the app; the server keeps supporting the others for existing connections.
    pub offered: bool,
    /// Connects one project at a time instead of the user's whole agent configuration.
    pub project: bool,
    /// The vendor whose models the agent gets by default.
    pub native: Option<String>,
    pub catalog: Option<AgentCatalog>,
}

/// One entry per [`Provider`]: a missing or extra key fails the parse, and [`Self::get`]
/// is total.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Providers {
    claude: ProviderEntry,
    codex: ProviderEntry,
    antigravity: ProviderEntry,
    kimi: ProviderEntry,
    xai: ProviderEntry,
    #[serde(rename = "opencode-go")]
    opencode_go: ProviderEntry,
    openai: ProviderEntry,
    anthropic: ProviderEntry,
    gemini: ProviderEntry,
    custom: ProviderEntry,
}

impl Providers {
    #[must_use]
    pub const fn get(&self, provider: Provider) -> &ProviderEntry {
        match provider {
            Provider::Claude => &self.claude,
            Provider::Codex => &self.codex,
            Provider::Antigravity => &self.antigravity,
            Provider::Kimi => &self.kimi,
            Provider::Xai => &self.xai,
            Provider::OpencodeGo => &self.opencode_go,
            Provider::Openai => &self.openai,
            Provider::Anthropic => &self.anthropic,
            Provider::Gemini => &self.gemini,
            Provider::Custom => &self.custom,
        }
    }

    /// Every provider in picker order.
    pub fn iter(&self) -> impl Iterator<Item = (Provider, &ProviderEntry)> {
        Provider::ALL
            .iter()
            .map(|&provider| (provider, self.get(provider)))
    }
}

/// One entry per [`Agent`], like [`Providers`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agents {
    claude: AgentEntry,
    codex: AgentEntry,
    pi: AgentEntry,
    opencode: AgentEntry,
}

impl Agents {
    #[must_use]
    pub const fn get(&self, agent: Agent) -> &AgentEntry {
        match agent {
            Agent::Claude => &self.claude,
            Agent::Codex => &self.codex,
            Agent::Pi => &self.pi,
            Agent::Opencode => &self.opencode,
        }
    }

    /// Every agent in tab order.
    pub fn iter(&self) -> impl Iterator<Item = (Agent, &AgentEntry)> {
        Agent::ALL.iter().map(|&agent| (agent, self.get(agent)))
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};

    use super::*;

    /// An object's keys in source order (`serde_json::Map` sorts them).
    struct Keys(Vec<String>);

    impl<'de> Deserialize<'de> for Keys {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct KeysVisitor;
            impl<'de> Visitor<'de> for KeysVisitor {
                type Value = Keys;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("an object")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Keys, A::Error> {
                    let mut keys = Vec::new();
                    while let Some(key) = map.next_key::<String>()? {
                        map.next_value::<IgnoredAny>()?;
                        keys.push(key);
                    }
                    Ok(Keys(keys))
                }
            }
            deserializer.deserialize_map(KeysVisitor)
        }
    }

    #[derive(Deserialize)]
    struct KeyOrder {
        providers: Keys,
        agents: Keys,
    }

    fn ids<T: Copy>(all: &[T], as_str: fn(T) -> &'static str) -> Vec<&'static str> {
        all.iter().map(|&value| as_str(value)).collect()
    }

    #[test]
    fn the_embedded_registry_parses() {
        let registry = registry().unwrap();
        assert_eq!(registry.limits.name_length, 60);
        assert_eq!(registry.limits.prefix_length, 30);
        assert_eq!(registry.usage_page_size, 500);
        assert_eq!(registry.quota.refresh_ms, 120_000);
        assert_eq!(registry.quota.stale_ms, 300_000);
        let claude = registry.providers.get(Provider::Claude);
        assert!(claude.organizations && claude.reconnect);
        assert_eq!(
            claude.sign_in.as_ref().unwrap().core.as_deref(),
            Some("anthropic")
        );
        let go = registry
            .providers
            .get(Provider::OpencodeGo)
            .preset
            .as_ref()
            .unwrap();
        assert_eq!(go.upstream, Some(Upstream::OpencodeGo));
        assert!(go.fixed_endpoint);
        assert_eq!(
            registry.agents.get(Agent::Codex).catalog,
            Some(AgentCatalog::Header)
        );
    }

    #[test]
    fn enums_match_the_json_both_ways() {
        let registry = registry().unwrap();
        let order: KeyOrder = serde_json::from_str(SOURCE).unwrap();
        assert_eq!(order.providers.0, ids(Provider::ALL, Provider::as_str));
        assert_eq!(order.agents.0, ids(Agent::ALL, Agent::as_str));
        assert_eq!(registry.protocols, Protocol::ALL);
        assert_eq!(registry.strategies, Strategy::ALL);
        assert_eq!(registry.retention_days, RetentionDays::ALL);
        assert_eq!(registry.reset_result_codes, ResetResultCode::ALL);
        let signs_in: Vec<Provider> = registry
            .providers
            .iter()
            .filter(|(_, entry)| entry.sign_in.is_some())
            .map(|(provider, _)| provider)
            .collect();
        let oauth: Vec<Provider> = OAuthProvider::ALL.iter().map(|&p| p.into()).collect();
        assert_eq!(signs_in, oauth);
        for &provider in OAuthProvider::ALL {
            assert_eq!(provider.as_str(), Provider::from(provider).as_str());
        }
    }

    #[test]
    fn enum_strings_are_the_serde_strings() {
        for &provider in Provider::ALL {
            let json = serde_json::to_value(provider).unwrap();
            assert_eq!(json, provider.as_str());
            assert_eq!(Provider::from_id(provider.as_str()), Some(provider));
        }
        assert_eq!(Provider::from_id("openai-compatible-abc"), None);
        assert_eq!(
            serde_json::to_value(Strategy::RoundRobin).unwrap(),
            "round-robin"
        );
        assert_eq!(
            serde_json::to_value(ResetResultCode::AlreadyRedeemed).unwrap(),
            "already_redeemed"
        );
    }

    #[test]
    fn retention_days_are_numbers() {
        assert_eq!(
            serde_json::to_string(&RetentionDays::Days365).unwrap(),
            "365"
        );
        assert_eq!(
            serde_json::from_str::<RetentionDays>("0").unwrap(),
            RetentionDays::All
        );
        assert!(serde_json::from_str::<RetentionDays>("31").is_err());
        assert!(serde_json::from_str::<RetentionDays>("\"30\"").is_err());
    }
}
