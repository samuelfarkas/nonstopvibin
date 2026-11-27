//! Claude Code's stream-json output turned into the Anthropic answer the core expects.
//!
//! A [`Translator`] reads one process's stdout lines. For each HTTP response it re-indexes
//! content blocks, strips the `mcp__client__` prefix from the harness's tool names and drops
//! calls to tools the harness did not offer, which Claude Code answers itself. A message that
//! stopped only on such calls is merged with the next one, so the harness sees one message.
//! A response ends at the message that calls the harness's tools ([`Step::Hold`]), or when
//! the turn's result arrives ([`Step::Done`], [`Step::Failed`]).

use std::collections::HashMap;
use std::sync::Arc;

use hyper::body::Bytes;
use serde_json::{Map, Value, json};

use crate::conversation::{Block, Digest, Role, Tool, Turn};

/// The prefix Claude Code gives the tools of the MCP server the engine names `client`.
const PREFIX: &str = "mcp__client__";

/// One line of Claude Code's stdout, as far as the engine reads it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Line {
    /// The session's id, from `system/init`.
    Init {
        session: String,
    },
    /// A stream event of the main conversation.
    Event(Value),
    /// When a rejected rate limit resets, in seconds since the epoch.
    RateLimit {
        resets_at: Option<u64>,
    },
    Result(Outcome),
    /// The answer to a control request.
    Control {
        request_id: String,
        response: Value,
    },
    Other,
}

/// A turn's `result` line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub(crate) error: bool,
    pub(crate) subtype: Option<String>,
    pub(crate) status: Option<u16>,
    pub(crate) kind: Option<String>,
    pub(crate) terminal: Option<String>,
    pub(crate) text: Option<String>,
}

fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

/// Parses one stdout line; anything the engine does not read is [`Line::Other`].
pub(crate) fn parse_line(line: &str) -> Line {
    let Ok(Value::Object(line)) = serde_json::from_str::<Value>(line) else {
        return Line::Other;
    };
    match line.get("type").and_then(Value::as_str) {
        Some("system") if line.get("subtype").and_then(Value::as_str) == Some("init") => {
            text(line.get("session_id")).map_or(Line::Other, |session| Line::Init { session })
        }
        // Subagents' events carry the tool call they belong to; the engine runs none.
        Some("stream_event") if line.get("parent_tool_use_id").is_none_or(Value::is_null) => {
            line.get("event").cloned().map_or(Line::Other, Line::Event)
        }
        Some("rate_limit_event") => {
            let resets = line
                .get("rate_limit_info")
                .and_then(|info| info.get("resetsAt"))
                .and_then(Value::as_u64)
                // Milliseconds, if the value is too large for seconds.
                .map(|at| if at > 100_000_000_000 { at / 1000 } else { at });
            Line::RateLimit { resets_at: resets }
        }
        Some("result") => Line::Result(Outcome {
            error: line.get("is_error").and_then(Value::as_bool) == Some(true),
            subtype: text(line.get("subtype")),
            status: line
                .get("api_error_status")
                .and_then(Value::as_u64)
                .and_then(|status| u16::try_from(status).ok()),
            kind: text(line.get("api_error_type")),
            terminal: text(line.get("terminal_reason")),
            text: text(line.get("result")),
        }),
        Some("control_response") => {
            let response = line.get("response").cloned().unwrap_or(Value::Null);
            text(response.get("request_id")).map_or(Line::Other, |request_id| Line::Control {
                request_id,
                response,
            })
        }
        _ => Line::Other,
    }
}

/// The name Claude Code gives an MCP tool: characters other than letters, digits, `_` and `-`
/// become `_`.
fn claude_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The harness's tools by the name Claude Code calls them.
#[derive(Debug, Default)]
pub(crate) struct Offered(HashMap<String, String>);

impl Offered {
    pub(crate) fn new(tools: &[Tool]) -> Self {
        Self(
            tools
                .iter()
                .map(|tool| {
                    (
                        format!("{PREFIX}{}", claude_name(&tool.name)),
                        tool.name.clone(),
                    )
                })
                .collect(),
        )
    }

    /// The harness's name for the tool Claude Code calls `name`, if the harness offered it.
    fn original(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }
}

/// An error answer: the HTTP status and the Anthropic error body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) status: u16,
    pub(crate) kind: &'static str,
    pub(crate) message: String,
    /// When a rate limit resets, in seconds since the epoch, if Claude Code said.
    pub(crate) retry_at: Option<u64>,
}

impl Failure {
    pub(crate) fn new(status: u16, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            retry_at: None,
        }
    }

    pub(crate) fn body(&self) -> Value {
        json!({ "type": "error", "error": { "type": self.kind, "message": self.message } })
    }
}

/// The answer for a turn Claude Code ended with an error: a status harnesses act on.
pub(crate) fn failure(outcome: &Outcome, retry_at: Option<u64>, sign_in: &str) -> Failure {
    let said = outcome.text.as_deref().unwrap_or_default();
    let lower = said.to_lowercase();
    let kind = outcome.kind.as_deref();
    if outcome.terminal.as_deref() == Some("prompt_too_long")
        || lower.contains("prompt is too long")
    {
        return Failure::new(400, "invalid_request_error", "prompt is too long");
    }
    if outcome.status == Some(429) || kind == Some("rate_limit_error") {
        let message = if said.is_empty() {
            "Claude Code reported a rate limit."
        } else {
            said
        };
        return Failure {
            retry_at,
            ..Failure::new(429, "rate_limit_error", message)
        };
    }
    if outcome.status == Some(529) || kind == Some("overloaded_error") {
        return Failure::new(529, "overloaded_error", "Overloaded");
    }
    if outcome.status == Some(401)
        || kind == Some("authentication_error")
        || lower.contains("not logged in")
    {
        return Failure::new(
            401,
            "authentication_error",
            format!("Claude Code is not signed in for this profile. Sign in with: {sign_in}"),
        );
    }
    let message = if said.is_empty() {
        format!(
            "Claude Code ended the turn: {}.",
            outcome.subtype.as_deref().unwrap_or("error")
        )
    } else {
        said.to_owned()
    };
    Failure::new(502, "api_error", message)
}

/// Sets `field` of `object`, if it is an object.
fn set(object: &mut Value, field: &str, value: impl Into<Value>) {
    if let Some(object) = object.as_object_mut() {
        object.insert(field.to_owned(), value.into());
    }
}

/// One SSE frame.
pub(crate) fn frame(name: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {name}\ndata: {data}\n\n"))
}

/// What the response currently being written gets next.
#[derive(Debug, PartialEq)]
pub(crate) enum Step {
    /// Event-stream bytes, for a streaming response.
    Send(Bytes),
    /// The response ends on the harness's tool calls; the session holds them. A
    /// non-streaming response's body comes with it.
    Hold {
        calls: Vec<String>,
        turn: Digest,
        body: Option<Bytes>,
    },
    /// The turn ended.
    Done { turn: Digest, body: Option<Bytes> },
    /// The turn failed. Before any output this is the error answer; after it, an `error`
    /// event was sent.
    Failed(Option<Failure>),
}

/// The response being written: what went out, assembled as one message.
#[derive(Debug, Default)]
struct Response {
    stream: bool,
    /// A message started; a streaming response has sent it.
    started: bool,
    next: u64,
    head: Option<Map<String, Value>>,
    content: Vec<Value>,
    partial: HashMap<u64, String>,
    delta: Option<Map<String, Value>>,
    /// Output tokens of merged messages, added to the last one's.
    merged_tokens: u64,
}

impl Response {
    fn add(&mut self, name: &str, event: &Value, out: u64) {
        let slot = usize::try_from(out).ok();
        match name {
            "content_block_start" => {
                let mut block = event.get("content_block").cloned().unwrap_or(Value::Null);
                if let Some(fields) = block.as_object_mut() {
                    fields.remove("index");
                }
                self.content.push(block);
            }
            "content_block_delta" => {
                let delta = event.get("delta");
                let block = slot.and_then(|slot| self.content.get_mut(slot));
                let (Some(delta), Some(Value::Object(block))) = (delta, block) else {
                    return;
                };
                for (kind, field) in [
                    ("text_delta", "text"),
                    ("thinking_delta", "thinking"),
                    ("signature_delta", "signature"),
                ] {
                    if delta.get("type").and_then(Value::as_str) == Some(kind) {
                        let piece = delta.get(field).and_then(Value::as_str).unwrap_or_default();
                        match block.get_mut(field) {
                            Some(Value::String(joined)) => joined.push_str(piece),
                            _ => {
                                block.insert(field.to_owned(), Value::String(piece.to_owned()));
                            }
                        }
                    }
                }
                if let Some(json) = delta.get("partial_json").and_then(Value::as_str) {
                    self.partial.entry(out).or_default().push_str(json);
                }
            }
            "content_block_stop" => {
                if let (Some(json), Some(Value::Object(block))) = (
                    self.partial.remove(&out),
                    slot.and_then(|slot| self.content.get_mut(slot)),
                ) {
                    let input = if json.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&json).unwrap_or(Value::Null)
                    };
                    block.insert("input".to_owned(), input);
                }
            }
            _ => {}
        }
    }

    /// The message as a non-streaming answer.
    fn message(&self) -> Bytes {
        let mut message = self.head.clone().unwrap_or_default();
        message.insert("content".to_owned(), Value::Array(self.content.clone()));
        if let Some(delta) = &self.delta {
            for field in ["stop_reason", "stop_sequence"] {
                if let Some(value) = delta.get("delta").and_then(|d| d.get(field)) {
                    message.insert(field.to_owned(), value.clone());
                }
            }
            if let (Some(Value::Object(usage)), Some(Value::Object(more))) =
                (message.get_mut("usage"), delta.get("usage"))
            {
                for (key, value) in more {
                    usage.insert(key.clone(), value.clone());
                }
            }
        }
        Bytes::from(Value::Object(message).to_string())
    }

    /// The assistant turn the harness received, as it will send it back.
    fn turn(&self) -> Digest {
        let blocks = self
            .content
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str)? {
                "text" => Some(Block::Text(
                    block.get("text").and_then(Value::as_str)?.to_owned(),
                )),
                "tool_use" => Some(Block::ToolUse {
                    id: block.get("id").and_then(Value::as_str)?.to_owned(),
                    name: String::new(),
                    input: Value::Null,
                }),
                _ => None,
            })
            .collect();
        Turn {
            role: Role::Assistant,
            blocks,
        }
        .digest()
    }
}

/// The Claude Code message being read.
#[derive(Debug, Default)]
struct Message {
    /// Claude Code's block index to the response's, `None` for a dropped call.
    map: HashMap<u64, Option<u64>>,
    calls: Vec<String>,
    /// It called a tool the harness did not offer.
    dropped: bool,
    /// It stopped only on dropped calls: its stop and the next message's start are hidden.
    merged: bool,
}

/// One process's stdout turned into answers; see the module documentation.
#[derive(Debug)]
pub(crate) struct Translator {
    offered: Arc<Offered>,
    sign_in: Arc<str>,
    response: Response,
    message: Message,
    /// The next `message_start` continues the response's message.
    merge_next: bool,
    /// The response's last message stopped; the turn's result comes next.
    finished: bool,
    /// Claude Code went on past what the harness received.
    diverged: bool,
    retry_at: Option<u64>,
}

impl Translator {
    pub(crate) fn new(offered: Arc<Offered>, sign_in: Arc<str>) -> Self {
        Self {
            offered,
            sign_in,
            response: Response::default(),
            message: Message::default(),
            merge_next: false,
            finished: false,
            diverged: false,
            retry_at: None,
        }
    }

    /// A new HTTP response starts reading; `stream` says whether it is an event stream.
    pub(crate) fn respond(&mut self, stream: bool) {
        self.response = Response {
            stream,
            ..Response::default()
        };
    }

    /// Whether the turn went on beyond what the harness received, so the session no longer
    /// matches the harness's history.
    pub(crate) const fn diverged(&self) -> bool {
        self.diverged
    }

    fn emit(&self, name: &str, event: &Value, steps: &mut Vec<Step>) {
        if self.response.stream {
            steps.push(Step::Send(frame(name, event)));
        }
    }

    /// The steps one stdout line makes.
    pub(crate) fn feed(&mut self, line: &Line) -> Vec<Step> {
        let mut steps = Vec::new();
        match line {
            Line::Event(event) => self.event(event.clone(), &mut steps),
            Line::RateLimit { resets_at } => self.retry_at = resets_at.or(self.retry_at),
            Line::Result(outcome) => self.result(outcome, &mut steps),
            Line::Init { .. } | Line::Control { .. } | Line::Other => {}
        }
        steps
    }

    fn event(&mut self, mut event: Value, steps: &mut Vec<Step>) {
        let name = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if name == "message_start" {
            self.message = Message::default();
            if std::mem::take(&mut self.merge_next) {
                return;
            }
        }
        if self.finished {
            self.diverged = true;
            return;
        }
        match name.as_str() {
            "message_start" => {
                self.response.started = true;
                self.response.head = event.get("message").and_then(Value::as_object).cloned();
                self.emit(&name, &event, steps);
            }
            "content_block_start" => self.block_start(event, steps),
            "content_block_delta" | "content_block_stop" => {
                let index = event.get("index").and_then(Value::as_u64);
                let Some(Some(out)) = index.and_then(|index| self.message.map.get(&index)) else {
                    return;
                };
                let out = *out;
                set(&mut event, "index", out);
                self.response.add(&name, &event, out);
                self.emit(&name, &event, steps);
            }
            "message_delta" => self.message_delta(event, steps),
            "message_stop" => self.message_stop(&event, steps),
            _ => self.emit(&name, &event, steps),
        }
    }

    fn block_start(&mut self, mut event: Value, steps: &mut Vec<Step>) {
        let Some(index) = event.get("index").and_then(Value::as_u64) else {
            return;
        };
        if let Some(block) = event
            .get_mut("content_block")
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        {
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let Some(original) = self.offered.original(name) else {
                self.message.map.insert(index, None);
                self.message.dropped = true;
                return;
            };
            set(block, "name", original);
            let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
            self.message.calls.push(id.to_owned());
        }
        let out = self.response.next;
        self.response.next += 1;
        self.message.map.insert(index, Some(out));
        set(&mut event, "index", out);
        self.response.add("content_block_start", &event, out);
        self.emit("content_block_start", &event, steps);
    }

    fn message_delta(&mut self, mut event: Value, steps: &mut Vec<Step>) {
        let tokens = event
            .get("usage")
            .and_then(|usage| usage.get("output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let stop = event
            .get("delta")
            .and_then(|delta| delta.get("stop_reason"))
            .and_then(Value::as_str);
        if stop == Some("tool_use") && self.message.calls.is_empty() && self.message.dropped {
            self.message.merged = true;
            self.response.merged_tokens += tokens;
            return;
        }
        if self.response.merged_tokens > 0
            && let Some(usage) = event.get_mut("usage").and_then(Value::as_object_mut)
        {
            usage.insert(
                "output_tokens".to_owned(),
                (tokens + self.response.merged_tokens).into(),
            );
        }
        self.response.delta = event.as_object().cloned();
        self.emit("message_delta", &event, steps);
    }

    fn message_stop(&mut self, event: &Value, steps: &mut Vec<Step>) {
        if self.message.merged {
            self.merge_next = true;
            return;
        }
        self.emit("message_stop", event, steps);
        if self.message.calls.is_empty() {
            self.finished = true;
            return;
        }
        let response = std::mem::take(&mut self.response);
        steps.push(Step::Hold {
            calls: std::mem::take(&mut self.message.calls),
            turn: response.turn(),
            body: (!response.stream).then(|| response.message()),
        });
        self.response.stream = response.stream;
    }

    fn result(&mut self, outcome: &Outcome, steps: &mut Vec<Step>) {
        let failed = if outcome.error {
            Some(failure(outcome, self.retry_at, &self.sign_in))
        } else if !self.finished {
            Some(Failure::new(
                502,
                "api_error",
                "Claude Code ended the turn without an answer.",
            ))
        } else {
            None
        };
        let response = std::mem::take(&mut self.response);
        match failed {
            Some(failure) if response.stream && response.started => {
                steps.push(Step::Send(frame("error", &failure.body())));
                steps.push(Step::Failed(None));
            }
            Some(failure) => steps.push(Step::Failed(Some(failure))),
            None => steps.push(Step::Done {
                turn: response.turn(),
                body: (!response.stream).then(|| response.message()),
            }),
        }
        self.message = Message::default();
        self.merge_next = false;
        self.finished = false;
        self.retry_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGN_IN: &str = "CLAUDE_CONFIG_DIR=/data/claude-agent/p claude auth login";

    fn translator(tools: &[&str]) -> Translator {
        let tools: Vec<Tool> = tools
            .iter()
            .map(|name| Tool {
                name: (*name).to_owned(),
                description: String::new(),
                schema: json!({ "type": "object" }),
            })
            .collect();
        Translator::new(Arc::new(Offered::new(&tools)), Arc::from(SIGN_IN))
    }

    fn lines(fixture: &str) -> Vec<Line> {
        fixture.lines().map(parse_line).collect()
    }

    /// Every step of `lines`, with each streamed event as `(name, data)`.
    fn run(translator: &mut Translator, lines: &[Line]) -> (Vec<(String, Value)>, Vec<Step>) {
        let mut events = Vec::new();
        let mut ends = Vec::new();
        for line in lines {
            for step in translator.feed(line) {
                match step {
                    Step::Send(bytes) => {
                        let text = String::from_utf8(bytes.to_vec()).unwrap();
                        let (name, data) = text
                            .strip_prefix("event: ")
                            .and_then(|rest| rest.split_once("\ndata: "))
                            .unwrap();
                        events.push((name.to_owned(), serde_json::from_str(data.trim()).unwrap()));
                    }
                    other => ends.push(other),
                }
            }
        }
        (events, ends)
    }

    fn names(events: &[(String, Value)]) -> Vec<&str> {
        events.iter().map(|(name, _)| name.as_str()).collect()
    }

    fn assistant(content: &Value) -> Digest {
        crate::conversation::parse(
            json!({ "model": "m", "messages": [
                { "role": "assistant", "content": content }, { "role": "user", "content": "x" },
            ] })
            .to_string()
            .as_bytes(),
        )
        .unwrap()
        .history[0]
            .digest()
    }

    #[test]
    fn a_call_to_the_harness_ends_the_response_and_the_next_message_is_the_next_response() {
        let lines = lines(include_str!("fixtures/tool.jsonl"));
        let mut translator = translator(&["echo"]);
        translator.respond(true);
        let split = lines
            .iter()
            .position(|line| matches!(line, Line::Event(event) if event["type"] == "message_stop"))
            .unwrap();
        let (events, ends) = run(&mut translator, &lines[..=split]);
        assert_eq!(
            names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(
            events[1].1["content_block"],
            json!({ "type": "tool_use", "id": "toolu_S237cae0f", "name": "echo", "input": {} })
        );
        assert_eq!(
            ends,
            [Step::Hold {
                calls: vec!["toolu_S237cae0f".to_owned()],
                turn: assistant(
                    &json!([{ "type": "tool_use", "id": "toolu_S237cae0f", "name": "echo", "input": { "x": 9 } }])
                ),
                body: None,
            }]
        );
        // The continuation, as a non-streaming response.
        translator.respond(false);
        let (events, ends) = run(&mut translator, &lines[split + 1..]);
        assert_eq!(events, Vec::new());
        let [
            Step::Done {
                turn,
                body: Some(body),
            },
        ] = ends.as_slice()
        else {
            panic!("{ends:?}")
        };
        let text = "got results: [{\"type\": \"text\", \"text\": \"echo {\\\"x\\\": 9}\"}]";
        assert_eq!(*turn, assistant(&json!(text)));
        let message: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(
            message["content"],
            json!([{ "type": "text", "text": text }])
        );
        assert_eq!(message["stop_reason"], "end_turn");
        assert_eq!(message["id"], "msg_a4aa9a1e04b2");
        assert_eq!(message["usage"]["output_tokens"], 7);
        assert!(!translator.diverged());
    }

    #[test]
    fn a_call_to_a_tool_the_harness_never_offered_is_hidden_and_its_messages_merge() {
        let mut translator = translator(&["echo"]);
        translator.respond(true);
        let (events, ends) = run(
            &mut translator,
            &lines(include_str!("fixtures/hallucinated.jsonl")),
        );
        // One message: the hidden Bash call, its stop and the second start are gone.
        assert_eq!(
            names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(events[0].1["message"]["id"], "msg_86f204ce679a");
        assert_eq!(events[1].1["index"], 0);
        assert_eq!(events[1].1["content_block"]["type"], "text");
        assert_eq!(events[4].1["delta"]["stop_reason"], "end_turn");
        // Both messages' output tokens.
        assert_eq!(events[4].1["usage"]["output_tokens"], 14);
        assert!(matches!(ends.as_slice(), [Step::Done { body: None, .. }]));
    }

    #[test]
    fn mixed_calls_keep_the_harnesses_and_reindex_around_the_hidden_one() {
        let mut translator = translator(&["read file"]);
        translator.respond(true);
        let event = |event: Value| Line::Event(event);
        let start = |index: u64, id: &str, name: &str| {
            event(json!({ "type": "content_block_start", "index": index,
                "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } }))
        };
        let (events, ends) = run(
            &mut translator,
            &[
                event(
                    json!({ "type": "message_start", "message": { "id": "m", "usage": { "input_tokens": 3 } } }),
                ),
                event(
                    json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "" } }),
                ),
                event(json!({ "type": "content_block_stop", "index": 0 })),
                start(1, "toolu_x", "Bash"),
                event(
                    json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{}" } }),
                ),
                event(json!({ "type": "content_block_stop", "index": 1 })),
                start(2, "toolu_a", "mcp__client__read_file"),
                event(json!({ "type": "content_block_stop", "index": 2 })),
                start(3, "toolu_b", "mcp__client__read_file"),
                event(json!({ "type": "content_block_stop", "index": 3 })),
                event(
                    json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 5 } }),
                ),
                event(json!({ "type": "message_stop" })),
            ],
        );
        let starts: Vec<(u64, Value)> = events
            .iter()
            .filter(|(name, _)| name == "content_block_start")
            .map(|(_, data)| {
                (
                    data["index"].as_u64().unwrap(),
                    data["content_block"]["name"].clone(),
                )
            })
            .collect();
        assert_eq!(
            starts,
            [
                (0, Value::Null),
                (1, json!("read file")),
                (2, json!("read file"))
            ]
        );
        assert!(!events.iter().any(|(_, data)| data["index"] == 3));
        let [Step::Hold { calls, .. }] = ends.as_slice() else {
            panic!("{ends:?}")
        };
        assert_eq!(calls, &["toolu_a", "toolu_b"]);
    }

    #[test]
    fn errors_before_any_output_are_http_answers_harnesses_act_on() {
        let mut translator = translator(&["echo"]);
        translator.respond(true);
        let (events, ends) = run(
            &mut translator,
            &lines(include_str!("fixtures/rate_limited.jsonl")),
        );
        assert_eq!(events, Vec::new());
        assert_eq!(
            ends,
            [Step::Failed(Some(Failure::new(
                429,
                "rate_limit_error",
                "API Error: Request rejected (429) · This request would exceed your account's rate limit. Please try again later."
            )))]
        );
        // A reset time Claude Code reported becomes the retry time.
        translator.respond(false);
        let mut with_reset = vec![Line::RateLimit {
            resets_at: Some(1_800_000_000),
        }];
        with_reset.extend(lines(include_str!("fixtures/rate_limited.jsonl")));
        let (_, ends) = run(&mut translator, &with_reset);
        let [Step::Failed(Some(failure))] = ends.as_slice() else {
            panic!("{ends:?}")
        };
        assert_eq!(
            (failure.status, failure.retry_at),
            (429, Some(1_800_000_000))
        );
        translator.respond(false);
        let (_, ends) = run(
            &mut translator,
            &lines(include_str!("fixtures/too_long.jsonl")),
        );
        assert_eq!(
            ends,
            [Step::Failed(Some(Failure::new(
                400,
                "invalid_request_error",
                "prompt is too long"
            )))]
        );
    }

    #[test]
    fn each_error_maps_to_its_status() {
        let outcome =
            |status: Option<u16>, kind: Option<&str>, terminal: Option<&str>, text: &str| Outcome {
                error: true,
                subtype: Some("success".to_owned()),
                status,
                kind: kind.map(str::to_owned),
                terminal: terminal.map(str::to_owned),
                text: Some(text.to_owned()),
            };
        let status = |outcome: Outcome| failure(&outcome, None, SIGN_IN);
        assert_eq!(
            status(outcome(Some(529), Some("overloaded_error"), None, "x")).status,
            529
        );
        let signed_out = status(outcome(
            None,
            None,
            None,
            "Not logged in · Please run /login",
        ));
        assert_eq!(
            signed_out,
            Failure::new(
                401,
                "authentication_error",
                format!("Claude Code is not signed in for this profile. Sign in with: {SIGN_IN}")
            )
        );
        assert_eq!(
            status(outcome(Some(401), Some("authentication_error"), None, "x")).status,
            401
        );
        assert_eq!(
            status(outcome(Some(400), None, Some("prompt_too_long"), "x")).status,
            400
        );
        assert_eq!(
            status(outcome(
                Some(500),
                Some("api_error"),
                None,
                "Internal server error"
            )),
            Failure::new(502, "api_error", "Internal server error")
        );
        let silent = Outcome {
            error: true,
            subtype: Some("error_during_execution".to_owned()),
            ..Outcome::default()
        };
        assert_eq!(
            failure(&silent, None, SIGN_IN).message,
            "Claude Code ended the turn: error_during_execution."
        );
    }

    #[test]
    fn an_error_after_output_started_is_an_error_event() {
        let mut translator = translator(&["echo"]);
        translator.respond(true);
        let lines = lines(include_str!("fixtures/tool.jsonl"));
        let first_delta = lines
            .iter()
            .position(
                |line| matches!(line, Line::Event(event) if event["type"] == "content_block_delta"),
            )
            .unwrap();
        let mut cut: Vec<Line> = lines[..=first_delta].to_vec();
        cut.push(Line::Result(Outcome {
            error: true,
            status: Some(529),
            ..Outcome::default()
        }));
        let (events, ends) = run(&mut translator, &cut);
        let (name, data) = events.last().unwrap();
        assert_eq!(name, "error");
        assert_eq!(
            data,
            &json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } })
        );
        assert_eq!(ends, [Step::Failed(None)]);
    }

    #[test]
    fn an_interrupted_turn_fails() {
        let mut translator = translator(&["echo"]);
        translator.respond(true);
        let lines = lines(include_str!("fixtures/interrupted.jsonl"));
        let (_, ends) = run(&mut translator, &lines);
        // The tool call ended the response; the interrupted turn's result fails what follows.
        assert!(matches!(ends.first(), Some(Step::Hold { .. })), "{ends:?}");
        assert!(
            matches!(
                ends.last(),
                Some(Step::Failed(Some(Failure { status: 502, .. })))
            ),
            "{ends:?}"
        );
    }

    #[test]
    fn lines_the_engine_reads_parse_and_the_rest_are_other() {
        assert_eq!(
            parse_line(r#"{"type":"system","subtype":"init","session_id":"s-1","tools":[]}"#),
            Line::Init {
                session: "s-1".to_owned()
            }
        );
        assert_eq!(
            parse_line(
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1800000000000}}"#
            ),
            Line::RateLimit {
                resets_at: Some(1_800_000_000)
            }
        );
        assert_eq!(
            parse_line(
                r#"{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":"toolu_sub"}"#
            ),
            Line::Other
        );
        assert_eq!(
            parse_line(
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"r1","response":{}}}"#
            ),
            Line::Control {
                request_id: "r1".to_owned(),
                response: json!({ "subtype": "success", "request_id": "r1", "response": {} }),
            }
        );
        assert_eq!(parse_line("not json"), Line::Other);
        assert_eq!(parse_line(r#"{"type":"tool_progress"}"#), Line::Other);
    }
}
