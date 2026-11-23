//! The pinned core under a generated config in front of a stub Anthropic provider, ported from
//! tests/native-anthropic.test.ts and tests/cross-harness.test.ts. The gateway forwards to the
//! core unchanged (golden/gateway.json and crates/cli/tests/serve.rs hold that hop), so each
//! case here talks to the core with its client key and reads what reached the provider.

use std::path::Path;
use std::time::{Duration, Instant};

use nsv_net::Method;
use nsv_store::Store;
use nsv_testkit::TempDir;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::core::{
    Provider, Reply, Running, Seen, account, call, events, frame, open, pinned_core, provider,
};

const MODEL: &str = "claude-sonnet-4-6";

fn inputs() -> [Value; 2] {
    [
        json!({ "path": "one.txt" }),
        json!({ "path": "two \"quoted\" π.txt" }),
    ]
}

fn image() -> Value {
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": "image/png",
            "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aT1sAAAAASUVORK5CYII=",
        },
    })
}

fn start() -> String {
    frame(
        "message_start",
        json!({ "message": {
            "id": "msg_native_fixture", "type": "message", "role": "assistant", "model": MODEL,
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": { "input_tokens": 12, "output_tokens": 0 },
        } }),
    )
}

/// `json` split after its ninth character, as the TypeScript fixtures split tool arguments.
fn halves(json: &str) -> [String; 2] {
    [
        json.chars().take(9).collect(),
        json.chars().skip(9).collect(),
    ]
}

fn tool_stream() -> String {
    let mut wire = start();
    for (index, input) in inputs().iter().enumerate() {
        wire += &frame(
            "content_block_start",
            json!({ "index": index, "content_block": {
                "type": "tool_use", "id": format!("toolu_native_{index}"), "name": "read_fixture", "input": {},
            } }),
        );
        for partial_json in halves(&input.to_string()) {
            wire += &frame(
                "content_block_delta",
                json!({ "index": index, "delta": { "type": "input_json_delta", "partial_json": partial_json } }),
            );
        }
        wire += &frame("content_block_stop", json!({ "index": index }));
    }
    wire + &frame(
        "message_delta",
        json!({ "delta": { "stop_reason": "tool_use", "stop_sequence": null }, "usage": { "output_tokens": 8 } }),
    ) + &frame("message_stop", json!({}))
}

/// The native provider of native-anthropic.test.ts, keyed by the label of the first message.
fn native(seen: &Seen) -> Reply {
    if seen.header("authorization") == Some("Bearer synthetic-native-exhausted") {
        return Reply::Json(
            429,
            json!({ "type": "error", "error": { "type": "rate_limit_error", "message": "synthetic quota exhausted" } }),
        );
    }
    let label = seen.label();
    if label == "reject" {
        return Reply::Json(
            400,
            json!({
                "type": "error",
                "error": { "type": "invalid_request_error", "message": "synthetic invalid request" },
                "request_id": "req_fixture",
            }),
        );
    }
    if seen.body["stream"] != true {
        return Reply::Json(
            200,
            json!({
                "id": "msg_native_fixture", "type": "message", "role": "assistant", "model": MODEL,
                "content": [{ "type": "text", "text": "fixture replay accepted" }],
                "stop_reason": "end_turn", "stop_sequence": null,
                "usage": { "input_tokens": 12, "output_tokens": 8 },
            }),
        );
    }
    match label {
        "hold" => Reply::Hold(start()),
        "truncated" => Reply::Events(vec![start()]),
        "stream-error" => Reply::Events(vec![
            start()
                + &frame(
                    "error",
                    json!({ "error": { "type": "overloaded_error", "message": "synthetic stream failure" } }),
                ),
        ]),
        // Transport chunks that split JSON and SSE delimiters.
        _ => {
            let wire: Vec<char> = tool_stream().chars().collect();
            Reply::Events(
                [&wire[..37], &wire[37..421], &wire[421..]]
                    .into_iter()
                    .map(|chunk| chunk.iter().collect())
                    .collect(),
            )
        }
    }
}

struct Native {
    root: TempDir,
    store: Store,
    upstream: Provider,
}

impl Native {
    async fn new(name: &str) -> Self {
        let root = TempDir::new(name);
        let store = open(&root.path().join("data")).await;
        Self {
            root,
            store,
            upstream: provider(native).await,
        }
    }

    /// A core for one profile whose Anthropic account answers to `key`.
    async fn core(&self, key: &str) -> Running {
        let base = format!("http://127.0.0.1:{}", self.upstream.port);
        let data = self.root.path().join("data");
        Running::start(
            &pinned_core(),
            &self.store,
            &data,
            vec![(account(key, &base, &[(MODEL, "anthropic")]), key)],
            |_| {},
        )
        .await
    }

    async fn close(self) {
        self.store.close().await;
    }
}

const VERSION: (&str, &str) = ("anthropic-version", "2023-06-01");

fn message(text: &str) -> Value {
    json!([{ "role": "user", "content": text }])
}

/// A Messages body for `MODEL` with `fields` added.
fn body(fields: Value) -> Value {
    let Value::Object(fields) = fields else {
        panic!("{fields}")
    };
    let mut body = json!({ "model": MODEL, "max_tokens": 64 });
    body.as_object_mut().unwrap().extend(fields);
    body
}

fn json_of(response: &nsv_net::Response) -> Value {
    serde_json::from_slice(&response.body).unwrap()
}

#[tokio::test]
async fn native_messages_sse_keeps_order_tool_ids_fragmented_arguments_and_replays_tool_results() {
    let fixture = Native::new("native-tools").await;
    let core = fixture.core("synthetic-native-primary").await;
    let response = core
        .post(
            "/v1/messages",
            &[VERSION],
            &body(json!({
                "stream": true,
                "messages": message("tools"),
                "tools": [{ "name": "read_fixture", "input_schema": {
                    "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"],
                } }],
            })),
        )
        .await;
    assert_eq!(response.status.as_u16(), 200);
    assert!(
        response.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let received = events(&response.body);
    // Native events, including usage and terminal order, survive unchanged.
    assert_eq!(received, events(tool_stream().as_bytes()));
    let blocks: Vec<Value> = received
        .iter()
        .filter(|event| event["type"] == "content_block_start")
        .map(|event| {
            let joined: String = received
                .iter()
                .filter(|delta| {
                    delta["type"] == "content_block_delta" && delta["index"] == event["index"]
                })
                .map(|delta| delta["delta"]["partial_json"].as_str().unwrap())
                .collect();
            let mut block = event["content_block"].clone();
            block["input"] = serde_json::from_str(&joined).unwrap();
            block
        })
        .collect();
    let arguments: Vec<&Value> = blocks.iter().map(|block| &block["input"]).collect();
    assert_eq!(arguments, inputs().iter().collect::<Vec<_>>());

    let results: Vec<Value> = (0..2)
        .map(|index| {
            json!({
                "type": "tool_result", "tool_use_id": format!("toolu_native_{index}"),
                "content": [{ "type": "text", "text": format!("result-{index}: π\nquoted \"data\"") }],
                "is_error": index == 1,
            })
        })
        .collect();
    let mut messages = message("tools").as_array().unwrap().clone();
    messages.push(json!({ "role": "assistant", "content": blocks }));
    let replay = core
        .post(
            "/v1/messages",
            &[VERSION],
            &body(json!({ "messages": [messages[0], messages[1], { "role": "user", "content": results }] })),
        )
        .await;
    assert_eq!(replay.status.as_u16(), 200);
    assert_eq!(
        json_of(&replay)["content"][0]["text"],
        "fixture replay accepted"
    );
    let forwarded = fixture.upstream.last();
    // The core adds an ephemeral cache breakpoint, but keeps every replay field.
    let mut cached = results[1].clone();
    cached["cache_control"] = json!({ "type": "ephemeral" });
    messages.push(json!({ "role": "user", "content": [results[0], cached] }));
    assert_eq!(forwarded.body["messages"], Value::Array(messages));
    assert_eq!(forwarded.path, "/v1/messages?beta=true");
    core.stop().await;
    fixture.close().await;
}

#[tokio::test]
async fn native_image_blocks_keep_source_type_media_type_and_bytes() {
    let fixture = Native::new("native-image").await;
    let core = fixture.core("synthetic-native-primary").await;
    let content = json!([{ "type": "text", "text": "Inspect fixture image" }, image()]);
    let response = core
        .post(
            "/v1/messages",
            &[VERSION],
            &body(json!({ "messages": [{ "role": "user", "content": content }] })),
        )
        .await;
    assert_eq!(response.status.as_u16(), 200);
    let mut cached = image();
    cached["cache_control"] = json!({ "type": "ephemeral" });
    assert_eq!(
        fixture.upstream.last().body["messages"][0]["content"],
        json!([content[0], cached])
    );
    core.stop().await;
    fixture.close().await;
}

#[tokio::test]
async fn native_headers_keep_version_and_betas_default_the_version_and_replace_client_auth() {
    let fixture = Native::new("native-headers").await;
    let core = fixture.core("synthetic-native-primary").await;
    let response = core
        .post(
            "/v1/messages",
            &[
                ("anthropic-version", "2023-01-01"),
                (
                    "anthropic-beta",
                    "message-threads-2026-08-12,mid-conversation-tool-changes-2026-07-01",
                ),
                ("x-api-key", &core.key.clone()),
                ("Cookie", "guest=not-upstream"),
                ("X-Forwarded-For", "192.0.2.1"),
            ],
            &body(json!({ "messages": message("headers"), "betas": ["fixture-extension-2026-01-01"] })),
        )
        .await;
    assert_eq!(response.status.as_u16(), 200);
    let request = fixture.upstream.last();
    assert_eq!(request.header("anthropic-version"), Some("2023-01-01"));
    assert_eq!(
        request
            .header("anthropic-beta")
            .unwrap()
            .split(',')
            .collect::<Vec<_>>(),
        [
            "message-threads-2026-08-12",
            "mid-conversation-tool-changes-2026-07-01",
            "fixture-extension-2026-01-01",
        ]
    );
    // The core lifts body betas into the provider's header.
    assert_eq!(request.body.get("betas"), None);
    assert_eq!(
        request.header("authorization"),
        Some("Bearer synthetic-native-primary")
    );
    for header in ["x-api-key", "cookie", "x-forwarded-for"] {
        assert_eq!(request.header(header), None, "{header}");
    }

    let defaulted = core
        .post(
            "/v1/messages",
            &[],
            &body(json!({ "messages": message("default headers") })),
        )
        .await;
    assert_eq!(defaulted.status.as_u16(), 200);
    let request = fixture.upstream.last();
    assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(request.header("anthropic-beta"), None);

    // The pinned core drops effort when thinking is disabled, not other extension betas.
    let gated = core
        .post(
            "/v1/messages",
            &[
                VERSION,
                ("anthropic-beta", "effort-2025-11-24,message-threads-2026-08-12"),
            ],
            &body(json!({ "messages": message("disabled thinking"), "thinking": { "type": "disabled" } })),
        )
        .await;
    assert_eq!(gated.status.as_u16(), 200);
    assert_eq!(
        fixture.upstream.last().header("anthropic-beta"),
        Some("message-threads-2026-08-12")
    );
    core.stop().await;
    fixture.close().await;
}

#[tokio::test]
async fn custom_origin_count_tokens_is_a_local_text_estimate_that_ignores_images_and_validates() {
    let fixture = Native::new("native-count").await;
    let core = fixture.core("synthetic-native-primary").await;
    let count =
        |content: Value| body(json!({ "messages": [{ "role": "user", "content": content }] }));
    let text = json!({ "type": "text", "text": "Count fixture" });
    let text_only = core
        .post(
            "/v1/messages/count_tokens",
            &[VERSION],
            &count(json!([text])),
        )
        .await;
    assert_eq!(text_only.status.as_u16(), 200);
    let estimate = json_of(&text_only);
    assert_eq!(
        estimate.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["input_tokens"]
    );
    assert!(estimate["input_tokens"].as_u64().unwrap() > 0, "{estimate}");
    // Not an authoritative multimodal count: the image adds nothing.
    let with_image = core
        .post(
            "/v1/messages/count_tokens",
            &[VERSION],
            &count(json!([text, image()])),
        )
        .await;
    assert_eq!(with_image.status.as_u16(), 200);
    assert_eq!(json_of(&with_image), estimate);
    let invalid = core
        .post(
            "/v1/messages/count_tokens",
            &[VERSION],
            &body(json!({ "messages": [] })),
        )
        .await;
    assert_eq!(invalid.status.as_u16(), 400);
    assert_eq!(json_of(&invalid)["error"]["type"], "invalid_request_error");
    // No provider inference and no pretend provider count endpoint.
    assert_eq!(fixture.upstream.seen().len(), 0);
    core.stop().await;
    fixture.close().await;
}

#[tokio::test]
async fn native_provider_errors_keep_status_type_and_message_but_drop_request_id() {
    let fixture = Native::new("native-error").await;
    let core = fixture.core("synthetic-native-primary").await;
    let response = core
        .post(
            "/v1/messages",
            &[VERSION],
            &body(json!({ "messages": message("reject") })),
        )
        .await;
    assert_eq!(response.status.as_u16(), 400);
    assert!(
        response.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    assert_eq!(
        json_of(&response),
        json!({ "type": "error", "error": { "type": "invalid_request_error", "message": "synthetic invalid request" } })
    );
    core.stop().await;
    fixture.close().await;
}

#[tokio::test]
async fn a_clean_eof_or_an_error_event_ends_with_an_error_not_message_stop_and_cools_the_account_down()
 {
    let fixture = Native::new("native-eof").await;
    for (label, error) in [
        (
            "truncated",
            json!({ "type": "api_error", "message": "upstream stream closed before message_stop" }),
        ),
        (
            "stream-error",
            json!({ "type": "overloaded_error", "message": "synthetic stream failure" }),
        ),
    ] {
        // A failed stream cools down its account, so each case gets its own core.
        let core = fixture.core(&format!("synthetic-native-{label}")).await;
        let response = core
            .post(
                "/v1/messages",
                &[VERSION],
                &body(json!({ "stream": true, "messages": message(label) })),
            )
            .await;
        // The status is committed after message_start; consumers must require message_stop.
        assert_eq!(response.status.as_u16(), 200, "{label}");
        let expected = start() + &frame("error", json!({ "error": error }));
        assert_eq!(
            events(&response.body),
            events(expected.as_bytes()),
            "{label}"
        );
        let next = core
            .post(
                "/v1/messages",
                &[VERSION],
                &body(json!({ "messages": message("after failure") })),
            )
            .await;
        assert_eq!(next.status.as_u16(), 503, "{label}");
        let reason = json_of(&next)["error"]["message"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(reason.starts_with("auth_unavailable"), "{label}: {reason}");
        core.stop().await;
    }
    fixture.close().await;
}

/// Whether a reader that closes a held stream after its first frame frees the provider's
/// connection within two seconds; `reset` closes with a TCP reset instead of a FIN.
async fn release_after_first_frame(core: &Running, upstream: &Provider, reset: bool) {
    let body = body(json!({ "stream": true, "messages": message("hold") })).to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", core.port)).await.unwrap();
    let head = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nanthropic-version: 2023-06-01\r\nContent-Length: {}\r\n\r\n{body}",
        core.port,
        core.key,
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut wire = Vec::new();
    let mut buffer = [0; 4096];
    while !String::from_utf8_lossy(&wire).contains("event: message_start") {
        let read = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(read, 0, "{}", String::from_utf8_lossy(&wire));
        wire.extend_from_slice(&buffer[..read]);
    }
    assert!(wire.starts_with(b"HTTP/1.1 200 "));
    assert_eq!(upstream.held(), 1);
    if reset {
        stream.set_zero_linger().unwrap();
    }
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(2);
    while upstream.held() != 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The local transport closes; this is no proof that remote compute or billing stopped.
    assert_eq!(upstream.held(), 0, "reset: {reset}");
}

#[tokio::test]
async fn a_fin_or_a_reset_after_the_first_frame_releases_the_provider_transport_within_two_seconds()
{
    let fixture = Native::new("native-release").await;
    let core = fixture.core("synthetic-native-primary").await;
    release_after_first_frame(&core, &fixture.upstream, false).await;
    release_after_first_frame(&core, &fixture.upstream, true).await;
    core.stop().await;
    fixture.close().await;
}

#[tokio::test]
async fn native_keys_reach_no_missing_model_and_an_exhausted_key_does_not_fall_back() {
    let fixture = Native::new("native-exhausted").await;
    let primary = fixture.core("synthetic-native-primary").await;
    let exhausted = fixture.core("synthetic-native-exhausted").await;
    let other = fixture.core("synthetic-native-other").await;
    let missing = primary
        .post(
            "/v1/messages",
            &[VERSION],
            &json!({ "model": "synthetic-unavailable-model", "max_tokens": 64, "messages": message("missing model") }),
        )
        .await;
    assert_eq!(missing.status.as_u16(), 400);
    assert_eq!(json_of(&missing)["error"]["type"], "invalid_request_error");
    assert_eq!(fixture.upstream.seen().len(), 0);

    let quota = exhausted
        .post(
            "/v1/messages",
            &[VERSION],
            &body(json!({ "messages": message("quota") })),
        )
        .await;
    assert_eq!(quota.status.as_u16(), 429);
    assert_eq!(json_of(&quota)["error"]["type"], "rate_limit_error");
    let keys: Vec<Option<String>> = fixture
        .upstream
        .seen()
        .iter()
        .map(|seen| seen.header("authorization").map(str::to_owned))
        .collect();
    assert!(
        !keys.is_empty()
            && keys
                .iter()
                .all(|key| key.as_deref() == Some("Bearer synthetic-native-exhausted")),
        "{keys:?}"
    );
    let healthy = other
        .post(
            "/v1/messages",
            &[VERSION],
            &body(json!({ "messages": message("healthy") })),
        )
        .await;
    assert_eq!(healthy.status.as_u16(), 200);
    assert_eq!(
        fixture.upstream.last().header("authorization"),
        Some("Bearer synthetic-native-other")
    );
    for core in [primary, exhausted, other] {
        core.stop().await;
    }
    fixture.close().await;
}

#[tokio::test]
async fn a_profile_without_models_lists_an_empty_native_catalog() {
    let root = TempDir::new("native-empty");
    let data = root.path().join("data");
    let store = open(&data).await;
    let core = Running::start(&pinned_core(), &store, &data, Vec::new(), |_| {}).await;
    let catalog = core
        .request(&Method::GET, "/v1/models", &[VERSION], None)
        .await;
    assert_eq!(catalog.status.as_u16(), 200);
    assert_eq!(
        json_of(&catalog),
        json!({ "data": [], "first_id": "", "has_more": false, "last_id": "" })
    );
    core.stop().await;
    store.close().await;
}

// tests/cross-harness.test.ts

const SCHEMA: &str =
    r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#;
/// base64 of the TypeScript fixture's synthetic signature envelope: bytes
/// `[18, 25, 10, 23, 8, 12, 16, 2, 50, 17, "claude-sonnet-4-6", 24, 1]`.
const SIGNATURE: &str = "EhkKFwgMEAIyEWNsYXVkZS1zb25uZXQtNC02GAE=";
/// Unpadded base64url of its 73-byte Fernet-shaped token: `0x80`, eight zeros, then 9..72.
const ENCRYPTED: &str = "gAAAAAAAAAAACQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0-P0BBQkNERUZHSA";

fn schema() -> Value {
    serde_json::from_str(SCHEMA).unwrap()
}

fn responses_stream(model: &Value) -> String {
    let reasoning = json!({
        "id": "rs_fixture", "type": "reasoning",
        "summary": [{ "type": "summary_text", "text": "Check both files." }],
        "encrypted_content": ENCRYPTED,
    });
    let tools: Vec<Value> = inputs()
        .iter()
        .enumerate()
        .map(|(i, input)| {
            json!({
                "id": format!("fc_{i}"), "type": "function_call", "call_id": format!("call_{i}"),
                "name": "read_fixture", "arguments": input.to_string(), "status": "completed",
            })
        })
        .collect();
    let mut text = frame(
        "response.created",
        json!({ "response": { "id": "resp_fixture", "object": "response", "status": "in_progress", "model": model, "output": [] } }),
    );
    let mut opened = reasoning.clone();
    opened["summary"] = json!([]);
    text += &frame(
        "response.output_item.added",
        json!({ "output_index": 0, "item": opened }),
    );
    text += &frame(
        "response.reasoning_summary_text.delta",
        json!({ "item_id": "rs_fixture", "output_index": 0, "summary_index": 0, "delta": "Check both files." }),
    );
    text += &frame(
        "response.output_item.done",
        json!({ "output_index": 0, "item": reasoning }),
    );
    for (i, tool) in tools.iter().enumerate() {
        let mut added = tool.clone();
        added["arguments"] = json!("");
        added["status"] = json!("in_progress");
        text += &frame(
            "response.output_item.added",
            json!({ "output_index": i + 1, "item": added }),
        );
        let arguments = tool["arguments"].as_str().unwrap();
        for delta in halves(arguments) {
            text += &frame(
                "response.function_call_arguments.delta",
                json!({ "output_index": i + 1, "item_id": tool["id"], "delta": delta }),
            );
        }
        text += &frame(
            "response.function_call_arguments.done",
            json!({ "output_index": i + 1, "item_id": tool["id"], "arguments": arguments }),
        );
        text += &frame(
            "response.output_item.done",
            json!({ "output_index": i + 1, "item": tool }),
        );
    }
    let mut output = vec![reasoning];
    output.extend(tools);
    text + &frame(
        "response.completed",
        json!({ "response": {
            "id": "resp_fixture", "object": "response", "status": "completed", "model": model,
            "output": output, "usage": { "input_tokens": 12, "output_tokens": 8, "total_tokens": 20 },
        } }),
    )
}

fn messages_stream(model: &Value) -> String {
    let mut text = frame(
        "message_start",
        json!({ "message": {
            "id": "msg_fixture", "type": "message", "role": "assistant", "model": model,
            "content": [], "stop_reason": null, "usage": { "input_tokens": 12, "output_tokens": 0 },
        } }),
    );
    text += &frame(
        "content_block_start",
        json!({ "index": 0, "content_block": { "type": "thinking", "thinking": "" } }),
    );
    text += &frame(
        "content_block_delta",
        json!({ "index": 0, "delta": { "type": "thinking_delta", "thinking": "Check both files." } }),
    );
    text += &frame(
        "content_block_delta",
        json!({ "index": 0, "delta": { "type": "signature_delta", "signature": SIGNATURE } }),
    );
    text += &frame("content_block_stop", json!({ "index": 0 }));
    for (i, input) in inputs().iter().enumerate() {
        text += &frame(
            "content_block_start",
            json!({ "index": i + 1, "content_block": {
                "type": "tool_use", "id": format!("toolu_{i}"), "name": "read_fixture", "input": {},
            } }),
        );
        for partial_json in halves(&input.to_string()) {
            text += &frame(
                "content_block_delta",
                json!({ "index": i + 1, "delta": { "type": "input_json_delta", "partial_json": partial_json } }),
            );
        }
        text += &frame("content_block_stop", json!({ "index": i + 1 }));
    }
    text + &frame(
        "message_delta",
        json!({ "delta": { "stop_reason": "tool_use", "stop_sequence": null }, "usage": { "output_tokens": 8 } }),
    ) + &frame("message_stop", json!({}))
}

/// The Pi test's provider: a native stream, or a plain `ok` message.
fn pi(seen: &Seen) -> Reply {
    if seen.body["stream"] == true {
        return Reply::Events(vec![messages_stream(&seen.body["model"])]);
    }
    Reply::Json(
        200,
        json!({
            "id": "msg_fixture", "type": "message", "role": "assistant", "model": seen.body["model"],
            "content": [{ "type": "text", "text": "ok" }], "stop_reason": "end_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1 },
        }),
    )
}

/// Every content block of every message whose content is a list.
fn content_blocks(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .cloned()
        .collect()
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the TypeScript test's four requests per wire profile, in order"
)]
async fn the_pinned_core_forwards_pi_tool_changes_and_betas_with_and_without_the_cli_fingerprint() {
    let root = TempDir::new("pi-tools");
    let data = root.path().join("data");
    let store = open(&data).await;
    let upstream = provider(pi).await;
    let base = format!("http://127.0.0.1:{}", upstream.port);
    let mut probes = 0;
    for oauth in [false, true] {
        let core = Running::start(
            &pinned_core(),
            &store,
            &data,
            vec![(
                account(
                    "synthetic-claude",
                    &base,
                    &[("claude-opus-5-5", "anthropic")],
                ),
                "synthetic-anthropic",
            )],
            |_| {},
        )
        .await;
        if oauth {
            // The OAuth wire profile, without a token or an account-profile lookup.
            let config = call(
                core.port,
                &Method::GET,
                "/v0/management/config",
                &core.management,
                None,
            )
            .await
            .unwrap();
            let config: Value = serde_json::from_slice(&config.body).unwrap();
            let entries: Vec<Value> = config["claude-api-key"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| {
                    let mut entry = entry.clone();
                    entry["fingerprint-profile"] = json!("claude-code-cli");
                    entry
                })
                .collect();
            let entries = Value::Array(entries).to_string();
            // The core applies a save to its executor through its file watcher, which can start
            // after the management API answers and miss a save: each probe saves again.
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let put = call(
                    core.port,
                    &Method::PUT,
                    "/v0/management/claude-api-key",
                    &core.management,
                    Some(entries.as_bytes()),
                )
                .await
                .unwrap();
                assert!(put.ok(), "{}", put.status);
                let probe = core
                    .post(
                        "/v1/messages",
                        &[VERSION],
                        &json!({ "model": "claude-opus-5-5", "max_tokens": 8, "messages": message("fingerprint probe") }),
                    )
                    .await;
                assert_eq!(probe.status.as_u16(), 200);
                probes += 1;
                if upstream
                    .last()
                    .header("user-agent")
                    .is_some_and(|agent| agent.starts_with("claude-cli/"))
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the fingerprint profile was not applied"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        for inline in [false, true] {
            for stream in [false, true] {
                let addition = if inline {
                    json!({ "type": "tool_definition", "definition": {
                        "name": "lookup_notes", "input_schema": { "type": "object", "properties": {} },
                    } })
                } else {
                    json!({ "type": "tool_reference", "name": "lookup_notes" })
                };
                let response = core
                    .post(
                        "/v1/messages",
                        &[
                            VERSION,
                            (
                                "anthropic-beta",
                                "mid-conversation-tool-changes-2026-07-01,message-threads-2026-08-12",
                            ),
                        ],
                        &json!({
                            "model": "claude-opus-5-5", "max_tokens": 64, "stream": stream,
                            "tools": [
                                { "name": "read_file", "input_schema": { "type": "object", "properties": {} } },
                                { "name": "lookup_notes", "input_schema": { "type": "object", "properties": {} }, "defer_loading": true },
                            ],
                            "messages": [
                                { "role": "user", "content": "Before tools change" },
                                { "role": "assistant", "content": [{ "type": "text", "text": "ok" }] },
                                { "role": "system", "content": [
                                    { "type": "tool_removal", "tool": { "type": "tool_reference", "name": "read_file" } },
                                    { "type": "tool_addition", "tool": addition },
                                ] },
                                { "role": "user", "content": "After tools change" },
                            ],
                        }),
                    )
                    .await;
                let case = format!("oauth {oauth}, inline {inline}, stream {stream}");
                assert_eq!(
                    response.status.as_u16(),
                    200,
                    "{case}: {}",
                    String::from_utf8_lossy(&response.body)
                );
                let request = upstream.last();
                let betas: Vec<&str> = request
                    .header("anthropic-beta")
                    .unwrap()
                    .split(',')
                    .collect();
                assert!(
                    betas.contains(&"mid-conversation-tool-changes-2026-07-01"),
                    "{case}: {betas:?}"
                );
                assert!(
                    betas.contains(&"message-threads-2026-08-12"),
                    "{case}: {betas:?}"
                );
                assert_eq!(
                    betas.contains(&"oauth-2025-04-20"),
                    oauth,
                    "{case}: {betas:?}"
                );
                if inline && oauth {
                    // OAuth inline definitions need an automatically assembled beta.
                    assert!(
                        betas.contains(&"inline-tools-2026-09-15"),
                        "{case}: {betas:?}"
                    );
                }
                assert_eq!(
                    request.header("authorization"),
                    Some("Bearer synthetic-anthropic")
                );
                let tools = &request.body["tools"];
                let blocks = content_blocks(&request.body);
                let removal = blocks
                    .iter()
                    .find(|block| block["type"] == "tool_removal")
                    .unwrap();
                let added = blocks
                    .iter()
                    .find(|block| block["type"] == "tool_addition")
                    .unwrap();
                assert_eq!(removal["tool"]["name"], tools[0]["name"], "{case}");
                let added = if inline {
                    &added["tool"]["definition"]["name"]
                } else {
                    &added["tool"]["name"]
                };
                assert_eq!(added, &tools[1]["name"], "{case}");
                assert_eq!(tools[1]["defer_loading"], true, "{case}");
                if oauth {
                    // OAuth aliases reach declarations, removals and additions together.
                    assert_ne!(tools[0]["name"], "read_file", "{case}");
                    assert_ne!(tools[1]["name"], "lookup_notes", "{case}");
                }
            }
        }
        core.stop().await;
    }
    assert_eq!(upstream.seen().len() - probes, 8);
    store.close().await;
}

/// The cross-harness provider: a Responses stream on a Responses path, else a Messages stream.
fn harness(seen: &Seen) -> Reply {
    Reply::Events(vec![if seen.path.contains("responses") {
        responses_stream(&seen.body["model"])
    } else {
        messages_stream(&seen.body["model"])
    }])
}

/// Each Messages content block rebuilt from its deltas, as an agent would replay it.
fn rebuilt(received: &[Value]) -> Vec<Value> {
    received
        .iter()
        .filter(|event| event["type"] == "content_block_start")
        .map(|start| {
            let deltas: Vec<&Value> = received
                .iter()
                .filter(|event| {
                    event["type"] == "content_block_delta" && event["index"] == start["index"]
                })
                .map(|event| &event["delta"])
                .collect();
            let joined = |field: &str| -> String {
                deltas
                    .iter()
                    .filter_map(|delta| delta[field].as_str())
                    .collect()
            };
            let mut block = start["content_block"].clone();
            if block["type"] == "tool_use" {
                block["input"] = serde_json::from_str(&joined("partial_json")).unwrap();
            }
            if block["type"] == "thinking" {
                block["thinking"] = json!(joined("thinking"));
                block["signature"] = json!(joined("signature"));
            }
            block
        })
        .collect()
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "both harnesses' turns and replays, in order"
)]
async fn the_pinned_core_translates_both_harness_tool_streams_and_replays_signed_reasoning() {
    let root = TempDir::new("cross-harness");
    let data = root.path().join("data");
    let store = open(&data).await;
    let upstream = provider(harness).await;
    let base = format!("http://127.0.0.1:{}/v1", upstream.port);
    // The production core refreshes a mutable remote catalog; this wrapper adds --local-model
    // only here, so the catalog stays fixed.
    let wrapper = root.path().join("core-local-catalog-wrapper");
    let binary = pinned_core();
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexec '{}' \"$@\" --local-model\n",
            binary.display().to_string().replace('\'', "'\\''")
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &wrapper,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let core = Running::start(
        Path::new(&wrapper),
        &store,
        &data,
        vec![
            (
                account("responses", &base, &[("gpt-6.1-sol", "responses")]),
                "synthetic-responses",
            ),
            (
                account("anthropic", &base, &[(MODEL, "anthropic")]),
                "synthetic-anthropic",
            ),
        ],
        |_| {},
    )
    .await;
    let send = async |path: &str, body: &Value| {
        let response = core.post(path, &[VERSION], body).await;
        assert_eq!(
            response.status.as_u16(),
            200,
            "{path}: {}",
            String::from_utf8_lossy(&response.body)
        );
        events(&response.body)
    };
    let catalog = core
        .request(
            &Method::GET,
            "/v1/models?client_version=0.140.0",
            &[VERSION],
            None,
        )
        .await;
    assert_eq!(catalog.status.as_u16(), 200);
    let catalog = json_of(&catalog);
    let models = catalog["models"].as_array().unwrap();
    let mut slugs: Vec<&str> = models
        .iter()
        .map(|model| model["slug"].as_str().unwrap())
        .collect();
    slugs.sort_unstable();
    assert_eq!(slugs, ["claude-sonnet-4-6", "gpt-6.1-sol"]);
    for model in models {
        assert!(model["supported_reasoning_levels"].is_array(), "{model}");
    }

    // Claude Code's Messages against a Responses model.
    let claude = json!({
        "model": "gpt-6.1-sol", "stream": true, "max_tokens": 4096,
        "thinking": { "type": "adaptive" }, "output_config": { "effort": "high" },
        "messages": [{ "role": "user", "content": "Read both files." }],
        "tools": [{ "name": "read_fixture", "description": "Read a synthetic fixture", "input_schema": schema() }],
    });
    let received = send("/v1/messages", &claude).await;
    let blocks = rebuilt(&received);
    let calls: Vec<&Value> = blocks
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .collect();
    assert_eq!(
        calls.iter().map(|call| &call["input"]).collect::<Vec<_>>(),
        inputs().iter().collect::<Vec<_>>()
    );
    assert!(calls.iter().all(|call| call["name"] == "read_fixture"));
    assert_ne!(calls[0]["id"], calls[1]["id"]);
    assert_eq!(received.last().unwrap()["type"], "message_stop");
    let delta = received
        .iter()
        .find(|event| event["type"] == "message_delta")
        .unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    let seen = upstream.seen();
    assert_eq!(seen[0].body["reasoning"]["effort"], "high");
    assert_eq!(seen[0].body["tools"][0]["parameters"], schema());
    let mut replay = claude.clone();
    let results: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, call)| json!({ "type": "tool_result", "tool_use_id": call["id"], "content": format!("fixture-result-{i}") }))
        .collect();
    let messages = replay["messages"].as_array_mut().unwrap();
    messages.push(json!({ "role": "assistant", "content": blocks }));
    messages.push(json!({ "role": "user", "content": results }));
    send("/v1/messages", &replay).await;
    let input = upstream.seen()[1].body["input"].clone();
    let input = input.as_array().unwrap();
    let outputs: Vec<(&Value, &Value)> = input
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| (&item["call_id"], &item["output"]))
        .collect();
    let expected: Vec<(Value, Value)> = calls
        .iter()
        .enumerate()
        .map(|(i, call)| (call["id"].clone(), json!(format!("fixture-result-{i}"))))
        .collect();
    assert_eq!(
        outputs,
        expected
            .iter()
            .map(|(id, output)| (id, output))
            .collect::<Vec<_>>()
    );
    let reasoning = input
        .iter()
        .find(|item| item["type"] == "reasoning")
        .unwrap();
    assert_eq!(reasoning["encrypted_content"], ENCRYPTED);

    // Codex's Responses against a Messages model.
    let codex = json!({
        "model": MODEL, "stream": true,
        "reasoning": { "effort": "high", "summary": "auto" },
        "input": [{ "role": "user", "content": "Read both files." }],
        "tools": [{ "type": "function", "name": "read_fixture", "description": "Read a synthetic fixture", "parameters": schema() }],
    });
    let received = send("/v1/responses", &codex).await;
    let output = received
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap()["response"]["output"]
        .as_array()
        .unwrap()
        .clone();
    let functions: Vec<&Value> = output
        .iter()
        .filter(|item| item["type"] == "function_call")
        .collect();
    assert_eq!(
        functions
            .iter()
            .map(|item| serde_json::from_str::<Value>(item["arguments"].as_str().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        inputs()
    );
    assert!(functions.iter().all(|item| item["name"] == "read_fixture"));
    let seen = upstream.seen();
    assert_eq!(seen[2].body["thinking"]["type"], "adaptive");
    assert_eq!(seen[2].body["output_config"]["effort"], "high");
    assert_eq!(seen[2].body["tools"][0]["input_schema"], schema());
    let mut replay = codex.clone();
    let items = replay["input"].as_array_mut().unwrap();
    items.extend(output.iter().cloned());
    items.extend(functions.iter().enumerate().map(|(i, item)| {
        json!({ "type": "function_call_output", "call_id": item["call_id"], "output": format!("fixture-result-{i}") })
    }));
    send("/v1/responses", &replay).await;
    let seen = upstream.seen();
    let content = content_blocks(&seen[3].body);
    let thinking = content
        .iter()
        .find(|block| block["type"] == "thinking")
        .unwrap();
    assert_eq!(thinking["signature"], SIGNATURE);
    let results: Vec<(&Value, &Value)> = content
        .iter()
        .filter(|block| block["type"] == "tool_result")
        .map(|block| (&block["tool_use_id"], &block["content"]))
        .collect();
    let expected: Vec<(Value, Value)> = functions
        .iter()
        .enumerate()
        .map(|(i, item)| {
            (
                item["call_id"].clone(),
                json!(format!("fixture-result-{i}")),
            )
        })
        .collect();
    assert_eq!(
        results,
        expected
            .iter()
            .map(|(id, output)| (id, output))
            .collect::<Vec<_>>()
    );
    let keys: Vec<Option<&str>> = seen
        .iter()
        .map(|request| {
            request
                .header("authorization")
                .or(request.header("x-api-key"))
        })
        .collect();
    assert_eq!(keys[..2], [Some("Bearer synthetic-responses"); 2]);
    assert!(
        keys[2..].iter().all(|key| matches!(
            key,
            Some("Bearer synthetic-anthropic" | "synthetic-anthropic")
        )),
        "{keys:?}"
    );
    assert_eq!(seen.len(), 4);
    core.stop().await;
    store.close().await;
}
