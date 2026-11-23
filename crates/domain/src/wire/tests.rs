use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::*;
use crate::registry::{Agent, Protocol, RetentionDays};

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Parses `value` as `T` and writes it back; the result must be the same JSON value.
#[track_caller]
fn round_trip<T: Serialize + DeserializeOwned>(value: &Value) -> T {
    let parsed: T =
        serde_json::from_value(value.clone()).unwrap_or_else(|error| panic!("{error} in {value}"));
    assert_eq!(&serde_json::to_value(&parsed).unwrap(), value);
    parsed
}

fn fixture() -> Value {
    let path = repository().join("tests/fixtures/ts-data-dir/expected.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn stored_bodies_round_trip() {
    let fixture = fixture();
    for profile in fixture["profiles"].as_array().unwrap() {
        round_trip::<Profile>(profile);
    }
    for accounts in fixture["apiAccounts"].as_object().unwrap().values() {
        round_trip::<Vec<ApiAccount>>(accounts);
    }
    for accounts in fixture["accountMetadata"].as_object().unwrap().values() {
        round_trip::<Vec<Account>>(accounts);
    }
    for accounts in fixture["accountsList"].as_object().unwrap().values() {
        round_trip::<Vec<Account>>(accounts);
    }
    let quotas = fixture["quotas"].as_array().unwrap();
    assert_eq!(quotas.len(), 6);
    for row in quotas {
        round_trip::<Quota>(&row["quota"]);
    }
}

#[test]
fn usage_records_round_trip_with_pricing_tokens_present_null_and_absent() {
    let fixture = fixture();
    let (mut object, mut null, mut absent) = (0, 0, 0);
    for usage in fixture["usage"].as_object().unwrap().values() {
        for row in usage["records"].as_array().unwrap() {
            let record = round_trip::<UsageRecord>(&row["record"]);
            match record.pricing_tokens {
                Some(Some(_)) => object += 1,
                Some(None) => null += 1,
                None => absent += 1,
            }
        }
    }
    assert!(
        object > 0 && null > 0 && absent > 0,
        "{object} {null} {absent}"
    );
}

/// The store skips a write whose JSON text is unchanged, so account metadata must
/// serialize to the very bytes TypeScript wrote.
#[test]
fn account_metadata_writes_the_bytes_typescript_wrote() {
    let fixture = fixture();
    let rows = fixture["settings"].as_object().unwrap();
    let mut checked = 0;
    for (id, value) in rows.iter().filter(|(id, _)| id.starts_with("accounts:")) {
        let text = value.as_str().unwrap();
        let accounts: Vec<Account> = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&accounts).unwrap(), text, "{id}");
        checked += 1;
    }
    assert_eq!(checked, 2);
}

#[test]
fn unknown_fields_survive_a_read_modify_write() {
    let stored = json!({
        "id": "6e4156ab-2acf-4d89-9a81-264f3b3da50a", "slug": "team", "name": "Team",
        "strategy": "round-robin", "sessionAffinity": false, "enabled": true,
        "createdAt": "2026-11-20T10:00:00.000Z", "color": "teal", "pins": [1, 2]
    });
    let mut profile: Profile = serde_json::from_value(stored.clone()).unwrap();
    profile.enabled = false;
    let mut expected = stored;
    expected["enabled"] = json!(false);
    assert_eq!(serde_json::to_value(&profile).unwrap(), expected);
}

#[test]
fn app_state_round_trips() {
    let account = json!({
        "id": "claude-1.json", "name": "Work", "accountUuid": "a", "organizationUuid": "b",
        "organizationName": "Acme", "provider": "claude", "kind": "oauth", "email": "a@b.c",
        "disabled": false, "status": "Needs attention", "priority": -3, "authIndex": "1",
        "quota": {
            "status": "error", "checkedAt": "2026-11-22T12:00:00.000Z", "error": "Timed out",
            "windows": [
                { "label": "5-hour", "remainingPercent": 12.25, "resetsAt": null },
                { "label": "Opus", "remainingPercent": null, "resetsAt": "2026-11-23T00:00:00.000Z", "scoped": true }
            ]
        }
    });
    let api = json!({
        "id": "5ca3dfd5-9fed-42fb-940a-ec69912cd659", "name": "Go", "provider": "opencode-go",
        "kind": "api-key", "disabled": true, "status": "Paused", "priority": 0,
        "prefix": "go", "modelCount": 3
    });
    let profile = |extra: Value| {
        let mut state = json!({
            "id": "d120c6b8-8067-4659-8593-37de2c72d1cf", "slug": "personal", "name": "★ ★ ★",
            "strategy": "fill-first", "sessionAffinity": true, "enabled": false,
            "createdAt": "2026-11-20T10:00:00.000Z", "runtime": "stopped", "accounts": [],
            "endpoint": "http://127.0.0.1:4318/p/personal"
        });
        state
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        state
    };
    let states = [
        json!({
            "profiles": [
                profile(json!({ "runtime": "error", "error": "Core exited", "accounts": [account, api] })),
                profile(json!({}))
            ],
            "coreVersion": "8.1.0",
            "core": { "bundled": "8.1.0", "updates": true, "available": "8.2.0", "phase": "downloading" },
            "gateway": "http://127.0.0.1:4318", "usageRetentionDays": 365, "version": "0.2.0",
            "errors": ["one"]
        }),
        json!({
            "profiles": [], "coreVersion": "8.1.0", "core": { "bundled": "8.1.0", "updates": false },
            "gateway": "http://127.0.0.1:4318", "usageRetentionDays": 0, "version": "0.2.0",
            "errors": []
        }),
    ];
    for state in &states {
        round_trip::<AppState>(state);
    }
}

#[test]
fn responses_round_trip() {
    round_trip::<UsageHistory>(&json!({
        "records": [],
        "summary": { "requests": 2, "failed": 1, "inputTokens": 10, "outputTokens": 5,
            "cachedTokens": 0, "reasoningTokens": 0, "totalTokens": 15 },
        "pricing": {
            "totals": { "requests": 2, "pricedRequests": 1, "usd": 0.0125, "normalizedRequests": 1,
                "tokens": { "input": 10, "cached": 0, "cacheWrite": 0, "output": 5, "reasoning": 0 } },
            "usd": { "a": 0.0125, "b": null }, "fetchedAt": null, "refreshing": true,
            "eur": { "rate": 0.92, "date": "2026-11-21" }
        }
    }));
    round_trip::<Vec<CatalogModel>>(&json!([
        { "id": "bare" },
        { "id": "claude-sonnet-4-5", "owned_by": "claude", "metadata": {
            "name": "Claude Sonnet 4.5", "context": 200_000, "output": 64_000, "inputLimit": 180_000,
            "input": ["text", "image"], "reasoning": true, "toolCall": true,
            "cost": { "input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75,
                "tiers": [{ "input": 6, "output": 22.5, "tier": { "type": "context", "size": 200_000 } }] },
            "source": "models.dev", "fetchedAt": "2026-11-22T12:00:00.000Z" } }
    ]));
    for status in [
        json!({ "status": "wait" }),
        json!({ "status": "ok" }),
        json!({ "status": "ok", "reconnected": true }),
        json!({ "status": "error", "error": "Denied" }),
        json!({ "status": "review", "review": { "action": "blocked", "message": "Seat taken",
            "account": { "id": "x.json", "name": "X", "provider": "claude", "kind": "oauth",
                "disabled": false, "status": "Ready", "priority": 0 } } }),
    ] {
        round_trip::<OAuthStatus>(&status);
    }
    round_trip::<OAuthSession>(&json!({
        "profileId": "d120c6b8-8067-4659-8593-37de2c72d1cf", "provider": "codex",
        "url": "https://auth.openai.com/x", "state": "s", "startedAt": 1_763_812_800_000_u64,
        "userCode": "ABCD-EFGH", "expiresAt": 1_763_813_700_000_u64
    }));
    round_trip::<ResetCredits>(&json!({ "available_count": 1, "credits": [
        { "id": "c1", "reset_type": "weekly", "status": "available", "expires_at": null },
        { "id": "c2", "reset_type": "weekly", "status": "used", "expires_at": "2026-12-01",
            "title": null, "description": "One reset" }
    ] }));
    round_trip::<AgentSetup>(&json!({
        "agent": "claude", "projectDirectory": "/p", "otherProviders": false, "models": ["m"],
        "projects": ["/p"], "command": "claude", "files": ["/p/.claude/settings.local.json"],
        "provider": "Claude", "instructions": "Run claude"
    }));
    round_trip::<ApiError>(&json!({ "error": { "message": "Profile not found.",
        "type": "nonstopvibin_error", "code": 404 } }));
    round_trip::<Ok>(&json!({ "ok": true }));
    round_trip::<ResetResult>(&json!({ "code": "already_redeemed", "quotaRefreshed": false }));
}

#[test]
fn request_bodies_are_strict() {
    let input: ApiAccountInput = serde_json::from_value(json!({
        "name": "Go", "provider": "opencode-go", "baseUrl": "https://opencode.ai/zen/go/v1",
        "apiKey": "sk-visible", "prefix": "go", "models": []
    }))
    .unwrap();
    assert_eq!(input.protocol, Protocol::Openai);
    assert_eq!(input.api_key.expose_secret(), "sk-visible");
    assert!(!format!("{input:?}").contains("sk-visible"));
    assert!(serde_json::from_value::<ProfilePatch>(json!({ "enabled": true })).is_err());
    assert!(serde_json::from_value::<RetentionBody>(json!({ "days": 7 })).is_err());
    let selection: AgentSelection = serde_json::from_value(json!({ "agent": "pi" })).unwrap();
    assert_eq!(selection.agent, Agent::Pi);
    assert_eq!(
        serde_json::from_value::<RetentionBody>(json!({ "days": 90 }))
            .unwrap()
            .days,
        RetentionDays::Days90
    );
}

#[test]
fn literal_true_rejects_false() {
    assert!(serde_json::from_value::<Ok>(json!({ "ok": false })).is_err());
    let window = json!({ "label": "x", "remainingPercent": 1, "resetsAt": null, "scoped": false });
    assert!(serde_json::from_value::<QuotaWindow>(window).is_err());
}
