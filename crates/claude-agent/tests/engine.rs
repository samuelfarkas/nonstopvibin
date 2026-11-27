//! The engine behind the real pinned core (`bun run core:install` puts it at `.vendor/core`),
//! with a fake `claude`: harness requests in Chat Completions and Responses form reach the
//! engine as the core translates them, and tool calls round-trip through the engine's MCP
//! server.
//!
//! The fake `claude` is this binary run with claude's arguments (`-p …`): it reads stream-json
//! on stdin, answers `initialize`, speaks MCP over HTTP to the URL in `--mcp-config` as Claude
//! Code does, carrying `_meta.claudecode/toolUseId`, and writes scripted stream events. It logs
//! what it did to `fake.jsonl` in its `CLAUDE_CONFIG_DIR`. Run otherwise, it runs the tests,
//! filtered by any name fragments given.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::indexing_slicing,
    reason = "a test binary without libtest: a panic is the failure report, stdout the log"
)]

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use hyper::Request;
use hyper::body::Incoming;
use hyper::service::service_fn;
use nsv_claude_agent::{Account, Engine, Host, Options};
use nsv_core::binary::{Binary, CoreTarget, bundled_pin};
use nsv_core::config::{applied, write_config};
use nsv_core::processes::{CoreProcess, spawn_core};
use nsv_domain::id::ProfileId;
use nsv_domain::secret::ApiKey;
use nsv_domain::wire::ApiAccount;
use nsv_files::DataDir;
use nsv_net::{Loopback, LoopbackRequest, Method, loopback};
use nsv_store::{Store, Vault};
use nsv_testkit::{Port, TempDir};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const MODEL: &str = "claude-opus-5-5";
const AGENT_KEY: &str = "nvc_test-agent-key";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("-p") {
        fake::run(&args[1..]);
        return ExitCode::SUCCESS;
    }
    run_tests(&args[1..])
}

// ---- The fake claude ----

mod fake {
    use std::fs::OpenOptions;
    use std::io::{BufRead, Read, Write};
    use std::net::TcpStream;
    use std::path::Path;

    use serde_json::{Value, json};

    fn log(entry: &Value) {
        let config = std::env::var("CLAUDE_CONFIG_DIR").unwrap();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(Path::new(&config).join("fake.jsonl"))
            .unwrap();
        writeln!(file, "{entry}").unwrap();
    }

    fn emit(line: &Value) {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{line}").unwrap();
        out.flush().unwrap();
    }

    fn event(event: &Value) {
        emit(&json!({ "type": "stream_event", "event": event, "parent_tool_use_id": null }));
    }

    fn start(id: &str) {
        event(&json!({ "type": "message_start", "message": {
            "id": id, "type": "message", "role": "assistant", "model": super::MODEL, "content": [],
            "stop_reason": null, "stop_sequence": null, "usage": { "input_tokens": 11, "output_tokens": 0 },
        } }));
    }

    fn text(index: u64, text: &str) {
        event(
            &json!({ "type": "content_block_start", "index": index, "content_block": { "type": "text", "text": "" } }),
        );
        event(
            &json!({ "type": "content_block_delta", "index": index, "delta": { "type": "text_delta", "text": text } }),
        );
        event(&json!({ "type": "content_block_stop", "index": index }));
    }

    fn tool_use(index: u64, id: &str, name: &str, input: &Value) {
        event(
            &json!({ "type": "content_block_start", "index": index, "content_block": {
            "type": "tool_use", "id": id, "name": format!("mcp__client__{name}"), "input": {},
        } }),
        );
        event(
            &json!({ "type": "content_block_delta", "index": index, "delta": {
            "type": "input_json_delta", "partial_json": input.to_string(),
        } }),
        );
        event(&json!({ "type": "content_block_stop", "index": index }));
    }

    fn stop(reason: &str) {
        event(
            &json!({ "type": "message_delta", "delta": { "stop_reason": reason, "stop_sequence": null }, "usage": { "output_tokens": 7 } }),
        );
        event(&json!({ "type": "message_stop" }));
    }

    fn result(session: &str, error: Option<&str>) {
        emit(&json!({
            "type": "result", "subtype": error.unwrap_or("success"), "is_error": error.is_some(),
            "stop_reason": "end_turn", "session_id": session,
        }));
    }

    /// The MCP server in `--mcp-config`: its URL and Authorization header.
    struct Mcp {
        url: String,
        authorization: String,
        next: u64,
    }

    impl Mcp {
        /// One JSON-RPC exchange: the answer with `id`, from JSON or an event stream.
        fn call(&mut self, method: &str, params: &Value) -> Value {
            self.next += 1;
            let id = self.next;
            let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
            let response = self.post(&body);
            let found = response
                .lines()
                .filter_map(|line| line.strip_prefix("data: ").or(Some(line)))
                .filter_map(|data| serde_json::from_str::<Value>(data).ok())
                .find(|message| message["id"] == id);
            found.unwrap_or_else(|| panic!("no answer to {method}: {response}"))
        }

        fn notify(&self, method: &str) {
            self.post(&json!({ "jsonrpc": "2.0", "method": method }));
        }

        fn post(&self, body: &Value) -> String {
            let (host, path) = self
                .url
                .strip_prefix("http://")
                .unwrap()
                .split_once('/')
                .unwrap();
            let mut stream = TcpStream::connect(host).unwrap();
            let body = body.to_string();
            write!(
                stream,
                "POST /{path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                self.authorization,
                body.len()
            )
            .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            let split = response
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap();
            let head = String::from_utf8_lossy(&response[..split]).to_lowercase();
            let body = &response[split + 4..];
            let body = if head.contains("transfer-encoding: chunked") {
                nsv_testkit::dechunk(body)
            } else {
                body.to_vec()
            };
            String::from_utf8(body).unwrap()
        }
    }

    fn mcp(path: &str) -> Mcp {
        let config: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let client = &config["mcpServers"]["client"];
        Mcp {
            url: client["url"].as_str().unwrap().to_owned(),
            authorization: client["headers"]["Authorization"]
                .as_str()
                .unwrap()
                .to_owned(),
            next: 0,
        }
    }

    /// Calls the harness's tool for `tool_use`, as Claude Code does after the message stops.
    fn call_tool(mcp: &mut Mcp, tool: &str, tool_use: &str) -> String {
        let answer = mcp.call(
            "tools/call",
            &json!({ "name": tool, "arguments": {}, "_meta": { "claudecode/toolUseId": tool_use, "progressToken": mcp.next + 1 } }),
        );
        let text = answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        log(&json!({ "called": tool_use, "result": text }));
        text
    }

    pub(super) fn run(args: &[String]) {
        let value = |flag: &str| {
            args.iter()
                .position(|arg| arg == flag)
                .and_then(|at| args.get(at + 1))
                .cloned()
        };
        log(&json!({
            "start": args,
            "entrypoint": std::env::var("CLAUDE_CODE_ENTRYPOINT").ok(),
            "max_tokens": std::env::var("CLAUDE_CODE_MAX_OUTPUT_TOKENS").ok(),
        }));
        let session = value("--session-id")
            .or_else(|| value("--resume"))
            .unwrap_or_else(|| "side".to_owned());
        let mut server = value("--mcp-config").map(|path| mcp(&path));
        let mut tools: Vec<String> = Vec::new();
        let mut started = false;
        let mut turns = 0;
        let stdin = std::io::stdin();
        let mut lines = stdin.lock().lines();
        while let Some(Ok(line)) = lines.next() {
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["type"] == "control_request" {
                emit(&json!({ "type": "control_response", "response": {
                    "subtype": "success", "request_id": message["request_id"], "response": {},
                } }));
                continue;
            }
            if !started {
                started = true;
                emit(&json!({ "type": "system", "subtype": "init", "session_id": session }));
                if let Some(server) = &mut server {
                    server.call(
                        "initialize",
                        &json!({ "protocolVersion": "2025-11-25", "capabilities": {} }),
                    );
                    server.notify("notifications/initialized");
                    let listed = server.call("tools/list", &json!({}));
                    tools = listed["result"]["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|tool| tool["name"].as_str().unwrap().to_owned())
                        .collect();
                }
            }
            turns += 1;
            let said: String = message["message"]["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect();
            log(&json!({ "turn": said }));
            let id = format!("msg_{}_{turns}", std::process::id());
            let call = |n: u64| format!("toolu_{}_{turns}_{n}", std::process::id());
            if said.contains("wait forever") {
                start(&id);
                text(0, "Working");
                // Until the engine interrupts.
                for line in lines.by_ref() {
                    let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
                    if message["request"]["subtype"] == "interrupt" {
                        log(&json!({ "interrupted": true }));
                        emit(&json!({ "type": "control_response", "response": {
                            "subtype": "success", "request_id": message["request_id"], "response": {},
                        } }));
                        result(&session, Some("error_during_execution"));
                        break;
                    }
                }
            } else if let (Some(server), Some(tool), true) =
                (&mut server, tools.first(), said.contains("tool"))
            {
                let count = if said.contains("two tools") { 2 } else { 1 };
                start(&id);
                text(0, "Reading.");
                for n in 1..=count {
                    tool_use(n, &call(n), tool, &json!({ "path": format!("file-{n}") }));
                }
                stop("tool_use");
                let results: Vec<String> = (1..=count)
                    .map(|n| call_tool(server, tool, &call(n)))
                    .collect();
                start(&format!("{id}_2"));
                text(0, &format!("results: {}", results.join(" | ")));
                stop("end_turn");
                result(&session, None);
            } else {
                start(&id);
                text(0, &format!("echo: {said}"));
                stop("end_turn");
                result(&session, None);
            }
        }
    }
}

// ---- The world: a store, the real core and the engine on a daemon port ----

struct Agent;

impl Host for Agent {
    fn account(&self, _: &ProfileId) -> impl Future<Output = Option<Account>> + Send {
        std::future::ready(Some(Account {
            key: ApiKey::new(AGENT_KEY),
            models: vec![MODEL.to_owned()],
        }))
    }
}

fn pinned_core() -> PathBuf {
    let directory = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.vendor/core"));
    assert!(
        directory.join("cli-proxy-api").exists(),
        "the pinned core is missing: run bun run core:install"
    );
    nsv_core::check_core_directory(
        bundled_pin().unwrap(),
        directory,
        CoreTarget::current().unwrap(),
        Binary::Pinned,
    )
    .unwrap();
    directory.join("cli-proxy-api")
}

struct World {
    _root: TempDir,
    data: DataDir,
    store: Store,
    profile: ProfileId,
    engine: Engine<Agent>,
    core: CoreProcess,
    port: u16,
    key: String,
}

impl World {
    async fn new(name: &str) -> Self {
        let root = TempDir::new(name);
        let data = DataDir::new(root.path().join("data"));
        nsv_files::owner_only_directory(data.path(), || Ok::<(), std::io::Error>(())).unwrap();
        let store = Store::open(&data, Vault::open(&data).unwrap())
            .await
            .unwrap();
        let profile = store
            .call(|db| {
                let profile = db.create_profile("Claude Agent")?;
                let account: ApiAccount = serde_json::from_value(json!({
                    "id": "agent", "name": "Claude Agent", "provider": "claude-agent",
                    "baseUrl": "", "prefix": "",
                    "models": [{ "id": MODEL, "protocol": "anthropic" }], "disabled": false,
                }))
                .unwrap();
                db.save_api_account(&profile.id, &account, Some(&ApiKey::new(AGENT_KEY)))?;
                Ok(profile.id)
            })
            .await
            .unwrap();
        // The daemon's loopback listener, serving only the engine.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let daemon = listener.local_addr().unwrap().port();
        let engine = Engine::new(
            Agent,
            Options {
                data: data.clone(),
                port: daemon,
                program: Some(std::env::current_exe().unwrap()),
            },
        );
        let served = engine.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let engine = served.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let engine = engine.clone();
                        async move {
                            let path = request.uri().path().to_owned();
                            Ok::<_, Infallible>(engine.answer(request, &path).await)
                        }
                    });
                    let _ = nsv_gateway::serve_connection(stream, service, std::future::pending())
                        .await;
                });
            }
        });
        let reserved = Port::reserve();
        let port = reserved.number();
        let config = write_config(&store, profile.clone(), port, Some(daemon))
            .await
            .unwrap();
        drop(reserved);
        let core = spawn_core(&pinned_core(), &data, &profile).unwrap();
        let management = config.management_key.expose_secret().to_owned();
        let world = Self {
            _root: root,
            data,
            store,
            profile,
            engine,
            core,
            port,
            key: config.core_key.expose_secret().to_owned(),
        };
        let reported = world.reported(&management).await;
        assert!(applied(&config, &reported), "{reported}");
        world
    }

    async fn reported(&self, management: &str) -> Value {
        let started = Instant::now();
        loop {
            let authorization = format!("Bearer {management}");
            let response = loopback(LoopbackRequest {
                to: Loopback::v4(self.port),
                method: &Method::GET,
                target: "/v0/management/config",
                headers: &[("Authorization", &authorization)],
                body: None,
                timeout: Duration::from_secs(5),
                limit: 1 << 20,
            })
            .await;
            if let Ok(response) = response
                && response.ok()
            {
                return serde_json::from_slice(&response.body).unwrap();
            }
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "the core did not start"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A harness request to the core with its key; the JSON answer.
    async fn post(&self, target: &str, body: &Value) -> Value {
        let authorization = format!("Bearer {}", self.key);
        let body = body.to_string();
        let response = loopback(LoopbackRequest {
            to: Loopback::v4(self.port),
            method: &Method::POST,
            target,
            headers: &[
                ("Authorization", &authorization),
                ("Content-Type", "application/json"),
            ],
            body: Some(body.as_bytes()),
            timeout: Duration::from_secs(30),
            limit: 16 << 20,
        })
        .await
        .unwrap();
        let text = String::from_utf8_lossy(&response.body).into_owned();
        assert!(response.ok(), "{}: {text}", response.status);
        serde_json::from_str(&text).unwrap()
    }

    /// What the fake claude processes logged, in order.
    fn log(&self) -> Vec<Value> {
        let path = self
            .data
            .claude_agent_config(&self.profile)
            .join("fake.jsonl");
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn starts(&self) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter_map(|entry| entry.get("start").cloned())
            .collect()
    }

    async fn stop(mut self) {
        self.engine.close().await;
        self.core.terminate().await.unwrap();
        self.store.close().await;
    }
}

fn read_tool() -> Value {
    json!({ "type": "function", "function": {
        "name": "read", "description": "Reads a file.",
        "parameters": { "type": "object", "properties": { "path": { "type": "string" } } },
    } })
}

// ---- The tests ----

async fn a_chat_completions_tool_call_round_trips_through_one_claude_process() {
    let world = World::new("cae-chat").await;
    let user = json!({ "role": "user", "content": "please use the tool" });
    let first = world
        .post(
            "/v1/chat/completions",
            &json!({ "model": MODEL, "messages": [user], "tools": [read_tool()] }),
        )
        .await;
    let message = &first["choices"][0]["message"];
    assert_eq!(
        first["choices"][0]["finish_reason"], "tool_calls",
        "{first}"
    );
    let call = &message["tool_calls"][0];
    assert_eq!(call["function"]["name"], "read", "{first}");
    assert_eq!(
        serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap(),
        json!({ "path": "file-1" })
    );
    let id = call["id"].as_str().unwrap();
    let assistant = json!({ "role": "assistant", "content": message["content"], "tool_calls": message["tool_calls"] });
    let answered = json!({ "role": "tool", "tool_call_id": id, "content": "the file body" });
    let second = world
        .post(
            "/v1/chat/completions",
            &json!({ "model": MODEL, "messages": [user, assistant, answered], "tools": [read_tool()] }),
        )
        .await;
    let reply = &second["choices"][0]["message"]["content"];
    assert_eq!(reply, "results: the file body", "{second}");
    // A next message goes into the same process.
    let next = json!({ "role": "user", "content": "thanks" });
    let reply_turn = json!({ "role": "assistant", "content": reply });
    let third = world
        .post(
            "/v1/chat/completions",
            &json!({
                "model": MODEL,
                "messages": [user, assistant, answered, reply_turn, next],
                "tools": [read_tool()],
            }),
        )
        .await;
    assert_eq!(third["choices"][0]["message"]["content"], "echo: thanks");
    let starts = world.starts();
    assert_eq!(starts.len(), 1, "{:?}", world.log());
    let log = world.log();
    assert_eq!(log[0]["entrypoint"], "nonstopvibin");
    assert!(
        log.iter().any(|entry| entry["called"].as_str() == Some(id)),
        "{log:?}"
    );
    world.stop().await;
}

async fn a_responses_tool_call_round_trips_through_one_claude_process() {
    let world = World::new("cae-responses").await;
    let tool = json!({
        "type": "function", "name": "read", "description": "Reads a file.",
        "parameters": { "type": "object", "properties": { "path": { "type": "string" } } },
    });
    let user = json!({ "role": "user", "content": [{ "type": "input_text", "text": "please use the tool" }] });
    let first = world
        .post(
            "/v1/responses",
            &json!({ "model": MODEL, "input": [user], "tools": [tool] }),
        )
        .await;
    let output = first["output"].as_array().unwrap();
    let call = output
        .iter()
        .find(|item| item["type"] == "function_call")
        .unwrap_or_else(|| panic!("{first}"));
    assert_eq!(call["name"], "read");
    let call_id = call["call_id"].as_str().unwrap();
    let mut input = vec![user];
    input.extend(output.iter().cloned());
    input.push(
        json!({ "type": "function_call_output", "call_id": call_id, "output": "the file body" }),
    );
    let second = world
        .post(
            "/v1/responses",
            &json!({ "model": MODEL, "input": input, "tools": [tool] }),
        )
        .await;
    let text: String = second["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "message")
        .flat_map(|item| item["content"].as_array().unwrap().clone())
        .filter_map(|part| part["text"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(text, "results: the file body", "{second}");
    assert_eq!(world.starts().len(), 1, "{:?}", world.log());
    world.stop().await;
}

async fn two_parallel_tool_calls_are_answered_in_one_request() {
    let world = World::new("cae-parallel").await;
    let user = json!({ "role": "user", "content": "use two tools" });
    let first = world
        .post(
            "/v1/chat/completions",
            &json!({ "model": MODEL, "messages": [user], "tools": [read_tool()] }),
        )
        .await;
    let message = &first["choices"][0]["message"];
    let calls = message["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2, "{first}");
    let answers: Vec<Value> = calls
        .iter()
        .zip(["first body", "second body"])
        .map(|(call, body)| json!({ "role": "tool", "tool_call_id": call["id"], "content": body }))
        .collect();
    let mut messages = vec![
        user,
        json!({ "role": "assistant", "content": message["content"], "tool_calls": calls }),
    ];
    messages.extend(answers);
    let second = world
        .post(
            "/v1/chat/completions",
            &json!({ "model": MODEL, "messages": messages, "tools": [read_tool()] }),
        )
        .await;
    assert_eq!(
        second["choices"][0]["message"]["content"], "results: first body | second body",
        "{second}"
    );
    assert_eq!(world.starts().len(), 1);
    world.stop().await;
}

async fn a_client_that_hangs_up_interrupts_the_turn() {
    let world = World::new("cae-hangup").await;
    let body = json!({
        "model": MODEL, "stream": true, "tools": [read_tool()],
        "messages": [{ "role": "user", "content": "wait forever" }],
    })
    .to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", world.port)).await.unwrap();
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        world.port,
        world.key,
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut seen = Vec::new();
    let mut buffer = [0; 4096];
    let started = Instant::now();
    while !String::from_utf8_lossy(&seen).contains("Working") {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "no delta: {}",
            String::from_utf8_lossy(&seen)
        );
        let read = tokio::time::timeout(Duration::from_secs(20), stream.read(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert!(read > 0, "closed early: {}", String::from_utf8_lossy(&seen));
        seen.extend_from_slice(&buffer[..read]);
    }
    drop(stream);
    let started = Instant::now();
    while !world.log().iter().any(|entry| entry["interrupted"] == true) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "not interrupted: {:?}",
            world.log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    world.stop().await;
}

fn block_on(test: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(test);
}

fn run_tests(arguments: &[String]) -> ExitCode {
    let tests: [(&str, fn()); 4] = [
        (
            "a_chat_completions_tool_call_round_trips_through_one_claude_process",
            || block_on(a_chat_completions_tool_call_round_trips_through_one_claude_process()),
        ),
        (
            "a_responses_tool_call_round_trips_through_one_claude_process",
            || block_on(a_responses_tool_call_round_trips_through_one_claude_process()),
        ),
        (
            "two_parallel_tool_calls_are_answered_in_one_request",
            || block_on(two_parallel_tool_calls_are_answered_in_one_request()),
        ),
        ("a_client_that_hangs_up_interrupts_the_turn", || {
            block_on(a_client_that_hangs_up_interrupts_the_turn());
        }),
    ];
    let filters: Vec<&String> = arguments.iter().filter(|a| !a.starts_with('-')).collect();
    let mut passed = 0;
    for (name, test) in tests {
        if filters.is_empty() || filters.iter().any(|filter| name.contains(filter.as_str())) {
            println!("test {name} ...");
            test();
            println!("test {name} ... ok");
            passed += 1;
        }
    }
    println!("test result: ok. {passed} passed");
    ExitCode::SUCCESS
}
