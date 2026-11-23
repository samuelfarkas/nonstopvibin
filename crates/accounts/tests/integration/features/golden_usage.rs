//! Replays the pricing, totals and models.dev cases of `tests/golden/usage.json`
//! (`tests/golden/areas/usage.ts`). The `queue entry` and `queue page` cases record the usage
//! drain, which the pool owns.

use std::sync::Arc;

use nsv_accounts::catalog::{Catalog, CatalogError, ModelCatalog, enrich_models};
use nsv_accounts::pricing::{UsagePrices, estimate_usd, normalized_tokens};
use nsv_domain::wire::{ApiAccount, JsNumber, Model, UsageRecord};
use nsv_testkit::TempDir;
use nsv_testkit::fake::response;
use nsv_testkit::world::{StubFetch, open_store, profile};
use serde_json::{Value, json};

use super::common::{NOW_ISO, case, cases, fixed, replaced};

fn payload(cases: &[Value]) -> Vec<u8> {
    serde_json::to_vec(&case(cases, "catalog payload")["output"]).unwrap()
}

fn price_cases(cases: &[Value]) -> Vec<(&str, UsageRecord, &Value)> {
    cases
        .iter()
        .filter(|case| case["name"].as_str().unwrap().starts_with("price "))
        .map(|case| {
            // The recorder's profile id `p` is no UUID; the totals case stores them under one.
            let mut record = case["input"]["record"].clone();
            record["profileId"] = json!("00000000-0000-4000-8000-000000000000");
            let record = serde_json::from_value(record).unwrap();
            (case["name"].as_str().unwrap(), record, &case["output"])
        })
        .collect()
}

#[test]
fn each_record_prices_as_typescript_did() {
    let cases = cases("usage");
    let catalog = Catalog::parse(&payload(&cases)).unwrap();
    let priced = price_cases(&cases);
    for (name, record, expected) in &priced {
        let model = Model {
            id: record
                .upstream_model
                .clone()
                .filter(|model| !model.is_empty())
                .unwrap_or_else(|| record.model.clone()),
            owned_by: Some(record.provider.clone()),
        };
        let cost = enrich_models(&[model], &[], &catalog, NOW_ISO)
            .pop()
            .unwrap()
            .metadata
            .and_then(|metadata| metadata.cost);
        let normalized = normalized_tokens(record);
        let usd = estimate_usd(cost.as_ref(), normalized.as_ref());
        let actual = json!({ "normalized": normalized, "usd": usd.map(JsNumber) });
        assert_eq!(&actual, *expected, "{name}");
    }
    assert_eq!(priced.len(), 21);
}

#[tokio::test]
async fn period_totals_update_incrementally_as_typescript_did() {
    let cases = cases("usage");
    let directory = TempDir::new("totals");
    let store = open_store(directory.path()).await;
    let owner = profile(&store, "Priced").await;
    let records: Vec<UsageRecord> = price_cases(&cases)
        .into_iter()
        .map(|(_, mut record, _)| {
            record.id = format!("{owner}:{}", record.id);
            record.profile_id = owner.clone();
            record
        })
        .collect();
    store.call(move |db| db.add_usage(&records)).await.unwrap();
    let body = payload(&cases);
    let catalog = Arc::new(ModelCatalog::new(
        StubFetch::new(move |_| Ok(response(200, body.clone()))),
        fixed,
    ));
    catalog.models(&[], &[]).await.unwrap();
    let prices = Arc::new(UsagePrices::new(
        StubFetch::new(|_| Ok(response(503, "{}"))),
        fixed,
    ));
    let mut replayed = 0;
    // In the recorder's order: the later periods subtract from the first one's total.
    for since in [
        "2026-11-01T00:00:00.000Z",
        "2026-11-15T00:00:00.000Z",
        "2026-12-01T00:00:00.000Z",
    ] {
        let history = prices
            .history(&store, &owner, since, &catalog)
            .await
            .unwrap();
        let actual = json!({
            "summary": history.summary,
            "totals": history.pricing.totals,
            "usd": history.pricing.usd,
            "count": history.records.len(),
            "fetchedAt": history.pricing.fetched_at,
        });
        let expected = replaced(
            &case(&cases, &format!("totals since {since}"))["output"],
            &[
                ("<priced-profile>", owner.as_str()),
                ("<fetched-at>", NOW_ISO),
            ],
        );
        assert_eq!(actual, expected, "since {since}");
        replayed += 1;
    }
    assert_eq!(replayed, 3);
    prices.close().await;
    store.close().await;
}

#[test]
fn models_match_one_catalog_entry_as_typescript_did() {
    let cases = cases("usage");
    let catalog = Catalog::parse(&payload(&cases)).unwrap();
    let mut replayed = 0;
    for case in cases
        .iter()
        .filter(|case| case["name"].as_str().unwrap().starts_with("enrich "))
    {
        let models: Vec<Model> = serde_json::from_value(case["input"]["models"].clone()).unwrap();
        let accounts: Vec<ApiAccount> =
            serde_json::from_value(case["input"]["accounts"].clone()).unwrap();
        let actual =
            serde_json::to_value(enrich_models(&models, &accounts, &catalog, NOW_ISO)).unwrap();
        assert_eq!(
            actual,
            replaced(&case["output"], &[("<fetched-at>", NOW_ISO)]),
            "{}",
            case["name"]
        );
        replayed += 1;
    }
    assert_eq!(replayed, 9);
}

#[tokio::test]
async fn a_malformed_catalog_is_a_502() {
    let cases = cases("usage");
    let golden = case(&cases, "catalog malformed payload");
    let body = golden["input"].to_string();
    let catalog = ModelCatalog::new(
        StubFetch::new(move |_| Ok(response(200, body.clone()))),
        fixed,
    );
    let model = Model {
        id: "x".to_owned(),
        owned_by: None,
    };
    let Err(CatalogError::App(error)) = catalog.models(&[model], &[]).await else {
        panic!("a malformed catalog was accepted");
    };
    assert_eq!(error.message, golden["error"]["message"]);
    assert_eq!(u64::from(error.status), golden["error"]["status"]);
}
