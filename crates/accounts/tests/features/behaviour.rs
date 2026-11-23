//! Server-side behaviours of `tests/model-catalog.test.ts`, `usage-pricing.test.ts`,
//! `quota.test.ts` and `reset-credits.test.ts` beyond the goldens.

use std::future::{Future, ready};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use nsv_accounts::api_accounts::api_account;
use nsv_accounts::catalog::{CatalogError, ModelCatalog};
use nsv_accounts::pricing::{PricingError, UsagePrices};
use nsv_accounts::quota::{QuotaError, Quotas, RefreshQuota, parse_quota};
use nsv_accounts::resets::CodexResets;
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::registry::{Protocol, RetentionDays};
use nsv_domain::secret::ApiKey;
use nsv_domain::wire::{
    Account, ApiAccount, ApiAccountInput, Model, PricingTokens, Quota, QuotaStatus, ResetResult,
    UsageRecord,
};
use nsv_net::Loopback;
use nsv_store::Store;
use serde_json::{Value, json};

use crate::common::{NOW, StubFetch, StubSource, TempDir, fixed, open_store, profile, response};

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
    let catalog = ModelCatalog::new(Arc::clone(&fetch), now);
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
    let catalog = ModelCatalog::new(Arc::clone(&fetch), fixed);
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
    let catalog = Arc::new(ModelCatalog::new(Arc::clone(&fetch), now));
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
    assert_eq!(snapshot.cost("claude", "m").unwrap().unwrap().input.0, 1.0);
    assert_eq!(fetch.count(), 2);
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
    let catalog = Arc::new(ModelCatalog::new(Arc::clone(&fetch), now));
    catalog.models(&[], &[]).await.unwrap();
    let prices = Arc::new(UsagePrices::new(
        StubFetch::new(|_| Ok(response(503, ""))),
        fixed,
    ));
    let total = async |since: &str| {
        let history = prices
            .history(&store, &owner, since, &catalog)
            .await
            .unwrap();
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
        prices
            .history(&store, &owner, "2000-01-01T00:00:00.000Z", &catalog)
            .await,
        Err(PricingError::Closed)
    ));
    store.close().await;
}

#[tokio::test]
async fn unknown_prices_and_incomplete_breakdowns_are_missing_not_free() {
    let directory = TempDir::new("missing-prices");
    let store = open_store(directory.path()).await;
    let owner = profile(&store, "Owner").await;
    let catalog = Arc::new(ModelCatalog::new(
        StubFetch::new(|_| Ok(response(200, priced_catalog(1.0)))),
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
        StubFetch::new(|_| Ok(response(503, ""))),
        fixed,
    ));
    let history = prices
        .history(&store, &owner, "2026-11-01T00:00:00.000Z", &catalog)
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
        Arc::clone(&fetch),
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
        Arc::clone(&fetch),
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
    let resets = CodexResets::new(store.clone());
    let source = reset_source(None);
    let available = Refresh(Ok(QuotaStatus::Available));
    let consume = async |answer: Value, quotas: &Refresh| {
        source.queue([answer]);
        let result = resets
            .consume(&source, quotas, &owner, &account, "credit-one")
            .await;
        (result, redeem_request_id(&source.take_calls()))
    };

    let (failed, first) = consume(
        json!({ "status_code": 503, "body": "sensitive upstream error" }),
        &available,
    )
    .await;
    let Err(QuotaError::App(error)) = failed else {
        panic!("an upstream failure succeeded")
    };
    assert_eq!(error.status, 502);
    assert!(!error.message.contains("sensitive"));
    let (reset, retried) = consume(
        json!({ "status_code": 200, "body": "{\"code\":\"reset\"}" }),
        &available,
    )
    .await;
    assert_eq!(
        serde_json::to_value(reset.unwrap()).unwrap(),
        json!({ "code": "reset", "quotaRefreshed": true })
    );
    assert_eq!(retried, first, "a retry redeems with the same request id");
    let (redeemed, again) = consume(
        json!({ "status_code": 200, "body": { "code": "already_redeemed" } }),
        &Refresh(Err(())),
    )
    .await;
    assert_eq!(
        serde_json::to_value(redeemed.unwrap()).unwrap(),
        json!({ "code": "already_redeemed", "quotaRefreshed": false })
    );
    assert_eq!(again, first);
    let (unknown, kept) = consume(
        json!({ "status_code": 200, "body": { "code": "unexpected" } }),
        &available,
    )
    .await;
    assert!(matches!(
        unknown,
        Err(QuotaError::App(AppError { status: 502, .. }))
    ));
    assert_eq!(kept, first, "an unknown outcome keeps the request");
    let (nothing, last) = consume(
        json!({ "status_code": 200, "body": { "code": "nothing_to_reset" } }),
        &available,
    )
    .await;
    assert!(nothing.is_ok());
    assert_eq!(last, first);
    let (_, fresh) = consume(
        json!({ "status_code": 200, "body": { "code": "no_credit" } }),
        &available,
    )
    .await;
    assert_ne!(fresh, first, "nothing redeemed clears the request");

    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let gated = reset_source(Some(Arc::clone(&gate)));
    gated.queue([json!({ "status_code": 200, "body": { "code": "reset" } })]);
    let (running, concurrent) = tokio::join!(
        resets.consume(&gated, &available, &owner, &account, "credit-one"),
        async {
            called(&gated, 1).await;
            let concurrent = resets
                .consume(&gated, &available, &owner, &account, "credit-one")
                .await;
            gate.add_permits(10);
            concurrent
        }
    );
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

    let stopped = StubSource {
        stopped: true,
        ..reset_source(None)
    };
    let Err(QuotaError::App(error)) = resets.credits(&stopped, &owner, &account).await else {
        panic!()
    };
    assert_eq!(
        (error.status, error.message.as_str()),
        (409, "Start this profile first.")
    );
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
