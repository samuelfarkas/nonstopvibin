//! Server-side behaviours of `tests/model-catalog.test.ts`, `usage-pricing.test.ts`,
//! `quota.test.ts` and `reset-credits.test.ts` beyond the goldens.

use std::future::{Future, ready};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use nsv_accounts::api_accounts::api_account;
use nsv_accounts::catalog::{CatalogError, ModelCatalog};
use nsv_accounts::pricing::{PricingError, UsagePrices, estimate_usd, normalized_tokens};
use nsv_accounts::quota::{QuotaError, Quotas, RefreshQuota, parse_quota};
use nsv_accounts::resets::Resets;
use nsv_core::upstream::CLAUDE_CODE_USER_AGENT;
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::registry::{Protocol, RetentionDays};
use nsv_domain::secret::ApiKey;
use nsv_domain::wire::{
    Account, ApiAccount, ApiAccountInput, Cost, Model, PricingTokens, Quota, QuotaStatus,
    ResetResult, UsageRecord,
};
use nsv_net::Loopback;
use nsv_store::Store;
use nsv_testkit::TempDir;
use nsv_testkit::fake::response;
use nsv_testkit::world::{StubFetch, open_store, profile};
use serde_json::{Value, json};

use super::common::{NOW, NOW_ISO, StubSource, fixed};

/// Waits until the stub core has received `calls` calls in all.
async fn called(source: &StubSource, calls: usize) {
    while source.calls.lock().unwrap().len() < calls {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

fn model(id: &str) -> Model {
    Model {
        id: id.to_owned(),
        owned_by: Some("claude".to_owned()),
    }
}

/// models.dev with one Anthropic model `m` at `input` USD per million input tokens.
fn priced_catalog(input: f64) -> Vec<u8> {
    json!({ "anthropic": { "models": { "m": {
        "name": "M",
        "limit": { "context": 1000, "output": 100 },
        "modalities": { "input": ["text"], "output": ["text"] },
        "reasoning": false,
        "tool_call": true,
        "cost": { "input": input, "output": 0 },
    } } } })
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn the_catalog_fetch_is_credential_free_bounded_cached_coalesced_and_retried() {
    static CLOCK: AtomicI64 = AtomicI64::new(NOW);
    fn now() -> i64 {
        CLOCK.load(Ordering::SeqCst)
    }
    let (fetch, gate) = StubFetch::gated(|_| Ok(response(200, priced_catalog(1.0))));
    let fetch = Arc::new(fetch);
    let catalog = Arc::new(ModelCatalog::new(Arc::<StubFetch>::clone(&fetch), now));
    let models = [model("m")];
    let (first, second) = tokio::join!(catalog.models(&models, &[]), async {
        settle().await;
        gate.add_permits(10);
        catalog.models(&models, &[]).await
    });
    assert_eq!(first.unwrap()[0].metadata.as_ref().unwrap().name, "M");
    assert!(second.unwrap()[0].metadata.is_some());
    assert_eq!(fetch.count(), 1, "concurrent callers share one fetch");
    let seen = fetch.seen.lock().unwrap()[0].clone();
    assert_eq!(seen.request.url, "https://models.dev/api.json");
    assert!(
        seen.request.headers.is_empty(),
        "no credentials or other headers"
    );
    assert_eq!(seen.request.limit, 16_000_000);
    assert_eq!(seen.request.timeout, Duration::from_secs(10));

    CLOCK.store(NOW + 3_599_999, Ordering::SeqCst);
    catalog.models(&[], &[]).await.unwrap();
    assert_eq!(fetch.count(), 1, "kept for an hour");
    CLOCK.store(NOW + 3_600_000, Ordering::SeqCst);
    fetch.answer(|_| Ok(response(500, "")));
    let Err(CatalogError::App(error)) = catalog.models(&[], &[]).await else {
        panic!("a failed refresh served the expired catalog");
    };
    assert_eq!(error.status, 502);
    fetch.answer(|_| Ok(response(200, priced_catalog(2.0))));
    catalog.models(&[], &[]).await.unwrap();
    assert_eq!(fetch.count(), 3, "a failure is not cached");
}

#[tokio::test]
async fn concurrent_callers_of_a_failed_fetch_all_get_the_502() {
    let (fetch, gate) = StubFetch::gated(|_| Ok(response(503, "")));
    let fetch = Arc::new(fetch);
    let catalog = Arc::new(ModelCatalog::new(Arc::<StubFetch>::clone(&fetch), fixed));
    let (first, second) = tokio::join!(catalog.models(&[], &[]), async {
        settle().await;
        gate.add_permits(10);
        catalog.models(&[], &[]).await
    });
    for result in [first, second] {
        assert!(matches!(
            result,
            Err(CatalogError::App(AppError { status: 502, .. }))
        ));
    }
    assert_eq!(fetch.count(), 1);
}

#[tokio::test]
async fn activity_snapshots_never_wait_and_back_off_after_a_failure() {
    static CLOCK: AtomicI64 = AtomicI64::new(NOW);
    fn now() -> i64 {
        CLOCK.load(Ordering::SeqCst)
    }
    let fetch = Arc::new(StubFetch::new(|_| Ok(response(500, ""))));
    let catalog = Arc::new(ModelCatalog::new(Arc::<StubFetch>::clone(&fetch), now));
    let snapshot = catalog.snapshot();
    assert_eq!((snapshot.fetched_at(), snapshot.refreshing), (None, true));
    settle().await;
    assert!(!catalog.snapshot().refreshing);
    assert_eq!(fetch.count(), 1);
    CLOCK.store(NOW + 59_999, Ordering::SeqCst);
    let _later = catalog.snapshot();
    settle().await;
    assert_eq!(fetch.count(), 1, "no retry within a minute");
    CLOCK.store(NOW + 60_000, Ordering::SeqCst);
    fetch.answer(|_| Ok(response(200, priced_catalog(1.0))));
    let _later = catalog.snapshot();
    settle().await;
    let snapshot = catalog.snapshot();
    assert_eq!(snapshot.fetched_at(), Some("2026-11-22T12:01:00.000Z"));
    assert_eq!(snapshot.cost("claude", "m").unwrap().input.0, 1.0);
    assert_eq!(fetch.count(), 2);
}

#[tokio::test]
async fn a_first_fetch_whose_caller_stopped_waiting_still_loads_the_catalog() {
    let (fetch, gate) = StubFetch::gated(|_| Ok(response(200, priced_catalog(1.0))));
    let fetch = Arc::new(fetch);
    let catalog = Arc::new(ModelCatalog::new(Arc::<StubFetch>::clone(&fetch), fixed));
    tokio::select! {
        _ = catalog.models(&[], &[]) => panic!("the held fetch answered"),
        () = async {
            while fetch.count() == 0 {
                tokio::task::yield_now().await;
            }
        } => {}
    }
    gate.add_permits(1);
    settle().await;
    let snapshot = catalog.snapshot();
    assert_eq!(
        (snapshot.fetched_at(), snapshot.refreshing),
        (Some(NOW_ISO), false)
    );
    assert_eq!(fetch.count(), 1);
}

/// A Claude request for model `m` that used `input` tokens.
fn usage(profile: &ProfileId, id: &str, timestamp: &str, input: u64) -> UsageRecord {
    serde_json::from_value(json!({
        "id": format!("{profile}:{id}"),
        "profileId": profile,
        "timestamp": timestamp,
        "provider": "claude",
        "model": "m",
        "pricingTokens": { "input": input, "cached": 0, "cacheWrite": 0, "output": 0, "reasoning": 0 },
        "account": "1",
        "inputTokens": input,
        "outputTokens": 0,
        "cachedTokens": 0,
        "reasoningTokens": 0,
        "cacheWriteTokens": 0,
        "totalTokens": input,
        "latencyMs": 1,
        "failed": false,
        "statusCode": 200,
        "stream": false,
    }))
    .unwrap()
}

async fn add(store: &Store, records: Vec<UsageRecord>) {
    store.call(move |db| db.add_usage(&records)).await.unwrap();
}

#[tokio::test]
async fn period_totals_follow_new_rows_shorter_periods_retention_and_new_rates() {
    static CLOCK: AtomicI64 = AtomicI64::new(NOW);
    fn now() -> i64 {
        CLOCK.load(Ordering::SeqCst)
    }
    let directory = TempDir::new("pricing");
    let store = open_store(directory.path()).await;
    let owner = profile(&store, "Owner").await;
    let other = profile(&store, "Other").await;
    let fetch = Arc::new(StubFetch::new(|_| Ok(response(200, priced_catalog(1.0)))));
    let catalog = Arc::new(ModelCatalog::new(Arc::<StubFetch>::clone(&fetch), now));
    catalog.models(&[], &[]).await.unwrap();
    let prices = Arc::new(UsagePrices::new(
        store.clone(),
        Arc::clone(&catalog),
        Arc::new(StubFetch::new(|_| Ok(response(503, "")))),
        fixed,
    ));
    let total = async |since: &str| {
        let history = prices.history(&owner, since).await.unwrap();
        let totals = history.pricing.totals.unwrap();
        (totals.requests, totals.usd.0, history.summary.input_tokens)
    };
    add(
        &store,
        vec![
            usage(&owner, "a", "2026-11-10T00:00:00.000Z", 1_000_000),
            usage(&owner, "b", "2026-11-20T00:00:00.000Z", 2_000_000),
            usage(&other, "c", "2026-11-20T00:00:00.000Z", 50_000_000),
        ],
    )
    .await;
    assert_eq!(
        total("2026-11-01T00:00:00.000Z").await,
        (2, 3.0, 3_000_000),
        "another profile's rows stay out"
    );
    add(
        &store,
        vec![usage(&owner, "d", "2026-11-21T00:00:00.000Z", 4_000_000)],
    )
    .await;
    assert_eq!(
        total("2026-11-01T00:00:00.000Z").await,
        (3, 7.0, 7_000_000),
        "new rows add"
    );
    assert_eq!(
        total("2026-11-15T00:00:00.000Z").await,
        (2, 6.0, 6_000_000),
        "a later start subtracts"
    );
    assert_eq!(
        total("2026-11-01T00:00:00.000Z").await,
        (3, 7.0, 7_000_000),
        "an earlier start rebuilds"
    );

    add(
        &store,
        vec![usage(&owner, "old", "2020-01-01T00:00:00.000Z", 8_000_000)],
    )
    .await;
    assert_eq!(
        total("2000-01-01T00:00:00.000Z").await,
        (4, 15.0, 15_000_000)
    );
    store
        .call(|db| db.set_usage_retention_days(RetentionDays::Days30))
        .await
        .unwrap();
    assert_eq!(
        total("2000-01-01T00:00:00.000Z").await,
        (3, 7.0, 7_000_000),
        "retention rebuilds"
    );

    CLOCK.store(NOW + 3_600_000, Ordering::SeqCst);
    fetch.answer(|_| Ok(response(200, priced_catalog(2.0))));
    catalog.models(&[], &[]).await.unwrap();
    assert_eq!(
        total("2000-01-01T00:00:00.000Z").await,
        (3, 14.0, 7_000_000),
        "new rates rebuild"
    );

    prices.close().await;
    assert!(matches!(
        prices.history(&owner, "2000-01-01T00:00:00.000Z").await,
        Err(PricingError::Closed)
    ));
    store.close().await;
}

fn fixture_profile() -> ProfileId {
    serde_json::from_value(json!("00000000-0000-4000-8000-000000000001")).unwrap()
}

/// A Claude request with every kind of token: 100 input, 200 cached, 50 cache writes, 30
/// output (10 of them reasoning), 380 in all.
fn fixture_record(profile: &ProfileId, id: &str) -> UsageRecord {
    serde_json::from_value(json!({
        "id": id, "profileId": profile, "timestamp": "2026-09-13T00:00:00Z",
        "provider": "claude", "model": "fixture-model", "account": "fixture",
        "inputTokens": 100, "cachedTokens": 200, "cacheWriteTokens": 50, "outputTokens": 30,
        "reasoningTokens": 10, "totalTokens": 380, "latencyMs": 10, "failed": false,
        "statusCode": 200, "stream": true,
    }))
    .unwrap()
}

/// USD per million tokens of `fixture-model`.
fn fixture_cost() -> Value {
    json!({ "input": 2, "output": 10, "cache_read": 0.2, "cache_write": 2.5 })
}

fn fixture_records(profile: &ProfileId, prefix: &str, count: u32) -> Vec<UsageRecord> {
    (0..count)
        .map(|i| fixture_record(profile, &format!("{prefix}-{i}")))
        .collect()
}

fn page_size() -> u32 {
    nsv_domain::registry::registry().usage_page_size
}

fn unavailable() -> Arc<StubFetch> {
    Arc::new(StubFetch::new(|_| Ok(response(503, ""))))
}

#[test]
fn a_context_tier_prices_requests_above_its_threshold_and_needs_cache_prices() {
    let tokens = normalized_tokens(&fixture_record(&fixture_profile(), "fixture"));
    let base = fixture_cost();
    let price = |tier: Value| {
        let mut cost = base.clone();
        cost["tiers"] = json!([tier]);
        estimate_usd(
            Some(&serde_json::from_value::<Cost>(cost).unwrap()),
            tokens.as_ref(),
        )
    };
    let tier = |size: u64| {
        let mut tier = base.clone();
        tier["input"] = json!(4);
        tier["tier"] = json!({ "type": "context", "size": size });
        tier
    };
    assert_eq!(price(tier(350)), Some(0.000_665));
    assert_eq!(price(tier(349)), Some(0.000_865));
    assert_eq!(
        price(json!({ "input": 4, "output": 10, "tier": { "type": "context", "size": 349 } })),
        None
    );
}

#[tokio::test]
async fn unknown_prices_and_incomplete_breakdowns_are_missing_not_free() {
    let directory = TempDir::new("missing-prices");
    let store = open_store(directory.path()).await;
    let owner = profile(&store, "Owner").await;
    let catalog = Arc::new(ModelCatalog::new(
        Arc::new(StubFetch::new(|_| Ok(response(200, priced_catalog(1.0))))),
        fixed,
    ));
    catalog.models(&[], &[]).await.unwrap();
    let mut unknown = usage(&owner, "unknown", "2026-11-20T00:00:00.000Z", 10);
    unknown.model = "not-in-catalog".to_owned();
    let mut incomplete = usage(&owner, "incomplete", "2026-11-20T00:00:00.000Z", 10);
    incomplete.pricing_tokens = Some(None);
    let mut mismatched = usage(&owner, "mismatched", "2026-11-20T00:00:00.000Z", 10);
    mismatched.pricing_tokens = Some(Some(PricingTokens {
        input: 9,
        ..PricingTokens::default()
    }));
    add(&store, vec![unknown, incomplete, mismatched]).await;
    let prices = Arc::new(UsagePrices::new(
        store.clone(),
        Arc::clone(&catalog),
        Arc::new(StubFetch::new(|_| Ok(response(503, "")))),
        fixed,
    ));
    let history = prices
        .history(&owner, "2026-11-01T00:00:00.000Z")
        .await
        .unwrap();
    assert!(history.pricing.usd.values().all(Option::is_none));
    let totals = history.pricing.totals.unwrap();
    assert_eq!(
        (
            totals.requests,
            totals.priced_requests,
            totals.normalized_requests,
            totals.usd.0
        ),
        (3, 0, 1, 0.0)
    );
}

#[tokio::test]
async fn counters_at_u64_max_saturate_the_period_totals() {
    let directory = TempDir::new("pricing-saturated");
    let store = open_store(directory.path()).await;
    let owner = profile(&store, "Owner").await;
    let catalog = Arc::new(ModelCatalog::new(
        Arc::new(StubFetch::new(|_| Ok(response(200, priced_catalog(1.0))))),
        fixed,
    ));
    catalog.models(&[], &[]).await.unwrap();
    add(
        &store,
        vec![
            usage(&owner, "a", "2026-11-20T00:00:00.000Z", u64::MAX),
            usage(&owner, "b", "2026-11-21T00:00:00.000Z", u64::MAX),
        ],
    )
    .await;
    let prices = Arc::new(UsagePrices::new(
        store.clone(),
        catalog,
        unavailable(),
        fixed,
    ));
    let history = prices
        .history(&owner, "2026-11-01T00:00:00.000Z")
        .await
        .unwrap();
    let summary = history.summary;
    assert_eq!(
        (summary.requests, summary.input_tokens, summary.total_tokens),
        (2, u64::MAX, u64::MAX)
    );
    // Past 2^53 a breakdown is incomplete, so neither row is priced.
    let totals = history.pricing.totals.unwrap();
    assert_eq!(
        (totals.requests, totals.normalized_requests, totals.usd.0),
        (2, 0, 0.0)
    );
    prices.close().await;
    store.close().await;
}

#[test]
fn quota_readings_keep_unknown_values_unknown() {
    let now = 1_788_609_600_000; // 2026-09-05T12:00:00Z
    let codex = parse_quota(
        "codex",
        &json!({ "plan_type": "team", "rate_limit": {
            "primary_window": { "used_percent": 31, "limit_window_seconds": 18000, "reset_at": 1_788_616_800 },
            "secondary_window": { "used_percent": null, "limit_window_seconds": 2_592_000 },
        } }),
        now,
    )
    .unwrap();
    let windows = serde_json::to_value(&codex.windows).unwrap();
    assert_eq!(
        windows,
        json!([
            { "label": "5-hour", "remainingPercent": 69, "resetsAt": "2026-09-05T14:00:00.000Z" },
            { "label": "Monthly", "remainingPercent": null, "resetsAt": null },
        ])
    );
    assert_eq!(codex.plan.as_deref(), Some("team"));

    let claude = parse_quota(
        "claude",
        &json!({ "five_hour": { "utilization": null }, "seven_day": { "utilization": 0 }, "seven_day_opus": { "utilization": 87.5 } }),
        now,
    )
    .unwrap();
    let percents: Vec<_> = claude
        .windows
        .iter()
        .map(|window| window.remaining_percent.map(|percent| percent.0))
        .collect();
    assert_eq!(percents, [None, Some(100.0), Some(12.5)]);
    assert_eq!(
        claude
            .windows
            .iter()
            .map(|window| window.scoped.is_some())
            .collect::<Vec<_>>(),
        [false, false, true]
    );

    let go = parse_quota(
        "opencode-go",
        &json!({ "usage": { "rolling": { "usagePercent": 0.5, "resetInSec": 1800 } } }),
        now,
    )
    .unwrap();
    assert_eq!(go.windows[0].remaining_percent.unwrap().0, 99.5);
    assert_eq!(
        go.windows[0].resets_at.as_deref(),
        Some("2026-09-05T12:30:00.000Z")
    );
    let xai = parse_quota("xai", &json!({ "monthly_limit": 0, "used": 0 }), now).unwrap();
    assert_eq!(xai.windows[0].remaining_percent, None);
    assert_eq!(
        parse_quota("unknown", &json!({}), now).unwrap().status,
        QuotaStatus::Unavailable
    );
    assert_eq!(
        parse_quota(
            "codex",
            &json!({ "rate_limit": { "primary_window": { "reset_after_seconds": 1e20 } } }),
            now
        )
        .unwrap_err()
        .to_string(),
        "Invalid time value"
    );

    for count in [json!(0), json!(1), json!(3)] {
        let payload = json!({ "rate_limit_reset_credits": { "available_count": count } });
        assert_eq!(
            json!(parse_quota("codex", &payload, now).unwrap().banked_resets),
            count
        );
        assert_eq!(
            parse_quota("claude", &payload, now).unwrap().banked_resets,
            None
        );
    }
    for count in [
        json!(null),
        json!(-1),
        json!(1.5),
        json!("invalid"),
        json!(9_007_199_254_740_992_u64),
    ] {
        let payload = json!({ "rate_limit_reset_credits": { "available_count": count } });
        assert_eq!(
            parse_quota("codex", &payload, now).unwrap().banked_resets,
            None,
            "{count}"
        );
    }
}

#[test]
fn claude_scoped_windows_keep_their_own_resets_and_unknown_readings() {
    let now = 1_788_609_600_000; // 2026-09-05T12:00:00Z
    let payload = json!({
        "five_hour": { "utilization": 25 },
        "seven_day": { "utilization": 40, "resets_at": "2026-09-07T12:00:00Z" },
        "seven_day_opus": { "utilization": 20 },
        "seven_day_sonnet": { "utilization": 10 },
        "limits": [
            { "kind": "weekly_all", "percent": 40 },
            {
                "kind": "weekly_scoped", "percent": 87.5, "resets_at": "2026-09-08T12:00:00Z",
                "scope": { "model": { "id": null, "display_name": "Fable" } }, "is_active": false,
            },
            { "kind": "weekly_scoped", "percent": 20, "scope": { "model": { "display_name": "Opus" } } },
        ],
    });
    let quota = serde_json::to_value(
        parse_quota("claude", &Value::String(payload.to_string()), now).unwrap(),
    )
    .unwrap();
    assert_eq!(quota["status"], "available");
    let windows = quota["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 5, "{windows:?}");
    assert_eq!(windows[1]["remainingPercent"], 60);
    assert_eq!(
        windows[4],
        json!({ "label": "Fable weekly", "remainingPercent": 12.5, "resetsAt": "2026-09-08T12:00:00.000Z", "scoped": true })
    );
    assert_ne!(windows[1]["resetsAt"], windows[4]["resetsAt"]);

    for (percent, expected) in [
        (Value::Null, Value::Null),
        (json!("invalid"), Value::Null),
        (json!(0), json!(100)),
        (json!(0.5), json!(99.5)),
        (json!(100), json!(0)),
        (json!(120), json!(0)),
        (json!(-1), json!(100)),
    ] {
        let payload = json!({ "limits": [
            null,
            { "kind": "weekly_scoped", "scope": null },
            { "kind": "weekly_scoped", "scope": { "model": { "display_name": 5 } } },
            { "kind": "weekly_scoped", "scope": { "model": { "display_name": " " } } },
            {
                "kind": "weekly_scoped", "scope": { "model": { "display_name": " Fable " } },
                "percent": percent, "resets_at": "invalid",
            },
        ] });
        let quota = serde_json::to_value(parse_quota("claude", &payload, now).unwrap()).unwrap();
        assert_eq!(
            quota["windows"],
            json!([{ "label": "Fable weekly", "remainingPercent": expected, "resetsAt": null, "scoped": true }]),
            "{percent}"
        );
    }
    for limits in [None, Some(Value::Null), Some(json!({})), Some(json!([]))] {
        let payload = limits.map_or_else(|| json!({}), |limits| json!({ "limits": limits }));
        let quota = serde_json::to_value(parse_quota("claude", &payload, now).unwrap()).unwrap();
        assert_eq!(quota["status"], "unavailable", "{payload}");
    }
}

fn subscription(id: &str, provider: &str) -> Account {
    serde_json::from_value(json!({
        "id": id, "name": "Codex", "provider": provider, "kind": "oauth", "disabled": false,
        "status": "Ready", "priority": 0, "authIndex": "fixture-index",
    }))
    .unwrap()
}

#[tokio::test]
async fn quota_checks_run_once_per_account_and_never_store_response_excerpts() {
    let directory = TempDir::new("quota-refresh");
    let store = open_store(directory.path()).await;
    let owner = profile(&store, "Owner").await;
    let go: ApiAccount = serde_json::from_value(json!({
        "id": "go", "name": "Go", "provider": "opencode-go", "baseUrl": "https://opencode.ai/zen/go/v1",
        "prefix": "", "models": [], "disabled": false,
    }))
    .unwrap();
    let saved = owner.clone();
    store
        .call(move |db| db.save_api_account(&saved, &go, Some(&ApiKey::new("synthetic-key"))))
        .await
        .unwrap();
    let mut go_account = subscription("go", "opencode-go");
    go_account.kind = nsv_domain::wire::AccountKind::ApiKey;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let source = Arc::new(StubSource {
        accounts: vec![subscription("claude.json", "claude"), go_account],
        gate: Some(Arc::clone(&gate)),
        ..StubSource::default()
    });
    let fetch = Arc::new(StubFetch::new(|_| {
        Ok(response(200, r#"{"secret":"SENTINEL_9f3a""#))
    }));
    let quotas = Quotas::new(
        store.clone(),
        Arc::clone(&source),
        Arc::<StubFetch>::clone(&fetch),
        fixed,
    );
    let claude = AccountId::parse("claude.json").unwrap();
    source.queue([json!({ "status_code": 200, "body": { "five_hour": { "utilization": 25 } } })]);
    let (first, second) = tokio::join!(quotas.refresh(&owner, &claude), async {
        called(&source, 1).await;
        let second = quotas.refresh(&owner, &claude).await;
        gate.add_permits(10);
        second
    });
    assert_eq!(first.unwrap().windows[0].remaining_percent.unwrap().0, 75.0);
    assert!(matches!(
        second,
        Err(QuotaError::App(AppError { status: 409, .. }))
    ));

    let blocked = Arc::new(tokio::sync::Semaphore::new(0));
    let source = Arc::new(StubSource {
        accounts: source.accounts.clone(),
        gate: Some(Arc::clone(&blocked)),
        ..StubSource::default()
    });
    let quotas = Quotas::new(
        store.clone(),
        Arc::clone(&source),
        Arc::<StubFetch>::clone(&fetch),
        fixed,
    );
    source.queue([json!({ "status_code": 500, "body": "" })]);
    let (failed, stored) = tokio::join!(quotas.refresh(&owner, &claude), async {
        called(&source, 1).await;
        let stored = quotas.refresh(&owner, &claude).await;
        blocked.add_permits(10);
        stored
    });
    assert_eq!(
        stored.unwrap().status,
        QuotaStatus::Available,
        "a running check answers the stored quota"
    );
    let failed = failed.unwrap();
    assert_eq!(
        (failed.status, failed.windows.len()),
        (QuotaStatus::Error, 1),
        "a failure keeps the last windows"
    );

    let quota = quotas
        .refresh(&owner, &AccountId::parse("go").unwrap())
        .await
        .unwrap();
    assert_eq!(
        quota.error.as_deref(),
        Some("Quota response was not valid JSON.")
    );
    let (profile_id, account) = (owner.clone(), AccountId::parse("go").unwrap());
    let stored = store
        .call(move |db| db.quota(&profile_id, &account))
        .await
        .unwrap()
        .unwrap();
    assert!(
        !serde_json::to_string(&stored)
            .unwrap()
            .contains("SENTINEL_9f3a")
    );
    let seen = fetch.seen.lock().unwrap()[0].clone();
    assert_eq!(seen.request.url, "https://opencode.ai/zen/go/v1/usage");
    assert_eq!(
        seen.request.headers[0],
        ("Authorization", "Bearer synthetic-key".to_owned())
    );
}

/// The remaining percent of a quota's first window.
fn remaining(quota: &Quota) -> f64 {
    quota.windows[0].remaining_percent.unwrap().0
}

#[tokio::test]
async fn a_check_queued_behind_a_running_one_answers_the_stored_quota_once_it_is_saved() {
    let directory = TempDir::new("quota-overlap");
    let store = open_store(directory.path()).await;
    let owner = profile(&store, "Owner").await;
    let claude = AccountId::parse("claude.json").unwrap();
    let old = parse_quota(
        "claude",
        &json!({ "five_hour": { "utilization": 90 } }),
        NOW,
    )
    .unwrap();
    let (id, account) = (owner.clone(), claude.clone());
    store
        .call(move |db| db.save_quota(&id, &account, &old))
        .await
        .unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let source = Arc::new(StubSource {
        accounts: vec![subscription("claude.json", "claude")],
        gate: Some(Arc::clone(&gate)),
        ..StubSource::default()
    });
    source.queue([
        json!({ "status_code": 200, "body": { "five_hour": { "utilization": 25 } } }),
        json!({ "status_code": 500, "body": "" }),
    ]);
    let quotas = Quotas::new(store.clone(), Arc::clone(&source), unavailable(), fixed);
    let (release, held) = std::sync::mpsc::channel::<()>();
    let (first, second) = tokio::join!(quotas.refresh(&owner, &claude), async {
        called(&source, 1).await;
        // The store answers nothing until released, so the second check's read and the first
        // one's save queue up in that order.
        let blocker = tokio::spawn({
            let store = store.clone();
            async move {
                store
                    .call(move |_| {
                        let _ = held.recv();
                        Ok(())
                    })
                    .await
            }
        });
        settle().await;
        let (second, ()) = tokio::join!(quotas.refresh(&owner, &claude), async {
            settle().await;
            gate.add_permits(10);
            settle().await;
            release.send(()).unwrap();
        });
        blocker.await.unwrap().unwrap();
        second
    });
    assert_eq!(remaining(&first.unwrap()), 75.0);
    let second = second.unwrap();
    assert_eq!(
        (second.status, remaining(&second)),
        (QuotaStatus::Available, 10.0),
        "the queued check answers the stored quota"
    );
    assert_eq!(source.take_calls().len(), 1, "the provider is asked once");
    let stored = store
        .call(move |db| db.quota(&owner, &claude))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (stored.status, remaining(&stored)),
        (QuotaStatus::Available, 75.0)
    );
    store.close().await;
}

#[tokio::test]
async fn a_reading_with_room_in_every_window_clears_the_quota_cooldown_the_core_still_holds() {
    let directory = TempDir::new("quota-cooldown");
    let store = open_store(directory.path()).await;
    let owner = profile(&store, "Owner").await;
    let source = Arc::new(StubSource {
        accounts: vec![
            subscription("cooling.json", "claude"),
            subscription("ready.json", "claude"),
        ],
        cooling: ["cooling.json".to_owned()].into(),
        ..StubSource::default()
    });
    let quotas = Quotas::new(store.clone(), Arc::clone(&source), unavailable(), fixed);
    // The `auth_index` of each cooldown a refresh cleared.
    let refresh = async |account: &str, answer: Value| {
        source.queue([answer]);
        quotas
            .refresh(&owner, &AccountId::parse(account).unwrap())
            .await
            .unwrap();
        std::mem::take(&mut *source.cleared.lock().unwrap())
    };
    let usage = |five_hour: u32, opus: u32| {
        json!({ "status_code": 200, "body": {
            "five_hour": { "utilization": five_hour },
            "seven_day": { "utilization": 40 },
            "seven_day_opus": { "utilization": opus },
        } })
    };

    assert!(
        refresh("cooling.json", usage(100, 10)).await.is_empty(),
        "the account is still at its limit"
    );
    assert!(
        refresh("cooling.json", usage(0, 100)).await.is_empty(),
        "a model is still at its limit"
    );
    let unknown = json!({ "status_code": 200, "body": {
        "five_hour": { "utilization": 0 },
        "seven_day_opus": { "resets_at": "2026-12-01T00:00:00Z" },
    } });
    assert!(
        refresh("cooling.json", unknown).await.is_empty(),
        "a window of unknown usage may still be at its limit"
    );
    assert!(
        refresh("ready.json", usage(0, 10)).await.is_empty(),
        "the core holds no cooldown"
    );
    assert_eq!(
        refresh("cooling.json", usage(0, 10)).await,
        ["fixture-index"],
        "a reset the core did not see"
    );
    assert!(
        refresh("cooling.json", json!({ "status_code": 500, "body": "" }))
            .await
            .is_empty(),
        "a failed check keeps the last reading but proves nothing"
    );

    // A clear still in flight keeps the account's turn: no newer check can be overtaken by it.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let source = Arc::new(StubSource {
        accounts: vec![subscription("cooling.json", "claude")],
        cooling: ["cooling.json".to_owned()].into(),
        clear_gate: Some(Arc::clone(&gate)),
        ..StubSource::default()
    });
    let quotas = Quotas::new(store.clone(), Arc::clone(&source), unavailable(), fixed);
    let cooling = AccountId::parse("cooling.json").unwrap();
    source.queue([usage(0, 10), usage(100, 10)]);
    let (first, second) = tokio::join!(quotas.refresh(&owner, &cooling), async {
        while source.cleared.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        let second = quotas.refresh(&owner, &cooling).await;
        gate.add_permits(1);
        second
    });
    first.unwrap();
    assert_eq!(
        remaining(&second.unwrap()),
        100.0,
        "the stored reading answers"
    );
    assert_eq!(source.take_calls().len(), 1, "the provider is asked once");
    store.close().await;
}

struct Refresh(Result<QuotaStatus, ()>);

impl RefreshQuota for Refresh {
    fn refresh(
        &self,
        _: &ProfileId,
        _: &AccountId,
    ) -> impl Future<Output = Result<Quota, QuotaError>> + Send {
        ready(
            self.0
                .map(|status| Quota {
                    status,
                    windows: Vec::new(),
                    checked_at: String::new(),
                    error: None,
                    plan: None,
                    banked_resets: None,
                })
                .map_err(|()| AppError::new("Quota refresh is already in progress.", 409).into()),
        )
    }
}

fn reset_source(gate: Option<Arc<tokio::sync::Semaphore>>) -> StubSource {
    StubSource {
        accounts: vec![subscription("fixture.json", "codex")],
        chatgpt: [("fixture.json".to_owned(), "fixture-identity".to_owned())].into(),
        gate,
        ..StubSource::default()
    }
}

fn redeem_request_id(calls: &[Value]) -> String {
    let data: Value =
        serde_json::from_str(calls.last().unwrap()["data"].as_str().unwrap()).unwrap();
    assert_eq!(data["credit_id"], "credit-one");
    data["redeem_request_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one redemption history, step by step"
)]
async fn redemptions_retry_with_the_same_request_until_the_outcome_is_known() {
    let directory = TempDir::new("resets");
    let store = open_store(directory.path()).await;
    let owner = profile(&store, "Owner").await;
    let account = AccountId::parse("fixture.json").unwrap();
    let source = Arc::new(reset_source(None));
    let available = Arc::new(Refresh(Ok(QuotaStatus::Available)));
    let resets = Resets::new(
        store.clone(),
        Arc::clone(&source),
        Arc::clone(&available),
        fixed,
    );
    let unrefreshed = Resets::new(
        store.clone(),
        Arc::clone(&source),
        Arc::new(Refresh(Err(()))),
        fixed,
    );
    let consume = async |answer: Value, resets: &Resets<Arc<StubSource>, Refresh>| {
        source.queue([answer]);
        let result = resets.consume(&owner, &account, "credit-one").await;
        (result, redeem_request_id(&source.take_calls()))
    };

    let (failed, first) = consume(
        json!({ "status_code": 503, "body": "sensitive upstream error" }),
        &resets,
    )
    .await;
    let Err(QuotaError::App(error)) = failed else {
        panic!("an upstream failure succeeded")
    };
    assert_eq!(error.status, 502);
    assert!(!error.message.contains("sensitive"));
    let (reset, retried) = consume(
        json!({ "status_code": 200, "body": "{\"code\":\"reset\"}" }),
        &resets,
    )
    .await;
    assert_eq!(
        serde_json::to_value(reset.unwrap()).unwrap(),
        json!({ "code": "reset", "quotaRefreshed": true })
    );
    assert_eq!(retried, first, "a retry redeems with the same request id");
    let (redeemed, again) = consume(
        json!({ "status_code": 200, "body": { "code": "already_redeemed" } }),
        &unrefreshed,
    )
    .await;
    assert_eq!(
        serde_json::to_value(redeemed.unwrap()).unwrap(),
        json!({ "code": "already_redeemed", "quotaRefreshed": false })
    );
    assert_eq!(again, first);
    let (unknown, kept) = consume(
        json!({ "status_code": 200, "body": { "code": "unexpected" } }),
        &resets,
    )
    .await;
    assert!(matches!(
        unknown,
        Err(QuotaError::App(AppError { status: 502, .. }))
    ));
    assert_eq!(kept, first, "an unknown outcome keeps the request");
    let (nothing, last) = consume(
        json!({ "status_code": 200, "body": { "code": "nothing_to_reset" } }),
        &resets,
    )
    .await;
    assert!(nothing.is_ok());
    assert_eq!(last, first);
    let (_, fresh) = consume(
        json!({ "status_code": 200, "body": { "code": "no_credit" } }),
        &resets,
    )
    .await;
    assert_ne!(fresh, first, "nothing redeemed clears the request");

    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let gated = Arc::new(reset_source(Some(Arc::clone(&gate))));
    let resets = Resets::new(
        store.clone(),
        Arc::clone(&gated),
        Arc::clone(&available),
        fixed,
    );
    gated.queue([json!({ "status_code": 200, "body": { "code": "reset" } })]);
    let (running, concurrent) =
        tokio::join!(resets.consume(&owner, &account, "credit-one"), async {
            called(&gated, 1).await;
            let concurrent = resets.consume(&owner, &account, "credit-one").await;
            gate.add_permits(10);
            concurrent
        });
    assert_eq!(
        running.unwrap(),
        ResetResult {
            code: nsv_domain::registry::ResetResultCode::Reset,
            quota_refreshed: true
        }
    );
    let Err(QuotaError::App(error)) = concurrent else {
        panic!("two redemptions ran at once")
    };
    assert_eq!(
        (error.status, error.message.as_str()),
        (409, "A reset is already in progress for this subscription.")
    );

    let stopped = Resets::new(
        store.clone(),
        StubSource {
            stopped: true,
            ..reset_source(None)
        },
        available,
        fixed,
    );
    let Err(QuotaError::App(error)) = stopped.credits(&owner, &account).await else {
        panic!()
    };
    assert_eq!(
        (error.status, error.message.as_str()),
        (409, "Start this profile first.")
    );
    store.close().await;
}

const ORGANIZATION: &str = "bbbbbbbb-0000-4000-8000-000000000001";

fn claude_seat() -> StubSource {
    let mut account = subscription("claude.json", "claude");
    account.organization_uuid = Some(ORGANIZATION.to_owned());
    account.account_uuid = Some("aaaaaaaa-0000-4000-8000-000000000001".to_owned());
    StubSource {
        accounts: vec![account],
        ..StubSource::default()
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one Claude status per case, each with its answer"
)]
async fn claude_reset_grants_list_one_credit_per_kind_as_claude_code_offers_them() {
    let directory = TempDir::new("claude-resets");
    let store = open_store(directory.path()).await;
    let owner = profile(&store, "Owner").await;
    let account = AccountId::parse("claude.json").unwrap();
    let source = Arc::new(claude_seat());
    let resets = Resets::new(
        store.clone(),
        Arc::clone(&source),
        Arc::new(Refresh(Ok(QuotaStatus::Available))),
        fixed,
    );
    let list = async |status: Value| {
        source.queue([json!({ "status_code": 200, "body": {
            "seven_day": { "utilization": 100 },
            "cedar_ember": status,
        } })]);
        resets
            .credits(&owner, &account)
            .await
            .map(|credits| serde_json::to_value(credits).unwrap())
    };
    let fable = |id: &str, left: u32, usable: bool| {
        json!({ "id": id, "label": "New model launch", "resets_total": 1, "resets_left": left,
                "ends_at": "2027-03-09T07:59:00+00:00",
                "clears": ["five_hour", "seven_day_overage_included"], "paused": false,
                "usable_now": usable, "use_requires_limit": false, "blocking": [], "type": "full",
                "gives_back_weekly": true,
                "unavailable_reason": if usable { Value::Null } else { json!("unavailable") } })
    };

    // As Claude answered a Team seat at its weekly limit: two usable Fable grants, spent ones.
    let listed = list(json!({
        "eligible": true, "at_limit": true, "exhausted": ["seven_day"],
        "next_grant_id": "launch-fable-12mo", "cooldown_until": null,
        "grants": [
            { "id": "full-reset", "resets_total": 1, "resets_left": 0, "clears": ["five_hour", "seven_day"],
              "usable_now": false, "type": "full", "unavailable_reason": "unavailable" },
            fable("launch-fable", 0, false),
            // Ended before the test clock's 2026-11-22: never offered, never counted.
            { "id": "ended-fable", "resets_left": 1, "ends_at": "2026-11-20T00:00:00+00:00",
              "clears": ["five_hour"], "usable_now": true, "gives_back_weekly": true },
            fable("launch-fable-12mo", 1, true),
            fable("launch-fable-6mo", 1, true),
            { "id": "weekly-gift", "resets_left": 1, "ends_at": null, "clears": ["seven_day"],
              "usable_now": false, "blocking": ["five_hour"], "type": "weekly" },
            { "id": "paused", "resets_left": 1, "clears": ["five_hour"], "paused": true, "usable_now": true },
            { "id": "Not A Grant", "resets_left": 1, "clears": [], "usable_now": true },
        ],
    }))
    .await
    .unwrap();
    assert_eq!(
        listed,
        json!({ "available_count": 2, "credits": [
            { "id": "launch-fable-12mo", "reset_type": "Fable reset", "status": "available",
              "expires_at": "2027-03-09T07:59:00+00:00",
              "description": "Refills your Fable and 5-hour limits · 2 left" },
            { "id": "weekly-gift", "reset_type": "Weekly reset", "status": "unavailable",
              "expires_at": null,
              "description": "Refills your weekly limit · 1 left · can't be used while you're at your 5-hour limit" },
        ] })
    );
    let call = source.take_calls().pop().unwrap();
    assert_eq!(
        (&call["auth_index"], &call["method"], &call["url"]),
        (
            &json!("fixture-index"),
            &json!("GET"),
            &json!("https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1")
        )
    );
    assert_eq!(
        (
            &call["header"]["anthropic-beta"],
            &call["header"]["User-Agent"]
        ),
        (&json!("oauth-2025-04-20"), &json!(CLAUDE_CODE_USER_AGENT)),
        "Claude lists resets only to Claude Code"
    );

    let early = list(json!({
        "eligible": true, "at_limit": false,
        "grants": [{ "id": "session-gift", "resets_left": 1, "clears": ["five_hour"], "usable_now": false }],
    }))
    .await
    .unwrap();
    assert_eq!(
        (
            &early["available_count"],
            &early["credits"][0]["reset_type"],
            &early["credits"][0]["description"]
        ),
        (
            &json!(0),
            &json!("5-hour reset"),
            &json!("Refills your 5-hour limit · 1 left · usable once you reach a limit it refills")
        )
    );
    assert_eq!(
        list(Value::Null).await.unwrap(),
        json!({ "available_count": 0, "credits": [] }),
        "not in the program"
    );
    assert_eq!(
        list(json!({ "eligible": false, "ineligible_reason": "surface", "grants": [] }))
            .await
            .unwrap(),
        json!({ "available_count": 0, "credits": [],
                "note": "Claude offers resets only to Claude Code and claude.ai." })
    );
    let Err(QuotaError::App(error)) = list(json!({ "grants": [] })).await else {
        panic!("a block without eligibility was read")
    };
    assert_eq!(
        (error.status, error.message.as_str()),
        (502, "Claude reset information is unavailable.")
    );

    let mut unverified = claude_seat();
    unverified.accounts[0].organization_uuid = None;
    let resets = Resets::new(
        store.clone(),
        unverified,
        Arc::new(Refresh(Ok(QuotaStatus::Available))),
        fixed,
    );
    let Err(QuotaError::App(error)) = resets.credits(&owner, &account).await else {
        panic!("a seat without its organization was asked")
    };
    assert_eq!(error.status, 409);
    store.close().await;
}

#[tokio::test]
async fn claude_claims_reuse_a_request_id_only_until_claude_names_an_outcome() {
    let directory = TempDir::new("claude-claims");
    let store = open_store(directory.path()).await;
    let owner = profile(&store, "Owner").await;
    let account = AccountId::parse("claude.json").unwrap();
    let source = Arc::new(claude_seat());
    let resets = Resets::new(
        store.clone(),
        Arc::clone(&source),
        Arc::new(Refresh(Ok(QuotaStatus::Available))),
        fixed,
    );
    // The outcome, and the request id sent.
    let claim = async |result: &str| {
        source.queue([json!({ "status_code": 200, "body": { "result": result } })]);
        let outcome = resets.consume(&owner, &account, "holiday").await;
        let call = source.take_calls().pop().unwrap();
        assert_eq!(
            call["url"],
            format!("https://api.anthropic.com/api/organizations/{ORGANIZATION}/reset_rate_limits")
        );
        assert_eq!(call["header"]["User-Agent"], CLAUDE_CODE_USER_AGENT);
        let data: Value = serde_json::from_str(call["data"].as_str().unwrap()).unwrap();
        assert_eq!(
            (&data["program"], &data["grant_id"]),
            (&json!("cedar_ember"), &json!("holiday"))
        );
        let code = outcome.map(|result| serde_json::to_value(result.code).unwrap());
        (code, data["request_id"].as_str().unwrap().to_owned())
    };
    let status = |outcome: Result<Value, QuotaError>| match outcome {
        Err(QuotaError::App(error)) => error.status,
        other => panic!("an outcome Claude did not name settled: {other:?}"),
    };

    let (unknown, first) = claim("unavailable").await;
    assert_eq!(status(unknown), 502);
    let (busy, retried) = claim("cooldown").await;
    assert_eq!(status(busy), 409);
    assert_eq!(retried, first, "a retry claims with the same request id");
    let (reset, settled) = claim("reset").await;
    assert_eq!(reset.unwrap(), "reset");
    assert_eq!(settled, first);
    let (_, next) = claim("not_limited").await;
    assert_ne!(next, first, "a grant's next reset is a new request");
    let (ineligible, fresh) = claim("ineligible").await;
    assert_eq!(ineligible.unwrap(), "no_credit");
    assert_ne!(fresh, next);

    let Err(QuotaError::App(error)) = resets.consume(&owner, &account, "../holiday").await else {
        panic!("a malformed grant id was claimed")
    };
    assert_eq!(error.status, 400);
    assert!(source.take_calls().is_empty(), "Claude is never asked");
    store.close().await;
}

#[tokio::test]
async fn a_local_provider_is_discovered_over_loopback() {
    let fetch = StubFetch::new(|_| Ok(response(200, r#"{"data":[{"id":"llama"}]}"#)));
    let input: ApiAccountInput = serde_json::from_value(json!({
        "name": "Local", "provider": "custom", "baseUrl": "http://localhost:11434/v1/", "apiKey": "k",
        "protocol": "openai", "prefix": "", "models": [],
    }))
    .unwrap();
    let account = api_account(&input, &fetch).await.unwrap();
    assert_eq!(account.base_url, "http://localhost:11434/v1");
    assert_eq!(
        (account.models[0].id.as_str(), account.models[0].protocol),
        ("llama", Protocol::Openai)
    );
    let seen = fetch.seen.lock().unwrap()[0].clone();
    let to = Loopback::new("127.0.0.1:11434".parse().unwrap()).unwrap();
    assert_eq!(seen.loopback, Some((to, "/v1/models".to_owned())));
    assert_eq!(
        seen.request.headers[0],
        ("Authorization", "Bearer k".to_owned())
    );
    assert_eq!(seen.request.timeout, Duration::from_secs(20));
}

#[tokio::test]
async fn the_euro_rate_is_fetched_once_without_credentials_and_never_waited_for() {
    let (models, release_models) = StubFetch::gated(|_| {
        let catalog = json!({ "anthropic": { "models": { "fixture-model": {
            "name": "Fixture",
            "limit": { "context": 64000, "output": 8000 },
            "modalities": { "input": ["text"], "output": ["text"] },
            "reasoning": true,
            "tool_call": true,
            "cost": fixture_cost(),
        } } } });
        Ok(response(200, catalog.to_string()))
    });
    let (rates, release_rates) = StubFetch::gated(|_| {
        Ok(response(
            200,
            r#"{"base":"USD","quote":"EUR","rate":0.9,"date":"2026-09-11"}"#,
        ))
    });
    let (models, rates) = (Arc::new(models), Arc::new(rates));
    let directory = TempDir::new("euro-rate");
    let store = open_store(directory.path()).await;
    let catalog = Arc::new(ModelCatalog::new(Arc::<StubFetch>::clone(&models), fixed));
    let prices = Arc::new(UsagePrices::new(
        store.clone(),
        Arc::clone(&catalog),
        Arc::<StubFetch>::clone(&rates),
        fixed,
    ));
    let profile = fixture_profile();
    let record = fixture_record(&profile, "fixture");
    let first = prices.snapshot(std::slice::from_ref(&record));
    assert_eq!(first.usd["fixture"], None);
    assert_eq!(first.eur, None);
    let _again = prices.snapshot(std::slice::from_ref(&record));
    settle().await;
    assert_eq!((models.count(), rates.count()), (1, 1));
    let asked = rates.seen.lock().unwrap()[0].request.clone();
    assert_eq!(
        asked.url,
        "https://api.frankfurter.dev/v2/providers/ecb/rate/usd/eur"
    );
    assert!(asked.headers.is_empty(), "{:?}", asked.headers);
    assert!(
        models.seen.lock().unwrap()[0].request.headers.is_empty(),
        "the catalog is fetched without credentials"
    );

    release_models.add_permits(1);
    release_rates.add_permits(1);
    catalog.models(&[], &[]).await.unwrap();
    settle().await;
    let mut unknown = fixture_record(&profile, "unknown");
    unknown.provider = "custom".to_owned();
    let both = prices.snapshot(&[record, unknown]);
    assert_eq!(both.usd["fixture"].map(|usd| usd.0), Some(0.000_665));
    assert_eq!(both.usd["unknown"], None);
    assert_eq!(
        serde_json::to_value(&both.eur).unwrap(),
        json!({ "rate": 0.9, "date": "2026-09-11" })
    );
    assert!(both.fetched_at.is_some());
    assert_eq!((models.count(), rates.count()), (1, 1));
    let personal = prices.snapshot(&[fixture_record(&profile, "personal")]);
    assert_eq!(
        personal.usd.keys().collect::<Vec<_>>(),
        ["personal"],
        "prices hold only the records asked about"
    );
}

#[tokio::test]
async fn an_outage_backs_off_instead_of_retrying_on_every_poll() {
    let fetch = unavailable();
    let directory = TempDir::new("outage");
    let store = open_store(directory.path()).await;
    let catalog = Arc::new(ModelCatalog::new(Arc::<StubFetch>::clone(&fetch), fixed));
    let prices = Arc::new(UsagePrices::new(
        store.clone(),
        Arc::clone(&catalog),
        Arc::<StubFetch>::clone(&fetch),
        fixed,
    ));
    let record = fixture_record(&fixture_profile(), "fixture");
    let _first = prices.snapshot(std::slice::from_ref(&record));
    settle().await;
    let result = prices.snapshot(&[record]);
    settle().await;
    assert_eq!(fetch.count(), 2, "one catalog and one rate request");
    assert_eq!(result.usd["fixture"], None);
    assert_eq!(result.eur, None);
    assert!(!result.refreshing);
}

#[tokio::test]
async fn histories_restart_after_retention_and_stop_once_prices_close() {
    let directory = TempDir::new("pricing-lifecycle");
    let store = open_store(directory.path()).await;
    let owner = profile(&store, "Lifecycle").await;
    let fetch = unavailable();
    let catalog = Arc::new(ModelCatalog::new(Arc::<StubFetch>::clone(&fetch), fixed));
    let prices = Arc::new(UsagePrices::new(
        store.clone(),
        Arc::clone(&catalog),
        Arc::<StubFetch>::clone(&fetch),
        fixed,
    ));
    let empty = prices.history(&owner, "").await.unwrap();
    assert_eq!(empty.pricing.totals.unwrap().requests, 0);
    settle().await;
    assert_eq!(
        fetch.count(),
        0,
        "an empty history fetches no prices or rates"
    );

    let mut old = fixture_records(&owner, "old", page_size() + 1);
    for record in &mut old {
        "2020-01-01T00:00:00.000Z".clone_into(&mut record.timestamp);
    }
    add(&store, old).await;
    // Retention lands after the calculation's first read, before its pages.
    let (retained, deleted) = tokio::join!(
        prices.history(&owner, ""),
        store.call(|db| db.set_usage_retention_days(RetentionDays::Days30)),
    );
    deleted.unwrap();
    let retained = retained.unwrap();
    assert!(retained.records.is_empty(), "{}", retained.records.len());
    assert_eq!(retained.summary.requests, 0);
    assert_eq!(
        retained.pricing.totals.unwrap().requests,
        0,
        "deletions during the calculation invalidate the partial total"
    );

    add(&store, fixture_records(&owner, "recent", page_size() + 1)).await;
    let (interrupted, ()) = tokio::join!(prices.history(&owner, ""), prices.close());
    assert!(matches!(interrupted, Err(PricingError::Closed)));
    store.close().await;
}

#[tokio::test]
async fn queued_histories_each_keep_records_summary_and_prices_from_one_snapshot() {
    let directory = TempDir::new("pricing-snapshot");
    let store = open_store(directory.path()).await;
    let owner = profile(&store, "Snapshot").await;
    let fetch = unavailable();
    let catalog = Arc::new(ModelCatalog::new(Arc::<StubFetch>::clone(&fetch), fixed));
    let prices = Arc::new(UsagePrices::new(
        store.clone(),
        Arc::clone(&catalog),
        Arc::<StubFetch>::clone(&fetch),
        fixed,
    ));
    let count = u64::from(page_size()) + 1;
    add(&store, fixture_records(&owner, "snapshot", page_size() + 1)).await;
    let mut new = fixture_record(&owner, "new");
    "2026-09-14T00:00:00Z".clone_into(&mut new.timestamp);
    // The insert queues on the store after the first history's read, before the second's.
    let (first, inserted, queued) = tokio::join!(
        prices.history(&owner, ""),
        store.call(move |db| db.add_usage(&[new])),
        prices.history(&owner, ""),
    );
    inserted.unwrap();
    for (i, result) in [first.unwrap(), queued.unwrap()].into_iter().enumerate() {
        let i = i as u64;
        let totals = result.pricing.totals.unwrap();
        assert_eq!(result.summary.requests, count + i);
        assert_eq!(totals.requests, result.summary.requests);
        assert_eq!(result.summary.total_tokens, (count + i) * 380);
        let tokens: u64 = serde_json::to_value(totals.tokens)
            .unwrap()
            .as_object()
            .unwrap()
            .values()
            .map(|n| n.as_u64().unwrap())
            .sum();
        assert_eq!(tokens, result.summary.total_tokens);
        assert_eq!(result.records.iter().any(|r| r.id == "new"), i == 1);
    }
    prices.close().await;
    store.close().await;
}

#[tokio::test]
async fn a_queued_history_runs_its_own_read_after_the_one_ahead_fails() {
    let directory = TempDir::new("pricing-failure");
    let store = open_store(directory.path()).await;
    let owner = profile(&store, "Queued failure").await;
    let fetch = unavailable();
    let catalog = Arc::new(ModelCatalog::new(Arc::<StubFetch>::clone(&fetch), fixed));
    let prices = Arc::new(UsagePrices::new(
        store.clone(),
        Arc::clone(&catalog),
        Arc::<StubFetch>::clone(&fetch),
        fixed,
    ));
    add(&store, vec![fixture_record(&owner, "fixture")]).await;
    let database = nsv_files::DataDir::new(directory.path()).database();
    let rename = move |from: &str, to: &str| {
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute_batch(&format!("ALTER TABLE {from} RENAME TO {to}"))
            .unwrap();
    };
    // The first read fails; the store renames the table back before the queued read.
    rename("usage", "usage_hidden");
    let (first, (), queued) = tokio::join!(
        prices.history(&owner, ""),
        async {
            store
                .call(move |_| {
                    rename("usage_hidden", "usage");
                    Ok(())
                })
                .await
                .unwrap();
        },
        prices.history(&owner, ""),
    );
    assert!(first.is_err());
    assert_eq!(queued.unwrap().summary.requests, 1);
    prices.close().await;
    store.close().await;
}
