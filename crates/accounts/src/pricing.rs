//! Usage history with estimated prices (`src/server/usage-pricing.ts`): per-request USD from the
//! models.dev catalog, each profile's running total for the period, and the euro rate.
//!
//! A price is never guessed: an unknown model, a missing cache price or an incomplete token
//! breakdown is reported as missing (`null`), not as free.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::js;
use nsv_domain::registry::registry;
use nsv_domain::sync::locked;
use nsv_domain::time;
use nsv_domain::wire::{
    Cost, Eur, JsNumber, PricingTokens, UsageHistory, UsagePricing, UsageRecord, UsageSummary,
    UsageTotals,
};
use nsv_store::{Store, StoreError, UsageCursor, UsageRange};
use serde_json::Value;

use crate::catalog::{ModelCatalog, Snapshot};
use crate::fetch::{Outside, Request};

const FX_SOURCE: &str = "https://api.frankfurter.dev/v2/providers/ecb/rate/usd/eur";
const FX_TIMEOUT: Duration = Duration::from_secs(5);
const FX_LIMIT: u64 = 4096;
const FX_RETRY_MS: i64 = 60_000;
const FX_KEEP_MS: i64 = 86_400_000;
/// Past every timestamp: the open end of a period.
const END: &str = "\u{ffff}";

#[derive(Debug, thiserror::Error)]
pub enum PricingError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Activity pricing is shutting down")]
    Closed,
}

/// What the API answers when pricing fails: a store's own error, or a hidden 500.
impl From<PricingError> for AppError {
    fn from(error: PricingError) -> Self {
        match error {
            PricingError::Store(error) => error.into(),
            PricingError::Closed => Self::internal(PricingError::Closed),
        }
    }
}

/// How each provider's legacy counters overlap: which parts the core counted twice.
#[derive(Clone, Copy)]
enum Accounting {
    /// Raw output includes reasoning.
    OutputIncludesReasoning,
    /// Raw input includes cache reads and writes.
    InputIncludesCache,
    Both,
}

fn accounting(provider: &str) -> Option<Accounting> {
    match provider {
        "claude" | "anthropic" => Some(Accounting::OutputIncludesReasoning),
        "gemini" | "antigravity" | "google" => Some(Accounting::InputIncludesCache),
        "openai" | "codex" | "xai" | "kimi" => Some(Accounting::Both),
        _ => None,
    }
}

/// Non-negative safe integers that add up to the record's total.
fn complete(parts: [i128; 5], total: u64) -> bool {
    const SAFE: i128 = 9_007_199_254_740_991;
    parts.iter().all(|part| (0..=SAFE).contains(part))
        && parts.iter().sum::<i128>() == i128::from(total)
}

/// The record's tokens without overlap: the core's `pricingTokens`, or for older rows the
/// legacy counters with each provider's double counting removed. `None` when the breakdown
/// is incomplete, does not add up, or the provider's accounting is unknown.
#[must_use]
pub fn normalized_tokens(record: &UsageRecord) -> Option<PricingTokens> {
    let Some(tokens) = record.pricing_tokens else {
        let mut input = i128::from(record.input_tokens);
        let cached = i128::from(record.cached_tokens);
        let cache_write = i128::from(record.cache_write_tokens);
        let mut output = i128::from(record.output_tokens);
        let reasoning = i128::from(record.reasoning_tokens);
        let accounting = accounting(&record.provider)?;
        if matches!(
            accounting,
            Accounting::InputIncludesCache | Accounting::Both
        ) {
            input -= cached + cache_write;
        }
        if matches!(
            accounting,
            Accounting::OutputIncludesReasoning | Accounting::Both
        ) {
            output -= reasoning;
        }
        let parts = [input, cached, cache_write, output, reasoning];
        if !complete(parts, record.total_tokens) {
            return None;
        }
        let [input, cached, cache_write, output, reasoning] =
            parts.map(|part| u64::try_from(part).unwrap_or(0));
        return Some(PricingTokens {
            input,
            cached,
            cache_write,
            output,
            reasoning,
        });
    };
    let tokens = tokens?;
    let parts = [
        tokens.input,
        tokens.cached,
        tokens.cache_write,
        tokens.output,
        tokens.reasoning,
    ]
    .map(i128::from);
    complete(parts, record.total_tokens).then_some(tokens)
}

/// The prices one request pays: the base rates, or the tier with the largest threshold its
/// context (input, cache reads and writes) exceeds.
struct Rates<'a> {
    input: f64,
    output: f64,
    reasoning: Option<f64>,
    cache_read: Option<&'a JsNumber>,
    cache_write: Option<&'a JsNumber>,
}

/// The request's price in USD; `None` without prices or tokens, when it used a cache the
/// catalog has no price for, or when the result is not finite.
#[must_use]
pub fn estimate_usd(cost: Option<&Cost>, tokens: Option<&PricingTokens>) -> Option<f64> {
    let (cost, tokens) = (cost?, tokens?);
    let context =
        u128::from(tokens.input) + u128::from(tokens.cached) + u128::from(tokens.cache_write);
    let mut rates = Rates {
        input: cost.input.0,
        output: cost.output.0,
        reasoning: cost.reasoning.map(|rate| rate.0),
        cache_read: cost.cache_read.as_ref(),
        cache_write: cost.cache_write.as_ref(),
    };
    let mut threshold = None;
    for tier in cost.tiers.iter().flatten() {
        let size = tier.tier.size;
        if context > u128::from(size) && threshold.is_none_or(|above| size > above) {
            threshold = Some(size);
            rates = Rates {
                input: tier.input.0,
                output: tier.output.0,
                reasoning: tier.reasoning.map(|rate| rate.0),
                cache_read: tier.cache_read.as_ref(),
                cache_write: tier.cache_write.as_ref(),
            };
        }
    }
    if (tokens.cached > 0 && rates.cache_read.is_none())
        || (tokens.cache_write > 0 && rates.cache_write.is_none())
    {
        return None;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "JavaScript multiplied token counts as doubles"
    )]
    let count = |tokens: u64| tokens as f64;
    // The sum in JavaScript's order, so totals match to the last bit.
    let usd = (count(tokens.input) * rates.input
        + count(tokens.output) * rates.output
        + count(tokens.reasoning) * rates.reasoning.unwrap_or(rates.output)
        + count(tokens.cached) * rates.cache_read.map_or(0.0, |rate| rate.0)
        + count(tokens.cache_write) * rates.cache_write.map_or(0.0, |rate| rate.0))
        / 1_000_000.0;
    usd.is_finite().then_some(usd)
}

/// The model a record is priced as: its upstream model, else the model it asked for.
fn priced_model(record: &UsageRecord) -> &str {
    record
        .upstream_model
        .as_deref()
        .filter(|model| !model.is_empty())
        .unwrap_or(&record.model)
}

/// A profile's totals for the period starting at `since`, through store sequence `sequence`.
#[derive(Debug, Clone)]
struct Aggregate {
    since: String,
    sequence: i64,
    generation: u64,
    fetched_at: Option<String>,
    total: UsageTotals,
    summary: UsageSummary,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Add,
    Subtract,
}

impl Direction {
    fn apply(self, value: &mut u64, amount: u64) {
        match self {
            Self::Add => *value += amount,
            Self::Subtract => *value -= amount,
        }
    }
}

/// Each model's prices from one frozen catalog, looked up once per provider and model.
struct Costs<'a> {
    snapshot: Option<&'a Snapshot>,
    cache: HashMap<(String, String), Option<Cost>>,
}

impl<'a> Costs<'a> {
    fn new(snapshot: Option<&'a Snapshot>) -> Self {
        Self {
            snapshot,
            cache: HashMap::new(),
        }
    }

    fn of(&mut self, record: &UsageRecord) -> Option<&Cost> {
        let key = (record.provider.clone(), priced_model(record).to_owned());
        let cost = match self.cache.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let (provider, model) = entry.key();
                let cost = match self.snapshot {
                    Some(snapshot) => snapshot.cost(provider, model),
                    None => None,
                };
                entry.insert(cost)
            }
        };
        cost.as_ref()
    }
}

/// Adds or removes records from a running total, pricing each with one frozen catalog.
struct Tally<'a> {
    costs: Costs<'a>,
    total: UsageTotals,
    summary: UsageSummary,
}

impl Tally<'_> {
    fn include(&mut self, record: &UsageRecord, direction: Direction) {
        let tokens = normalized_tokens(record);
        let usd = estimate_usd(self.costs.of(record), tokens.as_ref());
        let summary = &mut self.summary;
        direction.apply(&mut summary.requests, 1);
        direction.apply(&mut summary.failed, u64::from(record.failed));
        direction.apply(&mut summary.input_tokens, record.input_tokens);
        direction.apply(&mut summary.output_tokens, record.output_tokens);
        direction.apply(&mut summary.cached_tokens, record.cached_tokens);
        direction.apply(&mut summary.reasoning_tokens, record.reasoning_tokens);
        direction.apply(&mut summary.total_tokens, record.total_tokens);
        let total = &mut self.total;
        direction.apply(&mut total.requests, 1);
        if let Some(usd) = usd {
            direction.apply(&mut total.priced_requests, 1);
            total.usd.0 += if direction == Direction::Add {
                usd
            } else {
                -usd
            };
        }
        if let Some(tokens) = tokens {
            direction.apply(&mut total.normalized_requests, 1);
            direction.apply(&mut total.tokens.input, tokens.input);
            direction.apply(&mut total.tokens.cached, tokens.cached);
            direction.apply(&mut total.tokens.cache_write, tokens.cache_write);
            direction.apply(&mut total.tokens.output, tokens.output);
            direction.apply(&mut total.tokens.reasoning, tokens.reasoning);
        }
    }
}

#[derive(Debug, Default)]
struct Euro {
    eur: Option<Eur>,
    next_refresh: i64,
    pending: bool,
}

/// Activity's prices: one bounded aggregate per profile, never its history rows.
pub struct UsagePrices {
    fetch: Arc<dyn Outside>,
    /// Milliseconds since the epoch.
    clock: fn() -> i64,
    totals: Mutex<HashMap<ProfileId, Aggregate>>,
    /// Serializes history calls per profile.
    histories: Mutex<HashMap<ProfileId, Arc<tokio::sync::Mutex<()>>>>,
    closed: AtomicBool,
    euro: Mutex<Euro>,
}

impl UsagePrices {
    #[must_use]
    pub fn new(fetch: Arc<dyn Outside>, clock: fn() -> i64) -> Self {
        Self {
            fetch,
            clock,
            totals: Mutex::new(HashMap::new()),
            histories: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            euro: Mutex::new(Euro::default()),
        }
    }

    /// Each record's price against the catalog now, without waiting for the network: the
    /// catalog and the euro rate refresh in the background (the rate at most once a minute
    /// after a failure, once a day after a success).
    pub fn snapshot(
        self: &Arc<Self>,
        records: &[UsageRecord],
        catalog: &Arc<ModelCatalog>,
    ) -> UsagePricing {
        let snapshot = (!records.is_empty()).then(|| catalog.snapshot());
        self.pricing(records, snapshot.as_ref())
    }

    fn pricing(
        self: &Arc<Self>,
        records: &[UsageRecord],
        snapshot: Option<&Snapshot>,
    ) -> UsagePricing {
        let eur = {
            let mut euro = locked(&self.euro);
            let now = (self.clock)();
            if !records.is_empty() && !euro.pending && now >= euro.next_refresh {
                euro.next_refresh = now + FX_RETRY_MS;
                euro.pending = true;
                let prices = Arc::clone(self);
                tokio::spawn(async move { prices.refresh_euro().await });
            }
            euro.eur.clone()
        };
        let mut costs = Costs::new(snapshot);
        let mut usd = BTreeMap::new();
        for record in records {
            let price = estimate_usd(costs.of(record), normalized_tokens(record).as_ref());
            usd.insert(record.id.clone(), price.map(JsNumber));
        }
        UsagePricing {
            totals: None,
            usd,
            fetched_at: snapshot.and_then(Snapshot::fetched_at).map(str::to_owned),
            refreshing: snapshot.is_some_and(|snapshot| snapshot.refreshing),
            eur,
        }
    }

    async fn refresh_euro(&self) {
        let rate = self.euro_rate().await;
        let mut euro = locked(&self.euro);
        euro.pending = false;
        // A failure keeps the last rate and retries after the minute set when it started.
        if let Some(rate) = rate {
            euro.eur = Some(rate);
            euro.next_refresh = (self.clock)() + FX_KEEP_MS;
        }
    }

    async fn euro_rate(&self) -> Option<Eur> {
        let response = self
            .fetch
            .https(Request {
                url: FX_SOURCE.to_owned(),
                headers: Vec::new(),
                timeout: FX_TIMEOUT,
                limit: FX_LIMIT,
            })
            .await
            .ok()
            .filter(nsv_net::Response::ok)?;
        let body: Value = serde_json::from_str(&String::from_utf8_lossy(&response.body)).ok()?;
        let body = Some(&body);
        let rate = js::field(body, "rate")?.as_f64()?;
        let date = js::text(js::field(body, "date"))?;
        (js::text(js::field(body, "base")) == Some("USD")
            && js::text(js::field(body, "quote")) == Some("EUR")
            && time::is_iso_date(date)
            && rate.is_finite()
            && rate > 0.0)
            .then(|| Eur {
                rate: JsNumber(rate),
                date: date.to_owned(),
            })
    }

    /// The profile's newest 500 records since `since`, their prices, and the totals of the
    /// whole period. Callers for one profile run one at a time; each keeps records, summary and
    /// prices from one store snapshot. The total is kept and updated incrementally: an
    /// unchanged refresh reads no rows, new rows are added, and a later `since` subtracts the
    /// rows it leaves out. It is rebuilt when the period starts earlier, retention deleted rows
    /// or the catalog changed.
    ///
    /// # Errors
    /// A store failure, or [`PricingError::Closed`] once [`UsagePrices::close`] began.
    pub async fn history(
        self: &Arc<Self>,
        store: &Store,
        profile: &ProfileId,
        since: &str,
        catalog: &Arc<ModelCatalog>,
    ) -> Result<UsageHistory, PricingError> {
        let turn = Arc::clone(locked(&self.histories).entry(profile.clone()).or_default());
        let result = {
            let _held = turn.lock().await;
            if self.closed.load(Ordering::Acquire) {
                Err(PricingError::Closed)
            } else {
                self.summarize(store, profile, since, catalog).await
            }
        };
        let mut histories = locked(&self.histories);
        // Only the map and this call hold it: nobody is waiting behind.
        if Arc::strong_count(&turn) == 2 {
            histories.remove(profile);
        }
        result
    }

    async fn summarize(
        self: &Arc<Self>,
        store: &Store,
        profile: &ProfileId,
        since: &str,
        catalog: &Arc<ModelCatalog>,
    ) -> Result<UsageHistory, PricingError> {
        loop {
            // One store snapshot before yielding: the records and where the total stops.
            let (records, generation, through) = {
                let profile = profile.clone();
                let since = since.to_owned();
                store
                    .call(move |db| {
                        let page = registry().usage_page_size;
                        Ok((
                            db.usage(&profile, &since, page)?,
                            db.usage_generation(),
                            db.usage_sequence()?,
                        ))
                    })
                    .await?
            };
            // Rates stay frozen for the whole calculation, even if a refresh lands meanwhile.
            let snapshot = (!records.is_empty()).then(|| catalog.snapshot());
            let pricing = self.pricing(&records, snapshot.as_ref());
            let fetched_at = snapshot
                .as_ref()
                .and_then(Snapshot::fetched_at)
                .map(str::to_owned);
            let previous = locked(&self.totals).get(profile).cloned().filter(|cached| {
                cached.generation == generation
                    && cached.fetched_at == fetched_at
                    && since >= cached.since.as_str()
            });
            let mut tally = Tally {
                costs: Costs::new(snapshot.as_ref()),
                total: previous
                    .as_ref()
                    .map_or_else(empty_totals, |cached| cached.total.clone()),
                summary: previous
                    .as_ref()
                    .map_or_else(UsageSummary::default, |cached| cached.summary),
            };
            if let Some(previous) = previous
                .as_ref()
                .filter(|previous| since > previous.since.as_str())
            {
                let left_out = Read::Between {
                    since: previous.since.clone(),
                    before: since.to_owned(),
                    through: previous.sequence,
                    from: None,
                };
                self.walk(store, profile, left_out, |record| {
                    tally.include(record, Direction::Subtract);
                })
                .await?;
            }
            let added = match &previous {
                Some(previous) => Read::After {
                    after: previous.sequence,
                    through,
                },
                None => Read::Between {
                    since: since.to_owned(),
                    before: END.to_owned(),
                    through,
                    from: None,
                },
            };
            let last_generation = self
                .walk(store, profile, added, |record| {
                    if record.timestamp.as_str() >= since {
                        tally.include(record, Direction::Add);
                    }
                })
                .await?;
            // Retention may have run while yielding; never publish a total with deleted rows.
            if last_generation != generation {
                continue;
            }
            let Tally {
                mut total, summary, ..
            } = tally;
            total.usd.0 = total.usd.0.max(0.0);
            locked(&self.totals).insert(
                profile.clone(),
                Aggregate {
                    since: since.to_owned(),
                    sequence: through,
                    generation,
                    fetched_at,
                    total: total.clone(),
                    summary,
                },
            );
            return Ok(UsageHistory {
                records,
                summary,
                pricing: UsagePricing {
                    totals: Some(total),
                    ..pricing
                },
            });
        }
    }

    /// Hands every row `read` selects to `each`, a page at a time with a yield between
    /// pages, and answers the retention generation the last page saw.
    async fn walk(
        &self,
        store: &Store,
        profile: &ProfileId,
        read: Read,
        mut each: impl FnMut(&UsageRecord) + Send,
    ) -> Result<u64, PricingError> {
        let mut read = read;
        loop {
            let (records, next, generation) = self.page(store, profile, read).await?;
            for record in &records {
                each(record);
            }
            let Some(next) = next else {
                return Ok(generation);
            };
            read = next;
            tokio::task::yield_now().await;
        }
    }

    /// One page of rows, the read that continues it, and the retention generation, in one
    /// store section.
    async fn page(
        &self,
        store: &Store,
        profile: &ProfileId,
        read: Read,
    ) -> Result<(Vec<UsageRecord>, Option<Read>, u64), PricingError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PricingError::Closed);
        }
        let profile = profile.clone();
        Ok(store
            .call(move |db| {
                let (records, next) = match read {
                    Read::After { after, through } => {
                        let page = db.usage_after(&profile, after, through)?;
                        let next = page.next.map(|after| Read::After { after, through });
                        (page.records, next)
                    }
                    Read::Between {
                        since,
                        before,
                        through,
                        from,
                    } => {
                        let range = UsageRange {
                            since: &since,
                            before: &before,
                            through,
                        };
                        let page = db.usage_between(&profile, range, from.as_ref())?;
                        let next = page.next.map(|cursor| Read::Between {
                            since: since.clone(),
                            before: before.clone(),
                            through,
                            from: Some(cursor),
                        });
                        (page.records, next)
                    }
                };
                Ok((records, next, db.usage_generation()))
            })
            .await?)
    }

    /// Refuses new history calls, waits for running ones, and drops every total.
    pub async fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let running: Vec<_> = locked(&self.histories).values().cloned().collect();
        for turn in running {
            drop(turn.lock().await);
        }
        locked(&self.totals).clear();
    }
}

fn empty_totals() -> UsageTotals {
    UsageTotals {
        requests: 0,
        priced_requests: 0,
        usd: JsNumber(0.0),
        normalized_requests: 0,
        tokens: PricingTokens::default(),
    }
}

/// Which rows a page reads: stored after a sequence, or in a time range.
#[derive(Debug)]
enum Read {
    After {
        after: i64,
        through: i64,
    },
    Between {
        since: String,
        before: String,
        through: i64,
        from: Option<UsageCursor>,
    },
}
