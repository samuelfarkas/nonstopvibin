use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::registry::RetentionDays;
use nsv_domain::secret::{ApiKey, ClientKey};
use nsv_domain::time::iso_string;
use nsv_domain::wire::{ApiAccount, Profile, UsageRecord};
use nsv_store::{Db, Store, StoreError, UsageCursor, UsageRange, Vault, VaultError};
use serde_json::json;

use crate::common::{TempDir, mode, open, owner, raw, set_mode};

const DAY: Duration = Duration::from_hours(24);

fn record(id: &str, profile: &ProfileId, timestamp: &str) -> UsageRecord {
    serde_json::from_value(json!({
        "id": id, "profileId": profile, "timestamp": timestamp, "provider": "custom",
        "model": "fixture", "account": "fixture", "inputTokens": 1, "outputTokens": 2,
        "cachedTokens": 0, "reasoningTokens": 0, "cacheWriteTokens": 0, "totalTokens": 3,
        "latencyMs": 1, "failed": false, "statusCode": 200, "stream": false,
    }))
    .unwrap()
}

fn api_account(id: &str) -> ApiAccount {
    serde_json::from_value(json!({
        "id": id, "name": "Fixture", "provider": "openai",
        "baseUrl": "https://api.example.test/v1", "prefix": "",
        "models": [{ "id": "model", "protocol": "openai" }], "disabled": false,
    }))
    .unwrap()
}

fn milliseconds(time: SystemTime) -> i64 {
    i64::try_from(
        time.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

async fn count(store: &Store, profile: &ProfileId) -> usize {
    let profile = profile.clone();
    store
        .call(move |db| Ok(db.usage(&profile, "", 10_000)?.len()))
        .await
        .unwrap()
}

#[tokio::test]
async fn existing_databases_never_receive_a_replacement_vault_key() {
    let temp = TempDir::new("missing-key");
    let directory = temp.path();
    let store = open(directory).await;
    let profile = store.call(|db| db.create_profile("Fixture")).await.unwrap();
    let id = profile.id.clone();
    let secret = store.call(move |db| db.client_key(&id)).await.unwrap();
    store.close().await;

    let path = directory.join("vault.key");
    let original = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    let missing = Vault::open(directory).unwrap_err();
    assert!(matches!(missing, VaultError::MissingKey));
    assert_eq!(
        missing.to_string(),
        "vault.key is missing for an existing database. Restore its matching key from your backup."
    );
    assert!(!path.exists());

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap()
        .write_all(&original)
        .unwrap();
    let store = Store::open(directory, Vault::open(directory).unwrap())
        .await
        .unwrap();
    let id = profile.id.clone();
    let reread = store.call(move |db| db.client_key(&id)).await.unwrap();
    assert_eq!(reread.expose_secret(), secret.expose_secret());
}

#[tokio::test]
async fn vault_keys_must_be_32_bytes_and_are_kept_owner_only() {
    let temp = TempDir::new("vault-key");
    let directory = temp.path();
    Vault::open(directory).unwrap();
    let path = directory.join("vault.key");
    assert_eq!(fs::read(&path).unwrap().len(), 32);
    assert_eq!(mode(&path), 0o600);
    set_mode(&path, 0o644);
    Vault::open(directory).unwrap();
    assert_eq!(mode(&path), 0o600);
    fs::write(&path, [7_u8; 31]).unwrap();
    let invalid = Vault::open(directory).unwrap_err();
    assert!(matches!(invalid, VaultError::InvalidKey));
    assert_eq!(
        invalid.to_string(),
        "The local vault key is invalid. Restore vault.key from your backup."
    );
}

#[tokio::test]
async fn vault_keys_are_refused_through_links_fifos_or_at_the_wrong_length() {
    let temp = TempDir::new("vault-key-unsafe");
    let directory = temp.path();
    let path = directory.join("vault.key");
    let elsewhere = directory.join("elsewhere");
    fs::write(&elsewhere, [7_u8; 32]).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
    let linked = Vault::open(directory).unwrap_err();
    assert!(matches!(linked, VaultError::UnsafeKey));
    assert_eq!(
        linked.to_string(),
        "vault.key must be an ordinary file you own, not a link. Restore vault.key from your backup."
    );
    assert_eq!(mode(&elsewhere), 0o644);

    fs::remove_file(&path).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap();
    assert!(made.success());
    assert!(matches!(
        Vault::open(directory).unwrap_err(),
        VaultError::UnsafeKey
    ));

    fs::remove_file(&path).unwrap();
    fs::write(&path, [7_u8; 33]).unwrap();
    assert!(matches!(
        Vault::open(directory).unwrap_err(),
        VaultError::InvalidKey
    ));
}

#[tokio::test]
async fn a_linked_database_is_refused() {
    let temp = TempDir::new("linked-database");
    let directory = temp.path();
    let vault = Vault::open(directory).unwrap();
    let elsewhere = directory.join("elsewhere.sqlite");
    fs::write(&elsewhere, b"").unwrap();
    std::os::unix::fs::symlink(&elsewhere, directory.join("nonstopvibin.sqlite")).unwrap();
    let refused = Store::open(directory, vault).await.err().unwrap();
    assert!(matches!(refused, StoreError::UnsafeDatabase));
    assert_eq!(
        refused.to_string(),
        "nonstopvibin.sqlite must be an ordinary file you own, without links."
    );
    assert_eq!(fs::read(&elsewhere).unwrap(), b"");
}

#[tokio::test]
async fn profile_writes_refresh_the_cached_profile_names_the_tray_reads() {
    let temp = TempDir::new("names");
    let store = open(temp.path()).await;
    let empty = store.call(Db::profile_names).await.unwrap();
    assert!(empty.is_empty());
    let profile = store.call(|db| db.create_profile("Fixture")).await.unwrap();
    let names = store.call(Db::profile_names).await.unwrap();
    assert_eq!(&*names, [(profile.id.clone(), "Fixture".to_owned())]);
    let id = profile.id.clone();
    let after_reads = store
        .call(move |db| {
            db.profiles()?;
            db.client_key(&id)?;
            db.profile_names()
        })
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&after_reads, &names), "reads keep the cache");
    let mut renamed = profile.clone();
    renamed.name = "Renamed".to_owned();
    store
        .call(move |db| db.save_profile(&renamed))
        .await
        .unwrap();
    let names = store.call(Db::profile_names).await.unwrap();
    assert_eq!(&*names, [(profile.id, "Renamed".to_owned())]);
}

#[tokio::test]
async fn secrets_persist_encrypted_under_an_owner_only_local_key() {
    let temp = TempDir::new("vault");
    let directory = temp.path().join("data");
    let store = open(&directory).await;
    let profile = store.call(|db| db.create_profile("Fixture")).await.unwrap();
    let id = profile.id.clone();
    let key = store.call(move |db| db.client_key(&id)).await.unwrap();
    assert!(ClientKey::well_formed(key.expose_secret()));

    let encrypted: String = raw(&directory)
        .query_row(
            "SELECT encrypted FROM secrets WHERE id=?",
            [format!("{}:client", profile.id)],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!encrypted.contains(key.expose_secret()));
    // nonce(12) ‖ tag(16) ‖ ciphertext of the 46-byte key.
    assert_eq!(STANDARD.decode(&encrypted).unwrap().len(), 12 + 16 + 46);
    assert_eq!(mode(&directory), 0o700);
    assert_eq!(mode(&directory.join("vault.key")), 0o600);
    assert_eq!(mode(&directory.join("nonstopvibin.sqlite")), 0o600);

    store.close().await;
    let store = open(&directory).await;
    let id = profile.id.clone();
    assert_eq!(
        store
            .call(move |db| db.client_key(&id))
            .await
            .unwrap()
            .expose_secret(),
        key.expose_secret()
    );
    store.close().await;

    fs::remove_dir_all(&directory).unwrap();
    let store = open(&directory).await;
    assert_eq!(
        store.call(|db| db.profiles()).await.unwrap(),
        Vec::<Profile>::new()
    );
    let id = profile.id.clone();
    let gone = store.call(move |db| db.client_key(&id)).await.unwrap_err();
    assert!(matches!(
        gone,
        StoreError::App(AppError { status: 404, .. })
    ));
}

#[tokio::test]
async fn usage_writes_are_atomic_idempotent_and_pruned_by_opt_in_retention() {
    let temp = TempDir::new("retention");
    let directory = temp.path();
    let store = open(directory).await;
    let first = store
        .call(|db| db.create_profile("First"))
        .await
        .unwrap()
        .id;
    let second = store
        .call(|db| db.create_profile("Second"))
        .await
        .unwrap()
        .id;
    let now = SystemTime::now();
    let at = |age: Duration| iso_string(milliseconds(now - age));
    let records: Vec<UsageRecord> = [&first, &second]
        .into_iter()
        .flat_map(|profile| {
            [5, 40].map(|days| record(&format!("{profile}:{days}"), profile, &at(DAY * days)))
        })
        .collect();
    let batch = records.clone();
    store.call(move |db| db.add_usage(&batch)).await.unwrap();
    let batch = records.clone();
    store.call(move |db| db.add_usage(&batch)).await.unwrap();
    assert_eq!(count(&store, &first).await, 2, "stored ids are skipped");

    raw(directory)
        .execute_batch(
            "CREATE TRIGGER reject_fixture BEFORE INSERT ON usage
             WHEN NEW.id='reject' BEGIN SELECT RAISE(ABORT,'synthetic rejection'); END",
        )
        .unwrap();
    let mut rolled_back = records[0].clone();
    rolled_back.id = "rolled-back".to_owned();
    let mut rejected = records[0].clone();
    rejected.id = "reject".to_owned();
    let failed = store
        .call(move |db| db.add_usage(&[rolled_back, rejected]))
        .await
        .unwrap_err();
    assert!(matches!(failed, StoreError::Sqlite(_)));
    assert_eq!(
        count(&store, &first).await,
        2,
        "a failed batch stores nothing"
    );

    store.call(move |db| db.prune_usage(now)).await.unwrap();
    assert_eq!(
        count(&store, &first).await,
        2,
        "nothing is pruned by default"
    );
    assert_eq!(
        store.call(|db| db.usage_retention_days()).await.unwrap(),
        RetentionDays::All
    );
    let generation = store.call(|db| Ok(db.usage_generation())).await.unwrap();
    store
        .call(|db| db.set_usage_retention_days(RetentionDays::Days30))
        .await
        .unwrap();
    for profile in [&first, &second] {
        let profile = profile.clone();
        let kept = store
            .call(move |db| db.usage(&profile, "", 300))
            .await
            .unwrap();
        assert_eq!(kept.len(), 1);
        assert!(kept[0].id.ends_with(":5"));
    }
    assert_eq!(
        store.call(|db| Ok(db.usage_generation())).await.unwrap(),
        generation + 2,
        "one generation per profile that lost rows"
    );
    store.close().await;

    let store = open(directory).await;
    assert_eq!(
        store.call(|db| db.usage_retention_days()).await.unwrap(),
        RetentionDays::Days30
    );
    store
        .call(move |db| db.prune_usage(now + DAY * 26))
        .await
        .unwrap();
    assert_eq!(count(&store, &first).await, 0);
    assert_eq!(count(&store, &second).await, 0);
}

#[tokio::test]
async fn pruning_keeps_rows_at_the_cutoff_and_deletes_older_ones() {
    let temp = TempDir::new("cutoff");
    let store = open(temp.path()).await;
    let profile = store
        .call(|db| db.create_profile("Cutoff"))
        .await
        .unwrap()
        .id;
    let rows = vec![
        record("older", &profile, "2026-10-24T11:59:59.999Z"),
        record("at", &profile, "2026-10-24T12:00:00.000Z"),
    ];
    store.call(move |db| db.add_usage(&rows)).await.unwrap();
    raw(temp.path())
        .execute(
            "INSERT INTO settings VALUES ('usage-retention-days','30')",
            [],
        )
        .unwrap();
    // 2026-11-23T12:00:00.000Z minus 30 days is the cutoff.
    let now = SystemTime::UNIX_EPOCH + Duration::from_hours(498_732);
    store.call(move |db| db.prune_usage(now)).await.unwrap();
    let id = profile.clone();
    let left = store.call(move |db| db.usage(&id, "", 10)).await.unwrap();
    assert_eq!(
        left.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["at"]
    );
}

#[tokio::test]
async fn unchanged_account_snapshots_do_not_write_to_sqlite() {
    let temp = TempDir::new("metadata");
    let store = open(temp.path()).await;
    let profile = store
        .call(|db| db.create_profile("Fixture"))
        .await
        .unwrap()
        .id;
    let observer = raw(temp.path());
    let version = || -> i64 {
        observer
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .unwrap()
    };
    let id = profile.clone();
    store
        .call(move |db| db.save_account_metadata(&id, &[]))
        .await
        .unwrap();
    let before = version();
    let id = profile.clone();
    store
        .call(move |db| db.save_account_metadata(&id, &[]))
        .await
        .unwrap();
    assert_eq!(version(), before, "an unchanged list is not written");
    let account = serde_json::from_value(json!({
        "id": "a.json", "name": "A", "provider": "claude", "kind": "oauth",
        "disabled": false, "status": "Ready", "priority": 0,
    }))
    .unwrap();
    let id = profile.clone();
    store
        .call(move |db| db.save_account_metadata(&id, &[account]))
        .await
        .unwrap();
    assert_ne!(version(), before, "a changed list is written");
    let id = profile.clone();
    assert_eq!(
        store
            .call(move |db| db.account_metadata(&id))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn save_api_account_rolls_back_the_account_when_its_key_cannot_be_stored() {
    let temp = TempDir::new("api-rollback");
    let store = open(temp.path()).await;
    let profile = store
        .call(|db| db.create_profile("Fixture"))
        .await
        .unwrap()
        .id;
    raw(temp.path())
        .execute_batch(
            "CREATE TRIGGER reject_key BEFORE INSERT ON secrets
             WHEN NEW.id LIKE '%:api' BEGIN SELECT RAISE(ABORT,'synthetic rejection'); END",
        )
        .unwrap();
    let id = profile.clone();
    let failed = store
        .call(move |db| {
            db.save_api_account(
                &id,
                &api_account("fixture-account"),
                Some(&ApiKey::new("synthetic-key")),
            )
        })
        .await
        .unwrap_err();
    assert!(matches!(failed, StoreError::Sqlite(_)));
    let id = profile.clone();
    assert_eq!(
        store.call(move |db| db.api_accounts(&id)).await.unwrap(),
        Vec::<ApiAccount>::new()
    );
}

#[tokio::test]
async fn profile_slug_boundaries_remain_valid_after_truncation_and_duplicate_suffixes() {
    let temp = TempDir::new("slugs");
    let store = open(temp.path()).await;
    let name = format!("{} b", "a".repeat(39));
    let first = name.clone();
    assert_eq!(
        store
            .call(move |db| db.create_profile(&first))
            .await
            .unwrap()
            .slug
            .as_str(),
        "a".repeat(39)
    );
    assert_eq!(
        store
            .call(move |db| db.create_profile(&name))
            .await
            .unwrap()
            .slug
            .as_str(),
        format!("{}-2", "a".repeat(39))
    );
    let blank = " ".repeat(41);
    assert_eq!(
        store
            .call(move |db| db.create_profile(&blank))
            .await
            .unwrap()
            .slug
            .as_str(),
        "profile"
    );
}

#[tokio::test]
async fn time_pagination_seeks_through_tied_timestamps_without_losing_rows_or_crossing_snapshot_bounds()
 {
    let temp = TempDir::new("time-pages");
    let store = open(temp.path()).await;
    let profile = store
        .call(|db| db.create_profile("Pagination"))
        .await
        .unwrap()
        .id;
    let other = store
        .call(|db| db.create_profile("Other"))
        .await
        .unwrap()
        .id;
    let rows: Vec<UsageRecord> = (0..2003)
        .map(|i| {
            record(
                &i.to_string(),
                &profile,
                if i < 1501 { "2026-01-02" } else { "2026-01-03" },
            )
        })
        .collect();
    let extra = vec![
        record("before", &profile, "2026-01-01"),
        record("other", &other, "2026-01-02"),
    ];
    let late = vec![record("later", &profile, "2026-01-02")];
    let id = profile.clone();
    let (through, ids, empty) = store
        .call(move |db| {
            db.add_usage(&rows)?;
            db.add_usage(&extra)?;
            let through = db.usage_sequence()?;
            db.add_usage(&late)?;
            let range = UsageRange {
                since: "2026-01-02",
                before: "2026-01-04",
                through,
            };
            let mut from: Option<UsageCursor> = None;
            let mut ids = Vec::new();
            loop {
                let page = db.usage_between(&id, range, from.as_ref())?;
                ids.extend(page.records.iter().map(|record| record.id.clone()));
                let Some(next) = page.next else { break };
                assert_eq!(page.records.len(), 500);
                from = Some(next);
            }
            let empty = db.usage_between(
                &id,
                UsageRange {
                    before: "2026-01-02",
                    ..range
                },
                None,
            )?;
            Ok((through, ids, empty.records.len()))
        })
        .await
        .unwrap();
    assert_eq!(through, 2005);
    assert_eq!(ids, (0..2003).map(|i| i.to_string()).collect::<Vec<_>>());
    assert_eq!(empty, 0);

    let id = profile.clone();
    let after = store
        .call(move |db| {
            let mut sequences = Vec::new();
            let mut after = 0;
            loop {
                let page = db.usage_after(&id, after, 2005)?;
                sequences.push(page.records.len());
                let Some(next) = page.next else { break };
                after = next;
            }
            Ok(sequences)
        })
        .await
        .unwrap();
    // 2004 of the profile's rows are stored up to sequence 2005; "other" is not its row.
    assert_eq!(after, [500, 500, 500, 500, 4]);
}

#[tokio::test]
async fn secret_and_setting_rows_keep_the_ids_stored_databases_already_use() {
    let temp = TempDir::new("row-ids");
    let store = open(temp.path()).await;
    let profile = store
        .call(|db| db.create_profile("Fixture"))
        .await
        .unwrap()
        .id;
    let id = profile.clone();
    let (api, request) = store
        .call(move |db| {
            db.save_api_account(&id, &api_account("account"), Some(&ApiKey::new("secret")))?;
            db.save_account_metadata(&id, &[])?;
            let request = db.reset_request_id("identity", "credit")?;
            db.set_usage_retention_days(RetentionDays::Days30)?;
            Ok((db.api_key(&AccountId::parse("account")?)?, request))
        })
        .await
        .unwrap();
    assert_eq!(api.expose_secret(), "secret");
    let ids = |sql: &str| -> Vec<String> {
        let connection = raw(temp.path());
        let mut statement = connection.prepare(sql).unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let mut secrets = vec![
        format!("{profile}:client"),
        format!("{profile}:core"),
        format!("{profile}:management"),
        "account:api".to_owned(),
    ];
    secrets.sort();
    assert_eq!(ids("SELECT id FROM secrets ORDER BY id"), secrets);
    assert_eq!(
        ids("SELECT id FROM settings ORDER BY id"),
        [
            format!("accounts:{profile}"),
            r#"codex-reset:["identity","credit"]"#.to_owned(),
            "usage-retention-days".to_owned(),
        ]
    );
    let again = store
        .call(|db| db.reset_request_id("identity", "credit"))
        .await
        .unwrap();
    assert_eq!(again, request, "a retry reuses the stored request id");
    store
        .call(|db| db.clear_reset_request("identity", "credit"))
        .await
        .unwrap();
    let fresh = store
        .call(|db| db.reset_request_id("identity", "credit"))
        .await
        .unwrap();
    assert_ne!(fresh, request);
}

#[tokio::test]
async fn the_auth_snapshot_is_current_when_a_write_returns_and_fails_closed() {
    let temp = TempDir::new("auth");
    let store = open(temp.path()).await;
    let auth = store.auth();
    let profile = store.call(|db| db.create_profile("Fixture")).await.unwrap();
    let id = profile.id.clone();
    let (client, core) = store
        .call(move |db| Ok((db.client_key(&id)?, db.core_key(&id)?)))
        .await
        .unwrap();
    {
        let snapshot = auth.borrow();
        let entry = owner(&snapshot, client.expose_secret()).unwrap().unwrap();
        assert_eq!(entry.profile, profile.id);
        assert_eq!(entry.slug, profile.slug);
        assert_eq!(entry.core_key.expose_secret(), core.expose_secret());
        assert!(owner(&snapshot, "nv_unknown").unwrap().is_none());
        assert!(
            owner(&snapshot, core.expose_secret()).unwrap().is_none(),
            "a core key is no client key"
        );
    }

    // A key the store can no longer read is refused for every profile after the next write.
    let other = store.call(|db| db.create_profile("Other")).await.unwrap();
    raw(temp.path())
        .execute(
            "UPDATE secrets SET encrypted='AAAA' WHERE id=?",
            [format!("{}:client", other.id)],
        )
        .unwrap();
    store.call(move |db| db.save_profile(&other)).await.unwrap();
    assert!(owner(&auth.borrow(), client.expose_secret()).is_err());
}

#[tokio::test]
async fn closed_stores_refuse_calls() {
    let temp = TempDir::new("closed");
    let store = open(temp.path()).await;
    let clone = store.clone();
    store.close().await;
    store.close().await;
    assert!(matches!(
        clone.call(|db| db.profiles()).await,
        Err(StoreError::Closed)
    ));
    let missing = ProfileId::parse("00000000-0000-4000-8000-000000000000").unwrap();
    let store = open(temp.path()).await;
    let lookup = store
        .call(move |db| db.profile(&missing))
        .await
        .unwrap_err();
    assert!(matches!(
        lookup,
        StoreError::App(AppError { status: 404, .. })
    ));
}
