//! The provider, agent and vocabulary data the server shares with the renderer.
//!
//! `src/shared/providers.ts`, `agents.ts` and `vocabulary.ts` stay the source.
//! `scripts/export-registry.ts` writes the fields Rust reads to `data/registry.json`, which this
//! module embeds and parses once, and fails when relations it cannot type disagree. Every
//! registry type denies unknown fields, so an exported field Rust does not model fails the parse
//! test. The closed enums below are hand-written; the provider and agent tables must list
//! exactly their keys, in their order, and the generated TypeScript twins tie their values.

use std::fmt;
use std::marker::PhantomData;
use std::sync::LazyLock;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};

use crate::sha256_hex;

const SOURCE: &str = include_str!("../data/registry.json");

#[expect(
    clippy::expect_used,
    reason = "registry.json is embedded at build time: a mismatch is a build defect that the unit test the_embedded_registry_parses fails on, never input"
)]
static REGISTRY: LazyLock<Registry> = LazyLock::new(|| {
    serde_json::from_str(SOURCE).expect("the embedded registry.json matches the registry types")
});

/// The registry, parsed on first use.
#[must_use]
pub fn registry() -> &'static Registry {
    &REGISTRY
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Registry {
    pub providers: Table<Provider, ProviderEntry>,
    pub agents: Table<Agent, AgentEntry>,
    pub limits: Limits,
    /// Usage records per page: the core's drain, history pages and the Activity sample.
    pub usage_page_size: u32,
    /// How often the server refreshes every account's quota while a UI watches.
    pub quota_refresh_ms: u64,
    /// How often the UI polls the state.
    pub poll_ms: u64,
}

/// What the core calls the providers it generates for compatible endpoints.
const COMPATIBLE: &str = "openai-compatible-";

impl Registry {
    /// `providerLabel`: a known provider's label, `Compatible API` for a provider the core
    /// generated for a compatible endpoint, else the id itself.
    #[must_use]
    pub fn label(&self, provider: &str) -> String {
        match Provider::from_id(provider) {
            Some(known) => self.providers.get(known).label.clone(),
            None if provider.starts_with(COMPATIBLE) => "Compatible API".to_owned(),
            None => provider.to_owned(),
        }
    }

    /// The agent whose vendor's models these are: `owned_by` is the vendor for OAuth models
    /// and [`Provider::compatible`] for API-key accounts registered as compatible providers.
    #[must_use]
    pub fn native_agent(&self, owner: Option<&str>) -> Option<Agent> {
        let owner = owner?;
        self.providers
            .iter()
            .find(|(provider, _)| {
                let id = provider.as_str();
                owner == id
                    || owner
                        .strip_prefix(id)
                        .is_some_and(|rest| rest.starts_with('-'))
            })
            .and_then(|(_, entry)| entry.native_agent)
    }

    /// Whether `agent` is offered its vendor's curated catalog: it has a native vendor.
    /// Without one, it gets every model with models.dev pricing.
    #[must_use]
    pub fn curated(&self, agent: Agent) -> bool {
        self.agents.get(agent).native.is_some()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    /// Profile and account names, in UTF-16 code units ([`crate::js::utf16_len`]).
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

/// One entry per key of `K`, in `K::ALL` order: a missing, extra or misplaced key fails the
/// parse, and [`Self::get`] is total.
#[derive(Debug)]
pub struct Table<K, V> {
    entries: Box<[V]>,
    key: PhantomData<K>,
}

impl<K: Key, V> Table<K, V> {
    #[must_use]
    pub fn get(&self, key: K) -> &V {
        #[expect(
            clippy::indexing_slicing,
            reason = "the parse holds one entry per key, at the key's index in K::ALL"
        )]
        &self.entries[key.index()]
    }

    /// Every entry in `K::ALL` order.
    pub fn iter(&self) -> impl Iterator<Item = (K, &V)> {
        K::ALL.iter().copied().zip(self.entries.iter())
    }
}

impl<'de, K: Key, V: Deserialize<'de>> Deserialize<'de> for Table<K, V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Entries<K, V>(PhantomData<(K, V)>);

        impl<'de, K: Key, V: Deserialize<'de>> Visitor<'de> for Entries<K, V> {
            type Value = Table<K, V>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object with one entry per key, in order")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::with_capacity(K::ALL.len());
                while let Some(key) = map.next_key::<String>()? {
                    let expected = K::ALL.get(entries.len()).map(|expected| expected.as_str());
                    if expected != Some(key.as_str()) {
                        return Err(de::Error::custom(format_args!(
                            "expected {expected:?} at position {}, found {key:?}",
                            entries.len()
                        )));
                    }
                    entries.push(map.next_value()?);
                }
                if let Some(missing) = K::ALL.get(entries.len()) {
                    return Err(de::Error::custom(format_args!(
                        "missing {:?}",
                        missing.as_str()
                    )));
                }
                Ok(Table {
                    entries: entries.into_boxed_slice(),
                    key: PhantomData,
                })
            }
        }

        deserializer.deserialize_map(Entries(PhantomData))
    }
}

/// A closed enum: its values in order, each with its wire string.
pub trait Key: Copy + 'static {
    const ALL: &'static [Self];
    fn as_str(self) -> &'static str;
    /// The position in [`Key::ALL`].
    fn index(self) -> usize;
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

        impl Key for $name {
            const ALL: &'static [Self] = $name::ALL;

            fn as_str(self) -> &'static str {
                $name::as_str(self)
            }

            fn index(self) -> usize {
                self as usize
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

impl Provider {
    /// The core's name for an API-key account's compatible endpoint: the provider and the
    /// SHA-256 of what identifies the endpoint, which [`Registry::native_agent`] reads back.
    #[must_use]
    pub fn compatible(self, identity: &str) -> String {
        format!("{}-{}", self.as_str(), sha256_hex(identity.as_bytes()))
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

/// Browser sign-in through the core.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignIn {
    /// The core's name for the provider, where it differs.
    pub core: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Preset {
    pub name: String,
    pub base_url: String,
    /// Only the official endpoint is accepted.
    #[serde(default)]
    pub fixed_endpoint: bool,
    /// The provider whose own server rules (headers, model protocols) its accounts follow.
    pub upstream: Option<Provider>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentEntry {
    pub title: String,
    /// Connects one project at a time instead of the user's whole agent configuration.
    pub project: bool,
    /// The vendor whose models the agent gets by default; the export checks that exactly
    /// the providers whose `native_agent` is this agent name one.
    pub native: Option<String>,
    pub catalog: Option<AgentCatalog>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_registry_parses() {
        let registry = registry();
        assert_eq!(registry.limits.name_length, 60);
        assert_eq!(registry.limits.prefix_length, 30);
        assert_eq!(registry.usage_page_size, 500);
        assert_eq!(registry.quota_refresh_ms, 120_000);
        assert_eq!(registry.poll_ms, 4000);
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
        assert_eq!(go.upstream, Some(Provider::OpencodeGo));
        assert!(go.fixed_endpoint);
        assert_eq!(
            registry.agents.get(Agent::Codex).catalog,
            Some(AgentCatalog::Header)
        );
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
    fn tables_hold_every_key_in_order() {
        let table: Table<Strategy, u8> =
            serde_json::from_str(r#"{"round-robin":1,"fill-first":2}"#).unwrap();
        assert_eq!(*table.get(Strategy::FillFirst), 2);
        for wrong in [
            r#"{"fill-first":2,"round-robin":1}"#,
            r#"{"round-robin":1}"#,
            r#"{"round-robin":1,"fill-first":2,"extra":3}"#,
        ] {
            assert!(
                serde_json::from_str::<Table<Strategy, u8>>(wrong).is_err(),
                "{wrong}"
            );
        }
    }

    #[test]
    fn labels_and_native_agents_follow_provider_names() {
        let registry = registry();
        assert_eq!(registry.label("codex"), "Codex");
        assert_eq!(registry.label("openai-compatible-abc"), "Compatible API");
        assert_eq!(registry.label("unknown"), "unknown");
        let compatible = Provider::Anthropic.compatible("x");
        assert_eq!(
            compatible,
            "anthropic-2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
        );
        assert_eq!(
            registry.native_agent(Some(&compatible)),
            Some(Agent::Claude)
        );
        assert_eq!(registry.native_agent(Some("openai")), Some(Agent::Codex));
        assert_eq!(registry.native_agent(Some("openaix")), None);
        assert!(registry.curated(Agent::Claude) && registry.curated(Agent::Codex));
        assert!(!registry.curated(Agent::Pi) && !registry.curated(Agent::Opencode));
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
