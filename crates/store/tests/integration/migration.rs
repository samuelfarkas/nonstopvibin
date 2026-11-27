//! A data directory the TypeScript server wrote (`tests/fixtures/ts-data-dir`) opens unchanged,
//! and one this store writes keeps the TypeScript format. With `NSV_KEEP_DIR=<dir>` the test
//! leaves `<dir>/data` and `<dir>/expected.json` for `tests/downgrade/read-with-ts.ts`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::registry::RetentionDays;
use nsv_domain::secret::{ApiKey, ClientKey};
use nsv_domain::time::iso_string;
use nsv_domain::wire::{Account, ApiAccount, Profile, Quota, UsageRecord};
use nsv_store::{Db, Store, StoreError};
use nsv_testkit::{TempDir, mode, set_mode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::common::{open, owner};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/ts-data-dir"
);
const FIRST: &str = "6e4156ab-2acf-4d89-9a81-264f3b3da50a";
const PERSONAL: &str = "d120c6b8-8067-4659-8593-37de2c72d1cf";
const THIRD: &str = "6a4fc666-ed37-4a2e-a0cc-8710745c83b0";
/// The store-visible keys of `expected.json`.
const KEYS: [&str; 8] = [
    "profiles",
    "secrets",
    "apiAccounts",
    "accountMetadata",
    "quotas",
    "usage",
    "retentionDays",
    "resetRequest",
];

fn profile(id: &str) -> ProfileId {
    ProfileId::parse(id).unwrap()
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A git checkout keeps neither the fixture's modes nor its empty directories (two profiles'
/// `auth/`); re-create them as the TypeScript build left them.
fn copy_fixture(root: &Path, expected: &Value) -> PathBuf {
    let data = root.join("data");
    copy_tree(&Path::new(FIXTURE).join("data"), &data);
    set_mode(&data, 0o700);
    for file in expected["files"].as_array().unwrap() {
        let path = file["path"].as_str().unwrap();
        if path.starts_with("data/") {
            if file["type"] == "directory" {
                fs::create_dir_all(root.join(path)).unwrap();
            }
            let mode = u32::from_str_radix(file["mode"].as_str().unwrap(), 8).unwrap();
            set_mode(&root.join(path), mode);
        }
    }
    data
}

fn to_value(value: impl Serialize) -> Value {
    serde_json::to_value(value).unwrap()
}

fn profile_keys(expected: &Value, key: &str) -> Vec<ProfileId> {
    expected[key]
        .as_object()
        .unwrap()
        .keys()
        .map(|id| profile(id))
        .collect()
}

/// What the store returns for everything `expected` names, in `expected.json`'s shape.
fn observe(db: &mut Db, expected: &Value) -> Result<Value, StoreError> {
    let mut secrets = Map::new();
    for id in expected["secrets"].as_object().unwrap().keys() {
        let (owner, kind) = id.rsplit_once(':').unwrap();
        let plaintext = match kind {
            "client" => db
                .client_key(&ProfileId::parse(owner)?)?
                .expose_secret()
                .to_owned(),
            "core" => db
                .core_key(&ProfileId::parse(owner)?)?
                .expose_secret()
                .to_owned(),
            "management" => db
                .management_key(&ProfileId::parse(owner)?)?
                .expose_secret()
                .to_owned(),
            "hub" => db
                .hub_key(&ProfileId::parse(owner)?)?
                .map_or_else(String::new, |key| key.expose_secret().to_owned()),
            "api" => db
                .api_key(&AccountId::parse(owner)?)?
                .expose_secret()
                .to_owned(),
            other => panic!("unexpected secret kind {other}"),
        };
        secrets.insert(id.clone(), plaintext.into());
    }
    let mut api_accounts = Map::new();
    for id in profile_keys(expected, "apiAccounts") {
        api_accounts.insert(id.to_string(), to_value(db.api_accounts(&id)?));
    }
    let mut account_metadata = Map::new();
    for id in profile_keys(expected, "accountMetadata") {
        account_metadata.insert(id.to_string(), to_value(db.account_metadata(&id)?));
    }
    let mut quotas = Vec::new();
    for entry in expected["quotas"].as_array().unwrap() {
        let (owner, account) = (
            ProfileId::parse(entry["profileId"].as_str().unwrap())?,
            AccountId::parse(entry["accountId"].as_str().unwrap())?,
        );
        let quota = db.quota(&owner, &account)?;
        quotas.push(json!({ "profileId": owner, "accountId": account, "quota": quota }));
    }
    let mut usage = Map::new();
    for id in profile_keys(expected, "usage") {
        let mut records = Vec::new();
        for entry in expected["usage"][id.as_str()]["records"]
            .as_array()
            .unwrap()
        {
            let sequence = entry["sequence"].as_i64().unwrap();
            let page = db.usage_after(&id, sequence - 1, sequence)?;
            records.push(json!({ "sequence": sequence, "record": page.records.first() }));
        }
        let count = db.usage(&id, "", u32::MAX)?.len();
        usage.insert(
            id.to_string(),
            json!({ "count": count, "records": records }),
        );
    }
    let reset = &expected["resetRequest"];
    let request_id = db.reset_request_id(
        reset["identity"].as_str().unwrap(),
        reset["creditId"].as_str().unwrap(),
    )?;
    Ok(json!({
        "profiles": db.profiles()?,
        "secrets": secrets,
        "apiAccounts": api_accounts,
        "accountMetadata": account_metadata,
        "quotas": quotas,
        "usage": usage,
        "retentionDays": db.usage_retention_days()?.days(),
        "resetRequest": {
            "identity": reset["identity"],
            "creditId": reset["creditId"],
            "settingId": reset["settingId"],
            "requestId": request_id,
        },
    }))
}

/// Every stored body of type `T` is exactly what this store writes for the value it reads.
fn assert_bodies_round_trip<T: Serialize + DeserializeOwned>(
    raw: &rusqlite::Connection,
    sql: &str,
) {
    let mut statement = raw.prepare(sql).unwrap();
    let bodies: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(!bodies.is_empty(), "{sql} has rows");
    for body in bodies {
        // Launch deletes Claude sign-ins' accounts, so no build writes those back.
        let body = match serde_json::from_str(&body).unwrap() {
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .filter(|item| item["provider"] != "claude")
                    .collect(),
            )
            .to_string(),
            _ => body,
        };
        let value: T = serde_json::from_str(&body).unwrap();
        assert_eq!(serde_json::to_string(&value).unwrap(), body, "{sql}");
    }
}

async fn check(store: &Store, data: &Path, expected: &Value) {
    let wanted = expected.clone();
    let observed = store.call(move |db| observe(db, &wanted)).await.unwrap();
    // Model-scoped windows were Claude sign-ins' only, and this build reads none.
    let mut expected = expected.clone();
    for row in expected["quotas"].as_array_mut().unwrap() {
        for window in row["quota"]["windows"].as_array_mut().unwrap() {
            window.as_object_mut().unwrap().shift_remove("scoped");
        }
    }
    let expected = &expected;
    for key in KEYS {
        assert_eq!(observed[key], expected[key], "{key}");
    }

    for (id, usage) in expected["usage"].as_object().unwrap() {
        let mut records: Vec<&Value> = usage["records"].as_array().unwrap().iter().collect();
        records.sort_by_key(|entry| entry["sequence"].as_i64());
        let wanted: Vec<Value> = records
            .iter()
            .map(|entry| entry["record"].clone())
            .collect();
        let id = profile(id);
        let listed = store
            .call(move |db| {
                let (mut after, through, mut listed) = (0, db.usage_sequence()?, Vec::new());
                loop {
                    let page = db.usage_after(&id, after, through)?;
                    listed.extend(page.records);
                    let Some(next) = page.next else { break };
                    after = next;
                }
                Ok(listed)
            })
            .await
            .unwrap();
        assert_eq!(to_value(listed), Value::from(wanted));
    }

    let auth = store.auth();
    for entry in expected["profiles"].as_array().unwrap() {
        let id = entry["id"].as_str().unwrap();
        let client = expected["secrets"][format!("{id}:client")]
            .as_str()
            .unwrap();
        let snapshot = auth.borrow();
        let found = owner(&snapshot, client).unwrap().unwrap();
        assert_eq!(found.profile.as_str(), id);
        assert_eq!(found.slug.as_str(), entry["slug"].as_str().unwrap());
        assert_eq!(
            found.core_key.expose_secret(),
            expected["secrets"][format!("{id}:core")].as_str().unwrap()
        );
    }

    let raw = rusqlite::Connection::open(data.join("nonstopvibin.sqlite")).unwrap();
    let mut secret_ids: Vec<&String> = expected["secrets"].as_object().unwrap().keys().collect();
    secret_ids.sort();
    let mut statement = raw.prepare("SELECT id FROM secrets ORDER BY id").unwrap();
    let stored: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(stored.iter().collect::<Vec<_>>(), secret_ids);
    let request: String = raw
        .query_row(
            "SELECT value FROM settings WHERE id=?",
            [expected["resetRequest"]["settingId"].as_str().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        request,
        expected["resetRequest"]["requestId"].as_str().unwrap()
    );
    assert_bodies_round_trip::<Profile>(&raw, "SELECT body FROM profiles");
    assert_bodies_round_trip::<ApiAccount>(&raw, "SELECT body FROM api_accounts");
    assert_bodies_round_trip::<Quota>(
        &raw,
        "SELECT body FROM quotas WHERE account_id NOT LIKE 'claude-%'",
    );
    assert_bodies_round_trip::<UsageRecord>(&raw, "SELECT body FROM usage");
    assert_bodies_round_trip::<Vec<Account>>(
        &raw,
        "SELECT value FROM settings WHERE id LIKE 'accounts:%'",
    );
    let journal: String = raw
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "wal");

    assert_eq!(mode(data), 0o700);
    assert_eq!(mode(&data.join("nonstopvibin.sqlite")), 0o600);
    assert_eq!(mode(&data.join("vault.key")), 0o600);
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario: each write next to what a reader must then find"
)]
async fn reads_a_typescript_data_directory_and_writes_one_in_the_same_format() {
    let expected: Value =
        serde_json::from_str(&fs::read_to_string(format!("{FIXTURE}/expected.json")).unwrap())
            .unwrap();
    let temp = TempDir::new("migration");
    let root = match std::env::var_os("NSV_KEEP_DIR") {
        Some(keep) => {
            let root = PathBuf::from(keep);
            let _ = fs::remove_dir_all(root.join("data"));
            fs::create_dir_all(&root).unwrap();
            root
        }
        None => temp.path().to_owned(),
    };
    let data = copy_fixture(&root, &expected);

    let store = open(&data).await;
    let hex = expected["vaultKeyHex"].as_str().unwrap();
    let key: Vec<u8> = hex
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    assert_eq!(
        fs::read(data.join("vault.key")).unwrap(),
        key,
        "the key is never replaced"
    );
    check(&store, &data, &expected).await;

    // Write through every operation, recording what a reader must then find.
    let mut after = expected.clone();
    let before = iso_string(millis(SystemTime::now()));
    let created = store
        .call(|db| db.create_profile("Rust Écrit ✓"))
        .await
        .unwrap();
    let created_at = created.created_at.clone();
    assert!(before <= created_at && created_at <= iso_string(millis(SystemTime::now())));
    let created_json = to_value(&created);
    assert_eq!(
        created_json,
        json!({
            "id": created.id, "slug": "rust-ecrit", "name": "Rust Écrit ✓", "strategy": "fill-first",
            "sessionAffinity": true, "enabled": false, "createdAt": created_at,
        })
    );
    let new = created.id.clone();
    let id = new.clone();
    let (client, core, management, hub) = store
        .call(move |db| {
            Ok((
                db.client_key(&id)?,
                db.core_key(&id)?,
                db.management_key(&id)?,
                db.hub_key(&id)?,
            ))
        })
        .await
        .unwrap();
    let hub = hub.expect("a new profile has a usage hub key");
    assert!(ClientKey::well_formed(client.expose_secret()));
    for key in [core.expose_secret(), management.expose_secret()] {
        assert_eq!(key.len(), 43);
        assert!(
            key.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }
    after["profiles"].as_array_mut().unwrap().push(created_json);
    let secrets = after["secrets"].as_object_mut().unwrap();
    secrets.insert(format!("{new}:client"), client.expose_secret().into());
    secrets.insert(format!("{new}:core"), core.expose_secret().into());
    secrets.insert(
        format!("{new}:management"),
        management.expose_secret().into(),
    );
    secrets.insert(format!("{new}:hub"), hub.expose_secret().into());

    store
        .call(|db| {
            let mut personal = db.profile(&profile(PERSONAL))?;
            personal.name = "Personal (renamed)".to_owned();
            personal.enabled = true;
            db.save_profile(&personal)
        })
        .await
        .unwrap();
    after["profiles"][1]["name"] = "Personal (renamed)".into();
    after["profiles"][1]["enabled"] = true.into();

    let account_id = "0f1e2d3c-4b5a-4968-8776-655443322110";
    let account = json!({
        "id": account_id, "name": "Rust Gateway", "provider": "openai",
        "baseUrl": "https://api.example.test/v1", "prefix": "rs",
        "models": [{ "id": "gpt-rust", "protocol": "openai" }, { "id": "claude-rust", "protocol": "anthropic" }],
        "disabled": false,
    });
    let api_key = "sk-rust-written-ünïcødé";
    let (id, typed) = (
        new.clone(),
        serde_json::from_value::<ApiAccount>(account.clone()).unwrap(),
    );
    store
        .call(move |db| db.save_api_account(&id, &typed, Some(&ApiKey::new(api_key))))
        .await
        .unwrap();
    after["apiAccounts"][new.as_str()] = json!([account]);
    after["secrets"][format!("{account_id}:api")] = api_key.into();

    // Replacing an account without a key keeps its key and moves it last.
    store
        .call(|db| {
            let id = profile(THIRD);
            let mut renamed = db.api_accounts(&id)?.remove(0);
            renamed.name = "OpenCode Go (renamed)".to_owned();
            db.save_api_account(&id, &renamed, None)
        })
        .await
        .unwrap();
    let third = after["apiAccounts"][THIRD].as_array_mut().unwrap();
    let mut renamed = third.remove(0);
    renamed["name"] = "OpenCode Go (renamed)".into();
    third.push(renamed);

    let removed = "60ede06a-58e3-4481-8f27-4961587f1a96";
    store
        .call(|db| db.remove_api_account(&profile(FIRST), &AccountId::parse(removed)?))
        .await
        .unwrap();
    after["apiAccounts"][FIRST]
        .as_array_mut()
        .unwrap()
        .retain(|a| a["id"] != removed);
    after["secrets"]
        .as_object_mut()
        .unwrap()
        .remove(&format!("{removed}:api"));
    after["quotas"]
        .as_array_mut()
        .unwrap()
        .retain(|q| q["accountId"] != removed);

    let metadata = json!([{
        "id": "claude-rust-1.json", "name": "Rust Claude", "accountUuid": "eeeeeeee-0000-4000-8000-000000000005",
        "provider": "claude", "kind": "oauth", "email": "rust@example.com", "disabled": false,
        "status": "Ready", "priority": 3, "authIndex": "000000000000000a",
    }]);
    let (id, typed) = (
        new.clone(),
        serde_json::from_value::<Vec<Account>>(metadata.clone()).unwrap(),
    );
    store
        .call(move |db| {
            db.save_account_metadata(&id, &typed)?;
            let first = profile(FIRST);
            let mut accounts = db.account_metadata(&first)?;
            accounts[0].priority = 5;
            db.save_account_metadata(&first, &accounts)
        })
        .await
        .unwrap();
    after["accountMetadata"][new.as_str()] = metadata;
    after["accountMetadata"][FIRST][0]["priority"] = 5.into();

    let new_quota = json!({
        "status": "available",
        "windows": [
            { "label": "Monthly", "remainingPercent": 12.5, "resetsAt": null },
            { "label": "Daily", "remainingPercent": 100, "resetsAt": "2026-11-24T00:00:00.000Z" },
        ],
        "checkedAt": "2026-11-23T10:00:00.000Z",
        "plan": "pro",
    });
    let failed_quota = json!({
        "status": "error", "windows": [], "checkedAt": "2026-11-23T11:00:00.000Z", "error": "Synthetic failure",
    });
    let replaced = "claude-6790977a-c183-44f7-bc9b-49a9585cb4c4.json";
    let (id, first, second) = (
        new.clone(),
        serde_json::from_value::<Quota>(new_quota.clone()).unwrap(),
        serde_json::from_value::<Quota>(failed_quota.clone()).unwrap(),
    );
    store
        .call(move |db| {
            db.save_quota(&id, &AccountId::parse(account_id)?, &first)?;
            db.save_quota(&profile(THIRD), &AccountId::parse(replaced)?, &second)
        })
        .await
        .unwrap();
    let quotas = after["quotas"].as_array_mut().unwrap();
    quotas.push(json!({ "profileId": new, "accountId": account_id, "quota": new_quota }));
    for quota in quotas.iter_mut().filter(|q| q["accountId"] == replaced) {
        quota["quota"] = failed_quota.clone();
    }

    let usage = [
        json!({
            "id": format!("{new}:rust-0001"), "profileId": new, "timestamp": "2026-11-20T08:00:00.000Z",
            "provider": "openai", "model": "rs/gpt-rust", "upstreamModel": "gpt-rust-2026", "pricingTokens": null,
            "account": "", "inputTokens": 10, "outputTokens": 5, "cachedTokens": 0, "reasoningTokens": 1,
            "cacheWriteTokens": 0, "totalTokens": 15, "latencyMs": 321, "failed": true, "statusCode": 429,
            "stream": false,
        }),
        json!({
            "id": format!("{new}:rust-0002"), "profileId": new, "timestamp": "2026-11-21T08:00:00.123456789Z",
            "provider": "openai", "model": "rs/gpt-rust",
            "pricingTokens": { "input": 7, "cached": 1, "cacheWrite": 0, "output": 3, "reasoning": 0 },
            "account": "000000000000000b", "inputTokens": 8, "outputTokens": 3, "cachedTokens": 1,
            "reasoningTokens": 0, "cacheWriteTokens": 0, "totalTokens": 11, "latencyMs": 1200, "failed": false,
            "statusCode": 200, "stream": true,
        }),
        // Already stored: skipped.
        expected["usage"][FIRST]["records"][0]["record"].clone(),
    ];
    let typed: Vec<UsageRecord> = usage
        .iter()
        .map(|r| serde_json::from_value(r.clone()).unwrap())
        .collect();
    store.call(move |db| db.add_usage(&typed)).await.unwrap();
    after["usage"][new.as_str()] = json!({
        "count": 2,
        "records": [{ "sequence": 13, "record": usage[0] }, { "sequence": 14, "record": usage[1] }],
    });

    let identity = "aaaaaaaa-1111-4222-8333-444444444444";
    let request_id = store
        .call(move |db| {
            db.set_usage_retention_days(RetentionDays::All)?;
            db.clear_reset_request("11111111-2222-4333-8444-555555555555", "credit-synthetic-1")?;
            db.reset_request_id(identity, "credit-rust-1")
        })
        .await
        .unwrap();
    after["retentionDays"] = 0.into();
    after["resetRequest"] = json!({
        "identity": identity,
        "creditId": "credit-rust-1",
        "settingId": r#"codex-reset:["aaaaaaaa-1111-4222-8333-444444444444","credit-rust-1"]"#,
        "requestId": request_id,
    });

    check(&store, &data, &after).await;
    store.close().await;
    let store = open(&data).await;
    check(&store, &data, &after).await;
    store.close().await;

    if std::env::var_os("NSV_KEEP_DIR").is_some() {
        fs::write(
            root.join("expected.json"),
            serde_json::to_string_pretty(&after).unwrap(),
        )
        .unwrap();
    }
}

#[tokio::test]
async fn a_new_database_has_the_typescript_schema() {
    let expected: Value =
        serde_json::from_str(&fs::read_to_string(format!("{FIXTURE}/expected.json")).unwrap())
            .unwrap();
    let temp = TempDir::new("schema");
    let store = open(temp.path()).await;
    store.close().await;
    let raw = rusqlite::Connection::open(temp.path().join("nonstopvibin.sqlite")).unwrap();
    let mut statement = raw
        .prepare("SELECT type, name, sql FROM sqlite_master WHERE sql NOT NULL ORDER BY rowid")
        .unwrap();
    let tables: Vec<Value> = statement
        .query_map([], |row| {
            Ok(json!({ "type": row.get::<_, String>(0)?, "name": row.get::<_, String>(1)?, "sql": row.get::<_, String>(2)? }))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(Value::from(tables), expected["tables"]);
}

fn millis(time: SystemTime) -> i64 {
    i64::try_from(
        time.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}
