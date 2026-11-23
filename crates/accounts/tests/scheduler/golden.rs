//! tests/golden/usage.json's usage queue cases and tests/golden/accounts.json's account list
//! and credential path cases, recorded from the TypeScript server. Identity, quota, reset,
//! API-account, pricing and catalog cases are replayed in tests/features.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use nsv_accounts::accounts::credential_path;
use nsv_accounts::usage::parse_page;
use nsv_domain::id::ProfileId;
use nsv_store::layout::credential_directory;
use serde_json::{Value, json};

use crate::common::{World, short};

fn cases(file: &str) -> Vec<Value> {
    let path = format!("{}/../../tests/golden/{file}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str::<Value>(&fs::read_to_string(path).unwrap())
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

fn named<'a>(cases: &'a [Value], prefix: &str) -> Vec<&'a Value> {
    cases
        .iter()
        .filter(|case| case["name"].as_str().unwrap().starts_with(prefix))
        .collect()
}

/// Every string with `from` replaced by `to`, as the recorder's placeholders.
fn placed(value: &Value, from: &str, to: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(from, to)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| placed(item, from, to)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), placed(item, from, to)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The error a case recorded: an `AppError`'s message and status; for a JavaScript runtime
/// error (`thrown`), only its status, since the message is the engine's.
fn assert_error(name: &str, recorded: &Value, error: &nsv_domain::error::AppError) {
    assert_eq!(
        u64::from(error.status),
        recorded["status"].as_u64().unwrap(),
        "{name}"
    );
    if recorded.get("thrown").is_none() {
        assert_eq!(
            error.message,
            recorded["message"].as_str().unwrap(),
            "{name}"
        );
    }
}

/// The one intentional difference: stored counters are integers, so the core's fractional
/// counters (which it never sends) lose their fraction: 1.5, 2.25, 3.75 and 0.5 ms are stored
/// as 1, 2, 3 and 0.
fn whole_counters(mut row: Value) -> Value {
    for (key, whole) in [
        ("inputTokens", 1),
        ("outputTokens", 2),
        ("totalTokens", 3),
        ("latencyMs", 0),
    ] {
        row[key] = json!(whole);
    }
    row
}

#[tokio::test(start_paused = true)]
async fn usage_queue_pages_are_stored_as_the_typescript_server_stored_them() {
    let world = World::new(short(Duration::from_secs(600))).await;
    let profile = world.profile("Usage", false, false, false).await;
    let cases = cases("usage.json");
    let queue = named(&cases, "queue ");
    let mut replayed = 0;
    for case in &queue {
        let name = case["name"].as_str().unwrap();
        let parsed = parse_page(&profile, &case["input"], 1_767_603_600_000);
        if let Some(recorded) = case.get("error") {
            assert_error(name, recorded, &parsed.unwrap_err());
            replayed += 1;
            continue;
        }
        let records = parsed.unwrap();
        let expected = placed(&case["output"], "<profile>", profile.as_str());
        assert_eq!(
            records.len() as u64,
            expected["collected"].as_u64().unwrap(),
            "{name}"
        );
        let id = profile.clone();
        let stored = world
            .store
            .call(move |db| {
                let before = db.usage_sequence()?;
                db.add_usage(&records)?;
                db.usage_after(&id, before, i64::MAX)
            })
            .await
            .unwrap()
            .records;
        let stored: Vec<Value> = stored
            .iter()
            .map(|record| serde_json::to_value(record).unwrap())
            .collect();
        let mut rows: Vec<Value> = expected["stored"].as_array().unwrap().clone();
        if name == "queue entry fractional counters" {
            rows = rows.into_iter().map(whole_counters).collect();
        }
        if name == "queue entry without timestamp uses now" {
            assert_eq!(rows[0]["timestamp"], "<now>");
            rows[0]["timestamp"] = json!("2026-01-05T09:00:00.000Z");
        }
        assert_eq!(stored, rows, "{name}");
        replayed += 1;
    }
    assert_eq!(replayed, 42);
}

#[test]
fn credential_paths_match_the_recorded_cases() {
    let cases = cases("accounts.json");
    let mut replayed = 0;
    let data = std::path::Path::new("/data");
    let profile = ProfileId::parse("00000000-0000-4000-8000-000000000001").unwrap();
    for case in named(&cases, "credentialPath ") {
        let name = case["name"].as_str().unwrap();
        let path = credential_path(data, &profile, case["input"].as_str().unwrap());
        match case.get("error") {
            Some(recorded) => assert_error(name, recorded, &path.unwrap_err()),
            None => assert_eq!(
                json!(path.unwrap().to_str().unwrap()),
                placed(
                    &placed(&case["output"], "<data>", "/data"),
                    "<profile>",
                    profile.as_str()
                ),
                "{name}"
            ),
        }
        replayed += 1;
    }
    assert_eq!(replayed, 7);
}

#[tokio::test(start_paused = true)]
async fn account_lists_are_synced_as_the_typescript_server_synced_them() {
    let cases = cases("accounts.json");
    let listing = named(&cases, "sync listing")[0];
    let world = World::new(short(Duration::from_secs(600))).await;
    let profile = world.profile("Accounts", false, false, false).await;
    world.fake.knobs(|knobs| {
        knobs.listings.insert(
            profile.clone(),
            json!({ "files": listing["input"]["listing"] }),
        );
    });
    world
        .run(world.pool().start(&profile, false))
        .await
        .unwrap();
    let auth = credential_directory(&world.data, &profile);
    for (name, content) in listing["input"]["credentialFiles"].as_object().unwrap() {
        let path = auth.join(name);
        fs::write(&path, content.as_str().unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    }
    world
        .run(world.accounts.sync(world.pool(), &profile))
        .await
        .unwrap();
    let id = profile.clone();
    let stored = world
        .store
        .call(move |db| db.account_metadata(&id))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&stored).unwrap(),
        listing["output"]["metadata"]
    );
    for (account, chatgpt) in listing["output"]["chatgptAccountIds"].as_object().unwrap() {
        assert_eq!(
            json!(world.accounts.chatgpt_account_id(&profile, account)),
            *chatgpt,
            "{account}"
        );
    }
    for entry in fs::read_dir(&auth).unwrap() {
        let mode = entry.unwrap().metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credential files are made owner-only");
    }
    let mut replayed = 1;
    for case in named(&cases, "sync ").into_iter().skip(1) {
        let name = case["name"].as_str().unwrap();
        world.fake.knobs(|knobs| {
            knobs
                .listings
                .insert(profile.clone(), case["input"].clone());
        });
        let synced = world.run(world.accounts.sync(world.pool(), &profile)).await;
        if let Some(recorded) = case.get("error") {
            assert_error(name, recorded, &synced.unwrap_err());
        } else {
            let id = profile.clone();
            let ids: Vec<String> = world
                .store
                .call(move |db| db.account_metadata(&id))
                .await
                .unwrap()
                .into_iter()
                .map(|account| account.id.as_str().to_owned())
                .collect();
            assert_eq!(json!(ids), case["output"], "{name}");
        }
        replayed += 1;
    }
    assert_eq!(replayed, 4);
}
