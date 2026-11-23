//! The core config: tests/golden/config.json replayed (the YAML read back must equal the
//! golden `parsed`, the JSON the golden `returned`), `applied`, and the file the store writes.

use std::fs;
use std::os::unix::fs::MetadataExt;

use nsv_core::config::{CoreConfiguration, applied, write_config};
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::secret::{ApiKey, CoreKey, ManagementKey};
use nsv_domain::wire::{ApiAccount, Profile};
use nsv_files::DataDir;
use nsv_store::{Store, StoreError, Vault};
use nsv_testkit::TempDir;
use serde_json::{Map, Value, json};

use crate::common::mode;

// ---- A reader for the emitter's YAML subset ----
// Block mappings and sequences, two spaces per level, and JSON scalars: what the emitter
// writes. The real core reads the same files in tests/core.rs.

/// Lines as (indent, content), with `- "key": value` split into `-` and the entry below it.
fn lines(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut content = line.trim_start_matches(' ');
        let mut indent = line.len() - content.len();
        while let Some(rest) = content.strip_prefix("- ") {
            if !is_entry(rest) && !rest.starts_with("- ") {
                break;
            }
            out.push((indent, "-".to_owned()));
            indent += 2;
            content = rest;
        }
        out.push((indent, content.to_owned()));
    }
    out
}

/// A `"key": ...` line: a JSON string followed by a colon.
fn split_entry(content: &str) -> Option<(String, &str)> {
    let mut stream = serde_json::Deserializer::from_str(content).into_iter::<String>();
    let key = stream.next()?.ok()?;
    let rest = content.get(stream.byte_offset()..)?.strip_prefix(':')?;
    Some((key, rest.trim_start()))
}

fn is_entry(content: &str) -> bool {
    content.starts_with('"') && split_entry(content).is_some()
}

fn block(lines: &[(usize, String)], at: &mut usize, indent: usize) -> Value {
    let sequence = lines[*at].1 == "-" || lines[*at].1.starts_with("- ");
    if sequence {
        let mut items = Vec::new();
        while *at < lines.len()
            && lines[*at].0 == indent
            && (lines[*at].1 == "-" || lines[*at].1.starts_with("- "))
        {
            let content = lines[*at].1.clone();
            *at += 1;
            items.push(match content.strip_prefix("- ") {
                Some(scalar) => serde_json::from_str(scalar).unwrap(),
                None => block(lines, at, indent + 2),
            });
        }
        Value::Array(items)
    } else {
        let mut map = Map::new();
        while *at < lines.len() && lines[*at].0 == indent && is_entry(&lines[*at].1) {
            let content = lines[*at].1.clone();
            let (key, rest) = split_entry(&content).unwrap();
            *at += 1;
            let value = if rest.is_empty() {
                block(lines, at, indent + 2)
            } else {
                serde_json::from_str(rest).unwrap()
            };
            assert!(map.insert(key, value).is_none(), "a repeated key");
        }
        Value::Object(map)
    }
}

pub(crate) fn read_yaml(text: &str) -> Value {
    let lines = lines(text);
    let mut at = 0;
    let value = block(&lines, &mut at, 0);
    assert_eq!(
        at,
        lines.len(),
        "unread lines from {}",
        lines[at.min(lines.len() - 1)].1
    );
    value
}

// ---- The golden cases ----

fn profile(strategy: &str, session_affinity: bool) -> Profile {
    serde_json::from_value(json!({
        "id": "00000000-0000-4000-8000-000000000001",
        "slug": "config",
        "name": "Config",
        "strategy": strategy,
        "sessionAffinity": session_affinity,
        "enabled": true,
        "createdAt": "2026-11-22T12:00:00.000Z",
    }))
    .unwrap()
}

/// `goModelProtocol` of api-accounts.ts for model ids without an endpoint: the golden inputs
/// are `ApiAccountInput`s, which `apiAccount()` turned into stored accounts.
fn go_protocol(id: &str) -> &'static str {
    if ["gpt-", "grok-", "muse-"].iter().any(|p| id.starts_with(p)) {
        "responses"
    } else if ["minimax-", "qwen"].iter().any(|p| id.starts_with(p)) {
        "anthropic"
    } else {
        "openai"
    }
}

/// The stored account and key an `ApiAccountInput` of the golden file becomes.
fn account(index: usize, input: &Value) -> (ApiAccount, ApiKey) {
    let provider = input["provider"].as_str().unwrap();
    let models: Vec<Value> = input["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let id = m["id"].as_str().unwrap();
            let protocol = if provider == "opencode-go" {
                go_protocol(id)
            } else {
                m["protocol"].as_str().unwrap()
            };
            json!({ "id": id, "protocol": protocol })
        })
        .collect();
    let base_url = input["baseUrl"].as_str().unwrap();
    let stored = json!({
        "id": format!("account-{index}"),
        "name": input["name"],
        "provider": provider,
        "baseUrl": base_url.strip_suffix('/').unwrap_or(base_url),
        "prefix": input["prefix"],
        "models": models,
        "disabled": input.get("disabled").and_then(Value::as_bool).unwrap_or(false),
    });
    (
        serde_json::from_value(stored).unwrap(),
        ApiKey::new(input["apiKey"].as_str().unwrap()),
    )
}

pub(crate) fn configuration(
    profile: &Profile,
    port: u16,
    keys: (&str, &str),
    accounts: &[(ApiAccount, ApiKey)],
    auth_dir: &str,
) -> CoreConfiguration {
    let stored: Vec<ApiAccount> = accounts
        .iter()
        .map(|(account, _)| account.clone())
        .collect();
    CoreConfiguration::new(
        profile,
        port,
        auth_dir.to_owned(),
        (CoreKey::new(keys.0), ManagementKey::new(keys.1)),
        &stored,
        |id: &AccountId| {
            Ok(accounts
                .iter()
                .find(|(account, _)| &account.id == id)
                .map(|(_, key)| key.clone())
                .expect("a key per account"))
        },
    )
    .unwrap()
}

pub(crate) fn golden_config_cases() -> Vec<(String, CoreConfiguration, Value)> {
    let cases = golden();
    let mut out = Vec::new();
    for (index, case) in cases
        .iter()
        .take_while(|case| case["name"] != "config unknown profile")
        .enumerate()
    {
        let input = &case["input"];
        let accounts: Vec<(ApiAccount, ApiKey)> = input["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(n, spec)| account(n, spec))
            .collect();
        let config = configuration(
            &profile(
                input["profile"]["strategy"].as_str().unwrap(),
                input["profile"]["sessionAffinity"].as_bool().unwrap(),
            ),
            u16::try_from(input["port"].as_u64().unwrap()).unwrap(),
            (
                &format!("core-key-{index}"),
                &format!("management-key-{index}"),
            ),
            &accounts,
            "<data>/profiles/<profile>/auth",
        );
        out.push((
            case["name"].as_str().unwrap().to_owned(),
            config,
            case["output"].clone(),
        ));
    }
    out
}

fn golden() -> Vec<Value> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/config.json"
    );
    let text = fs::read_to_string(path).unwrap();
    // The golden was recorded at version 0.2.0; the user agent follows the workspace version.
    let text = text.replace("nonstopvibin/0.2.0", nsv_net::USER_AGENT);
    serde_json::from_str(&text).unwrap()
}

#[test]
fn replays_the_golden_configs_as_json_and_as_yaml() {
    let cases = golden_config_cases();
    assert_eq!(cases.len(), 15);
    for (name, config, output) in &cases {
        assert_eq!(
            Value::Object(config.to_json()),
            output["returned"],
            "{name}: returned"
        );
        assert_eq!(
            read_yaml(&config.to_yaml()),
            output["parsed"],
            "{name}: parsed"
        );
    }
}

#[test]
fn replays_the_golden_applied_comparisons() {
    let cases = golden();
    let fill_first = profile("fill-first", true);
    let empty = configuration(
        &fill_first,
        9000,
        ("core-key-applied", "management-key-applied"),
        &[],
        "<data>/profiles/<profile>/auth",
    );
    let with_account = configuration(
        &fill_first,
        9000,
        ("core-key-applied", "management-key-applied"),
        &[account(
            0,
            &json!({
                "name": "acct", "provider": "custom", "baseUrl": "https://x.example/v1", "apiKey": "k",
                "prefix": "", "models": [{ "id": "m", "protocol": "anthropic" }],
            }),
        )],
        "<data>/profiles/<profile>/auth",
    );
    let mut passed = 0;
    for case in cases
        .iter()
        .filter(|case| case["name"].as_str().unwrap().starts_with("applied "))
    {
        let input = &case["input"];
        let expected = if input["expected"].is_string() {
            &empty
        } else {
            assert_eq!(
                Value::Object(with_account.to_json()),
                input["expected"],
                "{}",
                case["name"]
            );
            &with_account
        };
        let reported = if input["reported"] == "<expected>" {
            Value::Object(expected.to_json())
        } else {
            input["reported"].clone()
        };
        assert_eq!(
            Value::Bool(applied(expected, &reported)),
            case["output"],
            "{}",
            case["name"]
        );
        passed += 1;
    }
    assert_eq!(passed, 13);
}

#[test]
fn credentials_never_cross_endpoints_whose_urls_share_a_suffix() {
    let custom = |id: &str, name: &str, base_url: &str, model: &str| {
        let stored = json!({
            "id": id, "name": name, "provider": "custom", "baseUrl": base_url, "prefix": "",
            "models": [{ "id": model, "protocol": "openai" }], "disabled": false,
        });
        (
            serde_json::from_value::<ApiAccount>(stored).unwrap(),
            ApiKey::new(format!("test-{id}")),
        )
    };
    let first = "https://first.example/openai/v1";
    let other = "https://other.example/openai/v1";
    let accounts = [
        custom("first", "first", first, "model-a"),
        custom("other", "other", other, "model-a"),
        custom("same-endpoint", "second account", first, "model-a"),
        custom("other-model", "first", first, "model-b"),
    ];
    let config = configuration(
        &profile("round-robin", false),
        1,
        ("fixture", "fixture"),
        &accounts,
        "/tmp/fixture",
    )
    .to_json();
    let entries = config["openai-compatibility"].as_array().unwrap();
    assert_eq!(entries.len(), 3, "{entries:?}");
    let names: std::collections::HashSet<&Value> = entries.iter().map(|e| &e["name"]).collect();
    assert_eq!(names.len(), 3, "{entries:?}");
    for (id, base_url) in [
        ("first", first),
        ("other", other),
        ("same-endpoint", first),
        ("other-model", first),
    ] {
        let key = format!("test-{id}");
        let entry = entries
            .iter()
            .find(|entry| {
                entry["api-key-entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|k| k["api-key"] == key.as_str())
            })
            .unwrap_or_else(|| panic!("an entry holds {key}"));
        assert_eq!(entry["base-url"], base_url, "{id}");
    }
}

async fn open(directory: &std::path::Path) -> Store {
    nsv_files::owner_only_directory(directory, || Ok::<(), std::io::Error>(())).unwrap();
    let data = DataDir::new(directory);
    Store::open(&data, Vault::open(&data).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn replays_the_golden_slugs_and_the_unknown_profile() {
    let cases = golden();
    let root = TempDir::new("slugs");
    let store = open(root.path()).await;
    let slugs = cases
        .iter()
        .find(|case| case["name"] == "slugs in creation order")
        .unwrap();
    let mut made = Vec::new();
    for name in slugs["input"].as_array().unwrap() {
        let name = name.as_str().unwrap().to_owned();
        made.push(
            store
                .call(move |db| db.create_profile(&name))
                .await
                .unwrap()
                .slug
                .to_string(),
        );
    }
    assert_eq!(json!(made), slugs["output"]);
    let unknown = cases
        .iter()
        .find(|case| case["name"] == "config unknown profile")
        .unwrap();
    let missing = ProfileId::parse("00000000-0000-4000-8000-00000000dead").unwrap();
    let error = write_config(&store, missing.clone(), 1).await.unwrap_err();
    let StoreError::App(error) = error else {
        panic!("{error:?}")
    };
    assert_eq!(
        json!({ "message": error.message, "status": error.status }),
        unknown["error"]
    );
    assert!(
        !DataDir::new(root.path()).profile(&missing).exists(),
        "nothing was created"
    );
    store.close().await;
}

#[tokio::test]
async fn the_store_writes_an_owner_only_config_in_place() {
    let root = TempDir::new("write");
    let store = open(root.path()).await;
    let profile = store.call(|db| db.create_profile("Work")).await.unwrap();
    let (stored, key) = account(
        0,
        &json!({
            "name": "local", "provider": "custom", "baseUrl": "http://127.0.0.1:11434/v1/", "apiKey": "synthetic key local",
            "prefix": "local", "models": [{ "id": "qwen3-coder:30b", "protocol": "openai" }],
        }),
    );
    let id = profile.id.clone();
    store
        .call(move |db| db.save_api_account(&id, &stored, Some(&key)))
        .await
        .unwrap();
    let config = write_config(&store, profile.id.clone(), 8317)
        .await
        .unwrap();
    let path = DataDir::new(root.path()).core_config(&profile.id);
    let auth = DataDir::new(root.path()).credentials(&profile.id);
    assert_eq!(config.auth_dir, auth.to_str().unwrap());
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&auth), 0o700);
    assert_eq!(mode(&DataDir::new(root.path()).profile(&profile.id)), 0o700);
    let written = read_yaml(&fs::read_to_string(&path).unwrap());
    assert_eq!(written, Value::Object(config.to_json()));
    assert_eq!(written["port"], 8317);
    assert_eq!(
        written["routing"]["strategy"], "fill-first",
        "a new profile's default"
    );
    assert_eq!(written["routing"]["session-affinity"], true);
    assert_eq!(
        written["openai-compatibility"][0]["api-key-entries"][0]["api-key"],
        "synthetic key local"
    );
    // The core watches this inode: a rewrite keeps it, even after the mode was loosened.
    let inode = fs::metadata(&path).unwrap().ino();
    fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    write_config(&store, profile.id.clone(), 8318)
        .await
        .unwrap();
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(mode(&path), 0o600);
    assert_eq!(read_yaml(&fs::read_to_string(&path).unwrap())["port"], 8318);
    // Concurrent writes serialize: the file holds the last one, with the saved settings.
    let mut saved = profile.clone();
    saved.strategy = serde_json::from_value(json!("round-robin")).unwrap();
    saved.session_affinity = false;
    store.call(move |db| db.save_profile(&saved)).await.unwrap();
    let (first, second) = tokio::join!(
        write_config(&store, profile.id.clone(), 1235),
        write_config(&store, profile.id.clone(), 1236),
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!([first.port, second.port], [1235, 1236]);
    let written = read_yaml(&fs::read_to_string(&path).unwrap());
    assert_eq!(written, Value::Object(second.to_json()));
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(written["routing"]["strategy"], "round-robin");
    assert_eq!(written["routing"]["session-affinity"], false);
    // A failed write leaves the next one free to succeed.
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(
        write_config(&store, profile.id.clone(), 1237)
            .await
            .is_err()
    );
    fs::remove_dir(&path).unwrap();
    let recovered = write_config(&store, profile.id.clone(), 1238)
        .await
        .unwrap();
    assert_eq!(recovered.port, 1238);
    assert_eq!(
        read_yaml(&fs::read_to_string(&path).unwrap()),
        Value::Object(recovered.to_json())
    );
    // A symbolic link there is refused, and its target never receives the secrets.
    let target = root.path().join("elsewhere");
    fs::write(&target, "untouched").unwrap();
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(
        write_config(&store, profile.id.clone(), 8319)
            .await
            .is_err()
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
    store.close().await;
}
