//! Replays the quota, reset, identity and API-account cases of `tests/golden/accounts.json`
//! (`tests/golden/areas/accounts.ts`) and the import scan of `api.json`. The `sync`,
//! `credentialPath` and `list` cases are replayed in `scheduler::golden`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::future::{Future, ready};
use std::sync::Arc;

use nsv_accounts::api_accounts::{api_account, go_model_protocol};
use nsv_accounts::identity::{SeatIdentity, claude_identity, same_seat, verify_claude_identity};
use nsv_accounts::imports::ImportCatalog;
use nsv_accounts::quota::{QuotaError, Quotas, RefreshQuota, parse_quota};
use nsv_accounts::resets::{Resets, reset_credits, reset_result};
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId, is_uuid};
use nsv_domain::secret::ApiKey;
use nsv_domain::wire::{Account, ApiAccount, ApiAccountInput, Quota, QuotaStatus};
use nsv_testkit::TempDir;
use nsv_testkit::fake::response;
use nsv_testkit::world::{StubFetch, open_store, profile};
use serde_json::{Value, json};

use super::common::{NOW, NOW_ISO, StubSource, case, cases, fixed, headers};

fn named<'a>(cases: &'a [Value], prefix: &str) -> Vec<&'a Value> {
    cases
        .iter()
        .filter(|case| case["name"].as_str().unwrap().starts_with(prefix))
        .collect()
}

/// The golden error shape: the message for an `AppError`, the status always.
fn assert_error(expected: &Value, message: &str, status: u16, name: &Value) {
    assert_eq!(expected["status"], u64::from(status), "{name}");
    if expected.get("message").is_some() && expected.get("thrown").is_none() {
        assert_eq!(expected["message"], message, "{name}");
    }
}

#[test]
fn quotas_parse_as_typescript_did() {
    let cases = cases("accounts");
    let parsed = named(&cases, "parseQuota ");
    for case in &parsed {
        let input = &case["input"];
        assert_eq!(input["now"], NOW_ISO);
        let result = parse_quota(input["provider"].as_str().unwrap(), &input["payload"], NOW);
        match (result, case.get("output")) {
            (Ok(quota), Some(expected)) => assert_eq!(
                &serde_json::to_value(quota).unwrap(),
                expected,
                "{}",
                case["name"]
            ),
            (Err(error), None) => {
                assert_error(&case["error"], &error.to_string(), 500, &case["name"]);
            }
            (result, _) => panic!("{}: {result:?}", case["name"]),
        }
    }
    assert_eq!(parsed.len(), 31);
}

#[test]
fn identities_and_protocols_match_typescript() {
    let cases = cases("accounts");
    let mut replayed = 0;
    for case in named(&cases, "claudeIdentity ") {
        assert_eq!(
            serde_json::to_value(claude_identity(&case["input"])).unwrap(),
            case["output"],
            "{}",
            case["name"]
        );
        replayed += 1;
    }
    for case in named(&cases, "sameSeat ") {
        let a: SeatIdentity = serde_json::from_value(case["input"][0].clone()).unwrap();
        let b: SeatIdentity = serde_json::from_value(case["input"][1].clone()).unwrap();
        assert_eq!(json!(same_seat(&a, &b)), case["output"], "{}", case["name"]);
        replayed += 1;
    }
    for case in named(&cases, "goModelProtocol ") {
        let protocol = go_model_protocol(
            case["input"]["id"].as_str().unwrap(),
            case["input"]["endpoint"].as_str(),
        );
        assert_eq!(json!(protocol), case["output"], "{}", case["name"]);
        replayed += 1;
    }
    for case in named(&cases, "schema ") {
        let input = &case["input"];
        let output = if case["name"].as_str().unwrap().starts_with("schema result ") {
            reset_result(input).map(|code| json!({ "code": code }))
        } else {
            reset_credits(input).map(|credits| serde_json::to_value(credits).unwrap())
        };
        match (output, case.get("output")) {
            (Some(output), Some(expected)) => assert_eq!(&output, expected, "{}", case["name"]),
            (None, None) => assert_eq!(case["error"]["status"], 400),
            (output, _) => panic!("{}: {output:?}", case["name"]),
        }
        replayed += 1;
    }
    assert_eq!(replayed, 12 + 4 + 11 + 10);
}

/// The recorder's stubbed `/models` answers.
fn discovery(url: &str) -> nsv_net::Response {
    let (status, body) = match url {
        "https://discover.example.com/v1/models" => (
            200,
            json!({ "data": [{ "id": "a" }, { "id": 5 }, { "name": "no id" }, { "id": "b", "endpoint": "/v1/messages" }] }),
        ),
        "https://discover.example.com/anthropic/models" => {
            (200, json!({ "data": [{ "id": "claude-x" }] }))
        }
        "https://opencode.ai/zen/go/v1/models" => (
            200,
            json!({ "data": [
                { "id": "glm-4.6" },
                { "id": "kimi-k2", "endpoint": "https://opencode.ai/zen/go/v1/messages" },
                { "id": "gpt-5", "endpoint": "/v1/chat/completions" },
                { "id": "x", "endpoint": "/v1/responses" },
                { "id": "qwen3-coder" },
            ] }),
        ),
        "https://empty.example.com/v1/models" => (200, json!({ "data": [] })),
        "https://forbidden.example.com/v1/models" => (403, json!({})),
        "https://notarray.example.com/v1/models" => (200, json!({ "data": {} })),
        other => panic!("unexpected discovery of {other}"),
    };
    response(status, body.to_string())
}

#[tokio::test]
async fn api_accounts_validate_and_discover_as_typescript_did() {
    let cases = cases("accounts");
    let accounts = named(&cases, "apiAccount ");
    for case in &accounts {
        let input: ApiAccountInput = serde_json::from_value(case["input"].clone()).unwrap();
        let fetch = StubFetch::new(|url| Ok(discovery(url)));
        let result = api_account(&input, &fetch).await;
        let requests: Vec<Value> = fetch
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|seen| {
                assert_eq!(seen.loopback, None);
                json!({ "url": seen.request.url, "method": "GET", "headers": headers(&seen.request), "redirect": "error" })
            })
            .collect();
        match (result, case.get("output")) {
            (Ok(account), Some(expected)) => {
                let mut account = serde_json::to_value(account).unwrap();
                assert!(is_uuid(account["id"].as_str().unwrap()));
                account["id"] = json!("<uuid>");
                assert_eq!(
                    &json!({ "account": account, "requests": requests }),
                    expected,
                    "{}",
                    case["name"]
                );
            }
            (Err(error), None) => {
                let error = AppError::from(error);
                assert_error(&case["error"], &error.message, error.status, &case["name"]);
            }
            (result, _) => panic!("{}: {result:?}", case["name"]),
        }
    }
    assert_eq!(accounts.len(), 20);
}

#[tokio::test]
async fn claude_identity_is_verified_with_the_token_as_typescript_did() {
    let cases = cases("accounts");
    let verified = named(&cases, "verifyClaudeIdentity ");
    for case in &verified {
        let answer = case["input"]["response"].clone();
        let fetch = StubFetch::new(move |_| match &answer {
            Value::String(network) if network == "network" => Err(nsv_net::NetError::Connect(
                std::io::ErrorKind::ConnectionRefused.into(),
            )),
            answer => Ok(response(
                u16::try_from(answer["status"].as_u64().unwrap()).unwrap(),
                answer["body"].as_str().unwrap(),
            )),
        });
        let raw = case["input"]["raw"].as_object().unwrap();
        let result = verify_claude_identity(&fetch, raw).await;
        let requests: Vec<Value> = fetch
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|seen| {
                json!({ "url": seen.request.url, "method": "GET", "headers": headers(&seen.request), "redirect": "error" })
            })
            .collect();
        match (result, case.get("output")) {
            (Ok(credential), Some(expected)) => assert_eq!(
                &json!({ "credential": credential, "requests": requests }),
                expected,
                "{}",
                case["name"]
            ),
            (Err(error), None) => {
                assert_error(&case["error"], &error.message, error.status, &case["name"]);
            }
            (result, _) => panic!("{}: {result:?}", case["name"]),
        }
    }
    assert_eq!(verified.len(), 7);
}

/// Stands in for `Quotas` in the reset flow, as the recorder's stub did.
struct Available;

impl RefreshQuota for Available {
    fn refresh(
        &self,
        _: &ProfileId,
        _: &AccountId,
    ) -> impl Future<Output = Result<Quota, QuotaError>> + Send {
        ready(Ok(Quota {
            status: QuotaStatus::Available,
            windows: Vec::new(),
            checked_at: NOW_ISO.to_owned(),
            error: None,
            plan: None,
            banked_resets: None,
        }))
    }
}

fn api_call(body: &Value) -> Value {
    json!({ "path": "/api-call", "method": "POST", "body": body })
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the recorder's refresh and reset sequence, in order"
)]
async fn quota_refreshes_and_resets_replay_in_order() {
    let cases = cases("accounts");
    let accounts: Vec<Account> =
        serde_json::from_value(case(&cases, "list with api accounts")["output"].clone()).unwrap();
    let chatgpt: HashMap<String, String> =
        case(&cases, "sync listing")["output"]["chatgptAccountIds"]
            .as_object()
            .unwrap()
            .iter()
            .filter_map(|(id, value)| Some((id.clone(), value.as_str()?.to_owned())))
            .collect();
    let directory = TempDir::new("quota-golden");
    let store = open_store(directory.path()).await;
    let profile = profile(&store, "Accounts").await;
    for (raw, key) in [
        (
            json!({ "id": "go-account", "name": "Go", "provider": "opencode-go", "baseUrl": "https://opencode.ai/zen/go/v1", "prefix": "go", "models": [{ "id": "glm-4.6", "protocol": "openai" }], "disabled": false }),
            "synthetic go key",
        ),
        (
            json!({ "id": "custom-account", "name": "Custom", "provider": "custom", "baseUrl": "https://x.example/v1", "prefix": "", "models": [{ "id": "m", "protocol": "openai" }], "disabled": true }),
            "k",
        ),
    ] {
        let account: ApiAccount = serde_json::from_value(raw).unwrap();
        let profile = profile.clone();
        store
            .call(move |db| db.save_api_account(&profile, &account, Some(&ApiKey::new(key))))
            .await
            .unwrap();
    }
    let source = Arc::new(StubSource {
        accounts,
        chatgpt: chatgpt.clone(),
        ..StubSource::default()
    });
    let fetch = Arc::new(StubFetch::new(|_| Ok(response(500, ""))));
    let quotas = Quotas::new(
        store.clone(),
        Arc::clone(&source),
        Arc::<StubFetch>::clone(&fetch),
        fixed,
    );
    let mut replayed = 0;
    for case in named(&cases, "refresh ") {
        let input = &case["input"];
        let account = AccountId::parse(input["accountId"].as_str().unwrap()).unwrap();
        if let Some(status) = input["status"].as_u64() {
            let body = match case["name"].as_str().unwrap() {
                "refresh opencode go ok" => r#"{"usage":{"rolling":{"usagePercent":5}}}"#,
                "refresh opencode go unreadable" => "<html>",
                _ => "",
            };
            fetch.answer(move |_| Ok(response(u16::try_from(status).unwrap(), body)));
        }
        source.queue(input["answers"].as_array().cloned().unwrap_or_default());
        let before = fetch.count();
        let result = quotas.refresh(&profile, &account).await;
        let upstream: Vec<Value> = source.take_calls().iter().map(api_call).collect();
        let requests: Vec<Value> = fetch.seen.lock().unwrap()[before..]
            .iter()
            .map(|seen| json!({ "url": seen.request.url, "method": "GET", "headers": headers(&seen.request), "redirect": "error" }))
            .collect();
        match (result, case.get("output")) {
            (Ok(quota), Some(expected)) => {
                let quota = serde_json::to_value(quota).unwrap();
                let actual = if input.get("status").is_some() {
                    json!({ "quota": quota, "requests": requests })
                } else {
                    json!({ "quota": quota, "upstream": upstream })
                };
                assert_eq!(&actual, expected, "{}", case["name"]);
            }
            (Err(error), None) => {
                let error = AppError::from(error);
                assert_error(&case["error"], &error.message, error.status, &case["name"]);
            }
            (result, _) => panic!("{}: {result:?}", case["name"]),
        }
        replayed += 1;
    }
    assert_eq!(replayed, 16);

    let resets = Resets::new(store.clone(), Arc::clone(&source), Arc::new(Available));
    let identity = chatgpt["codex-pro.json"].clone();
    for case in named(&cases, "reset ") {
        let input = &case["input"];
        let account = AccountId::parse(input["accountId"].as_str().unwrap()).unwrap();
        if let Some(answer) = input.get("answer") {
            source.queue([answer.clone()]);
        }
        let credit = input["creditId"].as_str();
        let result = match credit {
            None => resets
                .credits(&profile, &account)
                .await
                .map(|credits| serde_json::to_value(credits).unwrap()),
            Some(credit) => resets
                .consume(&profile, &account, credit)
                .await
                .map(|result| serde_json::to_value(result).unwrap()),
        };
        let mut sent_id = None;
        let upstream: Vec<Value> = source
            .take_calls()
            .into_iter()
            .map(|mut body| {
                if let Some(data) = body["data"].as_str() {
                    let mut data: Value = serde_json::from_str(data).unwrap();
                    let id = data["redeem_request_id"].as_str().unwrap().to_owned();
                    assert!(is_uuid(&id));
                    data["redeem_request_id"] = json!("<uuid>");
                    body["data"] = json!(data.to_string());
                    sent_id = Some(id);
                }
                api_call(&body)
            })
            .collect();
        match (result, case.get("output")) {
            (Ok(result), Some(expected)) => {
                // Still pending when the store answers the same id again; a cleared one is new.
                let pending = match (credit, sent_id) {
                    (Some(credit), Some(sent)) => {
                        let (identity, credit) = (identity.clone(), credit.to_owned());
                        let stored = store
                            .call(move |db| db.reset_request_id(&identity, &credit))
                            .await
                            .unwrap();
                        json!(stored == sent)
                    }
                    _ => Value::Null,
                };
                let actual = json!({ "result": result, "upstream": upstream, "requestStillPending": pending });
                assert_eq!(&actual, expected, "{}", case["name"]);
            }
            (Err(error), None) => {
                let error = AppError::from(error);
                assert_error(&case["error"], &error.message, error.status, &case["name"]);
            }
            (result, _) => panic!("{}: {result:?}", case["name"]),
        }
        replayed += 1;
    }
    assert_eq!(replayed, 16 + 11);
    store.close().await;
}

#[test]
fn imports_scan_known_folders_as_typescript_did() {
    let api = cases("api");
    let golden = case(&api, "GET import-sources")["output"]["body"]
        .as_array()
        .unwrap()
        .clone();
    let home = TempDir::new("imports");
    let cli = home.path().join(".cli-proxy-api");
    let easy = home.path().join(".local/share/com.cpa.gui/oauth");
    fs::create_dir_all(&cli).unwrap();
    fs::create_dir_all(&easy).unwrap();
    let write = |path: std::path::PathBuf, content: &str| fs::write(path, content).unwrap();
    write(
        cli.join("claude-one.json"),
        r#"{"type":"claude","email":"ada@example.com","account_uuid":"AAAAAAAA-0000-4000-8000-000000000001","organization_uuid":"bbbbbbbb-0000-4000-8000-000000000001","organization_name":"Acme"}"#,
    );
    write(
        cli.join("codex-one.json"),
        r#"{"type":"codex","email":"ada@example.com"}"#,
    );
    write(
        cli.join("gemini.json"),
        r#"{"type":"gemini-cli","email":"x@example.com"}"#,
    );
    write(cli.join("broken.json"), "{");
    write(cli.join("notes.txt"), "ignored");
    write(easy.join("kimi.json"), r#"{"type":"kimi"}"#);
    fs::create_dir(cli.join("folder.json")).unwrap();
    std::os::unix::fs::symlink(cli.join("codex-one.json"), cli.join("link.json")).unwrap();

    let mut catalog = ImportCatalog::new(home.path());
    let found = catalog.scan();
    // Each id is the SHA-256 of the file's path; the recorder's temporary home differs.
    let without_ids = |sources: Vec<Value>| -> Vec<Value> {
        let mut sources: Vec<Value> = sources
            .into_iter()
            .map(|mut source| {
                source.as_object_mut().unwrap().remove("id");
                source
            })
            .collect();
        // Within one folder the order is the file system's.
        if let Some(rest) = sources.get_mut(1..) {
            rest.sort_by_key(|source| source["fileName"].as_str().unwrap().to_owned());
        }
        sources
    };
    let actual: Vec<Value> = found
        .iter()
        .map(|source| serde_json::to_value(source).unwrap())
        .collect();
    assert_eq!(without_ids(actual), without_ids(golden));
    for source in &found {
        let folder = if source.source == "CLIProxyAPI" {
            &cli
        } else {
            &easy
        };
        let path = folder.join(&source.file_name);
        let contents = catalog.contents(&source.id).unwrap();
        assert_eq!(contents["type"], source.provider.as_str());
        assert_eq!(source.id.len(), 64);
        assert_eq!(source.id, sha256_hex(path.to_str().unwrap()));
    }

    let unknown = case(&api, "import-source unknown id")["output"]["body"]["error"].clone();
    let error = AppError::from(catalog.contents(&"0".repeat(64)).unwrap_err());
    assert_eq!(error.status, 404);
    assert_eq!(unknown["message"], error.to_string());
    // A file that grew past the limit after the scan is refused.
    fs::write(easy.join("kimi.json"), vec![b' '; 2_000_001]).unwrap();
    let kimi = found
        .iter()
        .find(|source| source.file_name == "kimi.json")
        .unwrap();
    let error = AppError::from(catalog.contents(&kimi.id).unwrap_err());
    assert_eq!(
        (error.status, error.message.as_str()),
        (
            400,
            "The selected account file is not a supported regular file."
        )
    );
}

/// File names that are not UTF-8 but read alike as text still get an id each, and each id
/// reads its own file. macOS file systems take only UTF-8 names.
#[cfg(target_os = "linux")]
#[test]
fn imports_tell_apart_names_that_read_alike_as_text() {
    use std::os::unix::ffi::OsStrExt;

    let home = TempDir::new("imports-bytes");
    let cli = home.path().join(".cli-proxy-api");
    fs::create_dir_all(&cli).unwrap();
    for (name, email) in [
        (&b"seat-\xfe.json"[..], "one@example.com"),
        (&b"seat-\xff.json"[..], "two@example.com"),
    ] {
        let content = json!({ "type": "codex", "email": email }).to_string();
        fs::write(cli.join(std::ffi::OsStr::from_bytes(name)), content).unwrap();
    }
    let mut catalog = ImportCatalog::new(home.path());
    let found = catalog.scan();
    assert_eq!(found.len(), 2);
    assert_ne!(found[0].id, found[1].id);
    for source in &found {
        let contents = catalog.contents(&source.id).unwrap();
        assert_eq!(contents["email"], source.email.as_deref().unwrap());
    }
}

fn sha256_hex(text: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::new(), |mut hex, byte| {
            write!(hex, "{byte:02x}").unwrap();
            hex
        })
}
