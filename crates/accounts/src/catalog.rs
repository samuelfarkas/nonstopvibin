//! The models.dev catalog: limits and prices matched to a profile's models
//! (`src/server/model-catalog.ts`). It is fetched without credentials, kept an hour, and one
//! fetch serves every concurrent caller.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::registry::{Provider, RegistryError, registry};
use nsv_domain::time::iso_string;
use nsv_domain::wire::{
    ApiAccount, CatalogModel, ContextTier, Cost, CostTier, JsNumber, Modality, Model,
    ModelMetadata, TierType,
};
use nsv_store::{Store, StoreError};
use serde_json::{Map, Value};
use tokio::sync::OnceCell;

use crate::fetch::{Fetch, Request};
use crate::js;

const SOURCE: &str = "https://models.dev/api.json";
const TIMEOUT: Duration = Duration::from_secs(10);
const LIMIT: u64 = 16_000_000;
const FRESH_MS: i64 = 3_600_000;
/// After a background refresh starts, Activity starts no other for this long.
const ACTIVITY_BACKOFF_MS: i64 = 60_000;
const UNAVAILABLE: &str = "Could not refresh model metadata from models.dev. Try again before connecting or restarting the agent.";

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(transparent)]
    App(#[from] AppError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

/// A model definition as the catalog keeps it: only what agents and pricing read, and only
/// models with text in and text out (others never match).
#[derive(Debug, Clone, PartialEq)]
struct Definition {
    name: String,
    context: u64,
    output: u64,
    input_limit: Option<u64>,
    input: Vec<Modality>,
    reasoning: bool,
    tool_call: bool,
    cost: Option<Cost>,
}

/// models.dev's providers and their valid models.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog(HashMap<String, HashMap<String, Definition>>);

impl Catalog {
    /// Reads the payload. Each provider must be an object with a `models` object; a model that
    /// does not match the schema is dropped.
    ///
    /// # Errors
    /// When the payload is not JSON or a provider is malformed.
    pub fn parse(body: &[u8]) -> Result<Self, AppError> {
        let unavailable = || AppError::new(UNAVAILABLE, 502);
        let payload: Value =
            serde_json::from_str(&String::from_utf8_lossy(body)).map_err(|_| unavailable())?;
        let providers = payload.as_object().ok_or_else(unavailable)?;
        let mut catalog = HashMap::with_capacity(providers.len());
        for (provider, entry) in providers {
            let models = js::field(Some(entry), "models")
                .and_then(Value::as_object)
                .ok_or_else(unavailable)?;
            let models = models
                .iter()
                .filter_map(|(id, value)| Some((id.clone(), definition(value)?)))
                .collect();
            catalog.insert(provider.clone(), models);
        }
        Ok(Self(catalog))
    }

    fn get(&self, provider: &str, id: &str) -> Option<&Definition> {
        self.0.get(provider)?.get(id)
    }
}

/// zod's `definition` schema of a models.dev model, then the text-in, text-out rule.
fn definition(value: &Value) -> Option<Definition> {
    let object = value.as_object()?;
    let name = object.get("name")?.as_str()?;
    if !(1..=500).contains(&js::utf16_len(name)) {
        return None;
    }
    let limit = object.get("limit")?.as_object()?;
    let modalities = object.get("modalities")?.as_object()?;
    let strings = |key: &str| -> Option<Vec<&str>> {
        modalities
            .get(key)?
            .as_array()?
            .iter()
            .map(Value::as_str)
            .collect()
    };
    let (input, output) = (strings("input")?, strings("output")?);
    let reasoning = object.get("reasoning")?.as_bool()?;
    let tool_call = object.get("tool_call")?.as_bool()?;
    let definition = Definition {
        name: name.to_owned(),
        context: count(limit.get("context")?)?,
        output: count(limit.get("output")?)?,
        input_limit: optional(limit, "input", count)?,
        input: input
            .iter()
            .filter_map(|modality| match *modality {
                "text" => Some(Modality::Text),
                "image" => Some(Modality::Image),
                _ => None,
            })
            .collect(),
        reasoning,
        tool_call,
        cost: optional(object, "cost", cost)?,
    };
    (input.contains(&"text") && output.contains(&"text")).then_some(definition)
}

/// An optional field: absent is `Some(None)`, present must read, `null` included.
#[expect(
    clippy::option_option,
    reason = "invalid, absent and present are three outcomes"
)]
fn optional<T>(
    object: &Map<String, Value>,
    key: &str,
    read: impl Fn(&Value) -> Option<T>,
) -> Option<Option<T>> {
    match object.get(key) {
        None => Some(None),
        Some(value) => read(value).map(Some),
    }
}

/// A positive safe integer.
fn count(value: &Value) -> Option<u64> {
    const SAFE: f64 = 9_007_199_254_740_991.0;
    let number = value.as_f64()?;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a positive whole number up to 2^53 - 1"
    )]
    (number > 0.0 && number.fract() == 0.0 && number <= SAFE).then_some(number as u64)
}

/// A finite, non-negative price in USD per million tokens.
fn price(value: &Value) -> Option<JsNumber> {
    let number = value.as_f64()?;
    (number.is_finite() && number >= 0.0).then_some(JsNumber(number))
}

fn cost(value: &Value) -> Option<Cost> {
    let object = value.as_object()?;
    let tiers = optional(object, "tiers", |tiers| {
        tiers
            .as_array()?
            .iter()
            .map(tier)
            .collect::<Option<Vec<_>>>()
    })?;
    Some(Cost {
        input: price(object.get("input")?)?,
        output: price(object.get("output")?)?,
        reasoning: optional(object, "reasoning", price)?,
        cache_read: optional(object, "cache_read", price)?,
        cache_write: optional(object, "cache_write", price)?,
        tiers,
    })
}

fn tier(value: &Value) -> Option<CostTier> {
    let object = value.as_object()?;
    let threshold = object.get("tier")?.as_object()?;
    if threshold.get("type")?.as_str()? != "context" {
        return None;
    }
    Some(CostTier {
        input: price(object.get("input")?)?,
        output: price(object.get("output")?)?,
        reasoning: optional(object, "reasoning", price)?,
        cache_read: optional(object, "cache_read", price)?,
        cache_write: optional(object, "cache_write", price)?,
        tier: ContextTier {
            kind: TierType::Context,
            size: count(threshold.get("size")?)?,
        },
    })
}

/// The catalog id of a routing provider: the registry's mapping, else the id itself.
fn catalog_provider(provider: &str) -> Result<String, RegistryError> {
    let registry = registry()?;
    Ok(Provider::from_id(provider)
        .and_then(|known| registry.providers.get(known).catalog.clone())
        .unwrap_or_else(|| provider.to_owned()))
}

/// Matches each model to exactly one catalog entry through the routing provider and exact
/// upstream id: an enabled API account's (prefixed) model, else the model's owner. Two routes
/// that disagree, or none, leave the model plain; a same-named model of another vendor is
/// never borrowed and versions are never guessed from a prefix.
///
/// # Errors
/// Only when the embedded registry is broken.
pub fn enrich_models(
    models: &[Model],
    accounts: &[ApiAccount],
    catalog: &Catalog,
    fetched_at: &str,
) -> Result<Vec<CatalogModel>, RegistryError> {
    models
        .iter()
        .map(|model| {
            let mut routes = HashSet::new();
            for account in accounts.iter().filter(|account| !account.disabled) {
                for upstream in &account.models {
                    let matches = if account.prefix.is_empty() {
                        model.id == upstream.id
                    } else {
                        model
                            .id
                            .strip_prefix(account.prefix.as_str())
                            .and_then(|rest| rest.strip_prefix('/'))
                            == Some(upstream.id.as_str())
                    };
                    if matches {
                        routes.insert((
                            catalog_provider(account.provider.as_str())?,
                            upstream.id.as_str(),
                        ));
                    }
                }
            }
            if let (true, Some(owner)) = (routes.is_empty(), &model.owned_by) {
                routes.insert((catalog_provider(owner)?, model.id.as_str()));
            }
            let plain = CatalogModel {
                id: model.id.clone(),
                owned_by: model.owned_by.clone(),
                metadata: None,
            };
            let mut routes = routes.into_iter();
            let (Some((provider, id)), None) = (routes.next(), routes.next()) else {
                return Ok(plain);
            };
            let Some(data) = catalog.get(&provider, id) else {
                return Ok(plain);
            };
            Ok(CatalogModel {
                metadata: Some(ModelMetadata {
                    name: data.name.clone(),
                    context: data.context,
                    output: data.output,
                    input_limit: data.input_limit,
                    input: data.input.clone(),
                    reasoning: data.reasoning,
                    tool_call: data.tool_call,
                    cost: data.cost.clone(),
                    source: format!(
                        "{SOURCE}#{}/{}",
                        encode_uri_component(&provider),
                        encode_uri_component(id)
                    ),
                    fetched_at: fetched_at.to_owned(),
                }),
                ..plain
            })
        })
        .collect()
}

/// `encodeURIComponent`.
fn encode_uri_component(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// A fetched catalog and when it arrived.
#[derive(Debug)]
pub struct Cached {
    catalog: Catalog,
    fetched_at: String,
    at: i64,
}

type Load = Arc<OnceCell<Result<Arc<Cached>, AppError>>>;

#[derive(Debug, Default)]
struct State {
    cached: Option<Arc<Cached>>,
    pending: Option<Load>,
    next_activity_refresh: i64,
}

/// The catalog as of one moment; pricing freezes it for a whole calculation.
#[derive(Debug, Clone)]
pub struct Snapshot {
    cached: Option<Arc<Cached>>,
    pub refreshing: bool,
}

impl Snapshot {
    /// When the catalog was fetched, as `toISOString()`; `None` before the first fetch.
    #[must_use]
    pub fn fetched_at(&self) -> Option<&str> {
        self.cached
            .as_deref()
            .map(|cached| cached.fetched_at.as_str())
    }

    /// The models with metadata; unchanged before the first fetch.
    ///
    /// # Errors
    /// Only when the embedded registry is broken.
    pub fn enrich(&self, models: &[Model]) -> Result<Vec<CatalogModel>, RegistryError> {
        match &self.cached {
            Some(cached) => enrich_models(models, &[], &cached.catalog, &cached.fetched_at),
            None => Ok(models
                .iter()
                .map(|model| CatalogModel {
                    id: model.id.clone(),
                    owned_by: model.owned_by.clone(),
                    metadata: None,
                })
                .collect()),
        }
    }

    /// The prices of `id` routed through `provider`.
    ///
    /// # Errors
    /// Only when the embedded registry is broken.
    pub fn cost(&self, provider: &str, id: &str) -> Result<Option<Cost>, RegistryError> {
        let model = Model {
            id: id.to_owned(),
            owned_by: Some(provider.to_owned()),
        };
        Ok(self
            .enrich(&[model])?
            .pop()
            .and_then(|model| model.metadata?.cost))
    }
}

/// The application's models.dev catalog.
#[derive(Debug)]
pub struct ModelCatalog<F> {
    fetch: F,
    /// Milliseconds since the epoch.
    clock: fn() -> i64,
    state: Mutex<State>,
}

impl<F: Fetch + 'static> ModelCatalog<F> {
    #[must_use]
    pub fn new(fetch: F, clock: fn() -> i64) -> Self {
        Self {
            fetch,
            clock,
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The fresh cached catalog, or the fetch every concurrent caller shares.
    fn begin(&self, state: &mut State) -> Result<Arc<Cached>, Load> {
        match &state.cached {
            Some(cached) if (self.clock)() - cached.at < FRESH_MS => Ok(Arc::clone(cached)),
            _ => Err(Arc::clone(state.pending.get_or_insert_with(Load::default))),
        }
    }

    async fn finish(&self, load: &Load) -> Result<Arc<Cached>, AppError> {
        let result = load.get_or_init(|| self.download()).await.clone();
        let mut state = self.state();
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| Arc::ptr_eq(pending, load))
        {
            state.pending = None;
        }
        result
    }

    async fn download(&self) -> Result<Arc<Cached>, AppError> {
        let response = self
            .fetch
            .https(Request {
                url: SOURCE.to_owned(),
                headers: Vec::new(),
                timeout: TIMEOUT,
                limit: LIMIT,
            })
            .await
            .map_err(|_| AppError::new(UNAVAILABLE, 502))?;
        if !response.ok() {
            return Err(AppError::new(UNAVAILABLE, 502));
        }
        let catalog = Catalog::parse(&response.body)?;
        let at = (self.clock)();
        let cached = Arc::new(Cached {
            catalog,
            fetched_at: iso_string(at),
            at,
        });
        self.state().cached = Some(Arc::clone(&cached));
        Ok(cached)
    }

    async fn load(&self) -> Result<Arc<Cached>, AppError> {
        let begun = self.begin(&mut self.state());
        match begun {
            Ok(cached) => Ok(cached),
            Err(load) => self.finish(&load).await,
        }
    }

    /// `models` with metadata matched through `accounts`, waiting for a fetch when the cache is
    /// empty or an hour old.
    ///
    /// # Errors
    /// 502 when models.dev cannot be fetched or read.
    pub async fn models(
        &self,
        models: &[Model],
        accounts: &[ApiAccount],
    ) -> Result<Vec<CatalogModel>, CatalogError> {
        let cached = self.load().await?;
        Ok(enrich_models(
            models,
            accounts,
            &cached.catalog,
            &cached.fetched_at,
        )?)
    }

    /// The catalog now, never waiting for the network: a missing or stale catalog starts a
    /// background fetch, and a failed one is not retried for a minute.
    #[must_use]
    pub fn snapshot(self: &Arc<Self>) -> Snapshot {
        let now = (self.clock)();
        let mut state = self.state();
        let expired = state
            .cached
            .as_ref()
            .is_none_or(|cached| now - cached.at >= FRESH_MS);
        if expired && now >= state.next_activity_refresh && state.pending.is_none() {
            state.next_activity_refresh = now + ACTIVITY_BACKOFF_MS;
            if let Err(load) = self.begin(&mut state) {
                let catalog = Arc::clone(self);
                // Activity reads the outcome from the next snapshot; a failure only backs off.
                tokio::spawn(async move { drop(catalog.finish(&load).await) });
            }
        }
        Snapshot {
            cached: state.cached.clone(),
            refreshing: state
                .pending
                .as_ref()
                .is_some_and(|load| !load.initialized()),
        }
    }
}

/// A profile's models as its core lists them, with metadata matched through its API accounts.
///
/// # Errors
/// A store failure, or 502 when models.dev cannot be fetched.
pub async fn profile_models<F: Fetch + 'static>(
    catalog: &ModelCatalog<F>,
    store: &Store,
    profile: &ProfileId,
    models: &[Model],
) -> Result<Vec<CatalogModel>, CatalogError> {
    let profile = profile.clone();
    let accounts = store.call(move |db| db.api_accounts(&profile)).await?;
    catalog.models(models, &accounts).await
}
