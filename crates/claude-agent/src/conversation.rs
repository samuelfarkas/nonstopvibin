//! A Messages request as the engine sees it: its settings, the turns of its history and the
//! user turn it ends with. Turns are compared by a lenient [`Digest`], because the history
//! a harness sends back went through the core's Chat Completions or Responses translation.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

/// Who spoke a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
}

/// A content block the engine keeps: what it digests, replays and hands to Claude Code.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Block {
    Text(String),
    /// A base64 image.
    Image {
        media_type: String,
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult(ToolResult),
}

/// The harness's answer to one tool call.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolResult {
    pub(crate) id: String,
    pub(crate) content: Vec<Part>,
    pub(crate) is_error: bool,
}

/// A piece of a tool result.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Part {
    Text(String),
    Image { media_type: String, data: String },
}

/// One side's consecutive messages, merged.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Turn {
    pub(crate) role: Role,
    pub(crate) blocks: Vec<Block>,
}

/// A turn's lenient digest: its role, its text with all whitespace removed, its tool calls'
/// ids, and its tool results' ids and text. Thinking, signatures, cache markers, tool inputs
/// and how text is split into blocks do not count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Digest([u8; 32]);

fn squeezed(text: &str, into: &mut String) {
    into.extend(text.chars().filter(|c| !c.is_whitespace()));
}

impl Turn {
    pub(crate) fn digest(&self) -> Digest {
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for block in &self.blocks {
            match block {
                Block::Text(value) => squeezed(value, &mut text),
                Block::Image { .. } => {}
                Block::ToolUse { id, .. } => calls.push(id.as_str()),
                Block::ToolResult(result) => {
                    let mut content = String::new();
                    for part in &result.content {
                        if let Part::Text(value) = part {
                            squeezed(value, &mut content);
                        }
                    }
                    results.push((result.id.as_str(), content));
                }
            }
        }
        let mut hash = Sha256::new();
        let mut field = |tag: u8, value: &str| {
            hash.update([tag]);
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value.as_bytes());
        };
        field(
            b'r',
            match self.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            },
        );
        field(b't', &text);
        for id in calls {
            field(b'u', id);
        }
        for (id, content) in &results {
            field(b'i', id);
            field(b'c', content);
        }
        Digest(hash.finalize().into())
    }

    /// The ids of the tool results it carries.
    pub(crate) fn result_ids(&self) -> Vec<&str> {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                Block::ToolResult(result) => Some(result.id.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Whether it says anything besides tool results.
    pub(crate) fn has_message(&self) -> bool {
        self.blocks.iter().any(|block| match block {
            Block::Text(text) => !text.trim().is_empty(),
            Block::Image { .. } => true,
            Block::ToolUse { .. } | Block::ToolResult(_) => false,
        })
    }
}

/// An effort level Claude Code takes with `--effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::Xhigh,
            "max" => Self::Max,
            _ => return None,
        })
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// A tool the harness offers, which Claude Code sees as `mcp__client__<name>`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tool {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) schema: Value,
}

/// What a `claude` process is started with. A live session continues a conversation only
/// with the same settings; any change resumes it in a new process.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) model: String,
    pub(crate) effort: Option<Effort>,
    pub(crate) max_tokens: Option<u64>,
    pub(crate) system: String,
    pub(crate) tools: Arc<[Tool]>,
}

/// [`Settings`] reduced to what tells two of them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    model: String,
    effort: Option<Effort>,
    max_tokens: Option<u64>,
    system: [u8; 32],
    tools: [u8; 32],
}

impl Settings {
    pub(crate) fn fingerprint(&self) -> Fingerprint {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|tool| json!([tool.name, tool.description, tool.schema]))
            .collect();
        Fingerprint {
            model: self.model.clone(),
            effort: self.effort,
            max_tokens: self.max_tokens,
            system: Sha256::digest(self.system.as_bytes()).into(),
            tools: Sha256::digest(Value::Array(tools).to_string().as_bytes()).into(),
        }
    }
}

/// A Messages request: its settings, the turns before its last, and the user turn it ends with.
#[derive(Debug, Clone)]
pub(crate) struct Conversation {
    pub(crate) settings: Settings,
    pub(crate) stream: bool,
    pub(crate) history: Vec<Turn>,
    pub(crate) tail: Turn,
}

impl Conversation {
    pub(crate) fn history_digests(&self) -> Vec<Digest> {
        self.history.iter().map(Turn::digest).collect()
    }
}

/// A request the engine cannot serve, with the message the harness gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Invalid(pub(crate) String);

fn invalid(message: impl Into<String>) -> Invalid {
    Invalid(message.into())
}

/// A model name as `--model` takes it: no option-like or unusual characters.
fn model_name(value: &str) -> bool {
    (1..=200).contains(&value.len())
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_:[]".contains(&b))
}

fn text_of(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

fn image(source: Option<&Value>) -> Option<(String, String)> {
    let source = source?;
    (text_of(source.get("type")) == Some("base64")).then_some(())?;
    Some((
        text_of(source.get("media_type"))?.to_owned(),
        text_of(source.get("data"))?.to_owned(),
    ))
}

fn parts(content: Option<&Value>) -> Vec<Part> {
    match content {
        Some(Value::String(text)) => vec![Part::Text(text.clone())],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match text_of(item.get("type"))? {
                "text" => Some(Part::Text(text_of(item.get("text"))?.to_owned())),
                "image" => {
                    let (media_type, data) = image(item.get("source"))?;
                    Some(Part::Image { media_type, data })
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The blocks of one message's content that the engine keeps.
fn blocks(content: Option<&Value>) -> Vec<Block> {
    let items = match content {
        Some(Value::String(text)) => return vec![Block::Text(text.clone())],
        Some(Value::Array(items)) => items,
        _ => return Vec::new(),
    };
    items
        .iter()
        .filter_map(|item| match text_of(item.get("type"))? {
            "text" => Some(Block::Text(text_of(item.get("text"))?.to_owned())),
            "image" => {
                let (media_type, data) = image(item.get("source"))?;
                Some(Block::Image { media_type, data })
            }
            "tool_use" => Some(Block::ToolUse {
                id: text_of(item.get("id"))?.to_owned(),
                name: text_of(item.get("name")).unwrap_or_default().to_owned(),
                input: item.get("input").cloned().unwrap_or(Value::Null),
            }),
            "tool_result" => Some(Block::ToolResult(ToolResult {
                id: text_of(item.get("tool_use_id"))?.to_owned(),
                content: parts(item.get("content")),
                is_error: item.get("is_error").and_then(Value::as_bool) == Some(true),
            })),
            // Thinking, redacted thinking and server-side blocks are Claude Code's own.
            _ => None,
        })
        .collect()
}

/// `messages` as turns: system entries skipped, empty messages dropped, consecutive messages
/// of one side merged.
fn turns(messages: &[Value]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for message in messages {
        let role = match text_of(message.get("role")) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let mut content = blocks(message.get("content"));
        if content.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some(last) if last.role == role => last.blocks.append(&mut content),
            _ => turns.push(Turn {
                role,
                blocks: content,
            }),
        }
    }
    turns
}

fn system(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| text_of(item.get("text")))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn tools(value: Option<&Value>) -> Vec<Tool> {
    let mut seen = HashSet::new();
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            // Tools without a schema are the provider's own, which Claude Code cannot offer.
            let schema = tool
                .get("input_schema")
                .filter(|schema| schema.is_object())?;
            let name = text_of(tool.get("name")).filter(|name| !name.is_empty())?;
            seen.insert(name.to_owned()).then(|| Tool {
                name: name.to_owned(),
                description: text_of(tool.get("description"))
                    .unwrap_or_default()
                    .to_owned(),
                schema: schema.clone(),
            })
        })
        .collect()
}

/// Parses a Messages request body.
pub(crate) fn parse(body: &[u8]) -> Result<Conversation, Invalid> {
    let request: Map<String, Value> = serde_json::from_slice(body)
        .map_err(|_| invalid("The request body is not a JSON object."))?;
    let model = text_of(request.get("model"))
        .filter(|model| model_name(model))
        .ok_or_else(|| invalid("model: a model name is required."))?
        .to_owned();
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("messages: an array is required."))?;
    let mut history = turns(messages);
    let tail = match history.pop() {
        Some(tail) if tail.role == Role::User => tail,
        _ => return Err(invalid("messages: the last message must be the user's.")),
    };
    let effort = request
        .get("output_config")
        .and_then(|config| text_of(config.get("effort")))
        .and_then(Effort::parse);
    Ok(Conversation {
        settings: Settings {
            model,
            effort,
            max_tokens: request
                .get("max_tokens")
                .and_then(Value::as_u64)
                .filter(|tokens| *tokens > 0),
            system: system(request.get("system")),
            tools: tools(request.get("tools")).into(),
        },
        stream: request.get("stream").and_then(Value::as_bool) == Some(true),
        history,
        tail,
    })
}

// ---- What Claude Code receives ----

fn part_text(content: &[Part]) -> String {
    content
        .iter()
        .map(|part| match part {
            Part::Text(text) => text.as_str(),
            Part::Image { .. } => "[image]",
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The history Claude Code has not seen, as one text block: the start of a fresh session.
pub(crate) fn checkpoint(history: &[Turn]) -> Option<String> {
    if history.is_empty() {
        return None;
    }
    let mut out = String::from(
        "<conversation-checkpoint>\nThis conversation continues from an earlier session. Its history so far:\n",
    );
    for turn in history {
        let role = match turn.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let _ = write!(out, "\n<{role}>\n");
        for block in &turn.blocks {
            match block {
                Block::Text(text) => {
                    out.push_str(text);
                    out.push('\n');
                }
                Block::Image { .. } => out.push_str("[image]\n"),
                Block::ToolUse { id, name, input } => {
                    let _ = writeln!(
                        out,
                        "<tool_use id=\"{id}\" name=\"{name}\">{input}</tool_use>"
                    );
                }
                Block::ToolResult(result) => {
                    let error = if result.is_error {
                        " is_error=\"true\""
                    } else {
                        ""
                    };
                    let _ = writeln!(
                        out,
                        "<tool_result id=\"{}\"{error}>{}</tool_result>",
                        result.id,
                        part_text(&result.content)
                    );
                }
            }
        }
        let _ = writeln!(out, "</{role}>");
    }
    out.push_str("</conversation-checkpoint>");
    Some(out)
}

/// The content of a user message for Claude Code: an optional checkpoint, then the turn's
/// text and images. Tool results become text, since this session never made those calls.
pub(crate) fn user_content(checkpoint: Option<&str>, turn: &Turn) -> Vec<Value> {
    let mut content: Vec<Value> = checkpoint
        .map(|text| json!({ "type": "text", "text": text }))
        .into_iter()
        .collect();
    for block in &turn.blocks {
        match block {
            Block::Text(text) => content.push(json!({ "type": "text", "text": text })),
            Block::Image { media_type, data } => content.push(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data },
            })),
            Block::ToolUse { .. } => {}
            Block::ToolResult(result) => content.push(json!({
                "type": "text",
                "text": format!(
                    "<tool_result id=\"{}\">{}</tool_result>",
                    result.id,
                    part_text(&result.content)
                ),
            })),
        }
    }
    content
}

/// The tool results of a turn.
pub(crate) fn results(turn: &Turn) -> Vec<ToolResult> {
    turn.blocks
        .iter()
        .filter_map(|block| match block {
            Block::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(body: &Value) -> Conversation {
        parse(body.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn a_request_splits_into_settings_history_and_the_user_turn_it_ends_with() {
        let parsed = conversation(&json!({
            "model": "claude-opus-5-5",
            "max_tokens": 32000,
            "stream": true,
            "output_config": { "effort": "high" },
            "system": [{ "type": "text", "text": "Be brief." }, { "type": "text", "text": "Use tools." }],
            "tools": [
                { "name": "read", "description": "Reads.", "input_schema": { "type": "object" } },
                { "type": "web_search_20250305", "name": "web_search" },
                { "name": "read", "input_schema": { "type": "object" } },
            ],
            "messages": [
                { "role": "system", "content": "ignored" },
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "hm", "signature": "s" },
                    { "type": "text", "text": "Reading." },
                    { "type": "tool_use", "id": "toolu_1", "name": "read", "input": { "path": "a" } },
                ] },
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "toolu_1", "content": "A" }] },
                { "role": "user", "content": [{ "type": "text", "text": "and then?", "cache_control": { "type": "ephemeral" } }] },
            ],
        }));
        assert_eq!(parsed.settings.model, "claude-opus-5-5");
        assert_eq!(parsed.settings.effort, Some(Effort::High));
        assert_eq!(parsed.settings.max_tokens, Some(32000));
        assert_eq!(parsed.settings.system, "Be brief.\n\nUse tools.");
        assert_eq!(parsed.settings.tools.len(), 1);
        assert_eq!(parsed.settings.tools[0].name, "read");
        assert!(parsed.stream);
        assert_eq!(parsed.history.len(), 2);
        assert_eq!(
            parsed.history[1].blocks,
            vec![
                Block::Text("Reading.".to_owned()),
                Block::ToolUse {
                    id: "toolu_1".to_owned(),
                    name: "read".to_owned(),
                    input: json!({ "path": "a" }),
                },
            ]
        );
        // Both trailing user messages are one turn.
        assert_eq!(parsed.tail.result_ids(), vec!["toolu_1"]);
        assert!(parsed.tail.has_message());
    }

    #[test]
    fn requests_the_engine_cannot_serve_are_refused() {
        for body in [
            json!({ "messages": [{ "role": "user", "content": "hi" }] }),
            json!({ "model": "--dangerous", "messages": [{ "role": "user", "content": "hi" }] }),
            json!({ "model": "m", "messages": [] }),
            json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }, { "role": "assistant", "content": "prefill" }] }),
        ] {
            assert!(parse(body.to_string().as_bytes()).is_err(), "{body}");
        }
        assert!(parse(b"[]").is_err());
    }

    #[test]
    fn digests_ignore_what_translations_change_and_keep_what_they_keep() {
        let digest = |message: Value| turns(&[message])[0].digest();
        // Anthropic blocks as the engine streamed them.
        let streamed = digest(json!({ "role": "assistant", "content": [
            { "type": "thinking", "thinking": "plan", "signature": "sig" },
            { "type": "text", "text": "Reading the file.\n" },
            { "type": "text", "text": " Then the other." },
            { "type": "tool_use", "id": "toolu_1", "name": "read", "input": { "path": "a" } },
        ] }));
        // The same turn back through a Chat Completions translation: one text, no thinking,
        // reserialized arguments.
        let echoed = digest(json!({ "role": "assistant", "content": [
            { "type": "text", "text": "Reading the file. Then the other.", "cache_control": { "type": "ephemeral" } },
            { "type": "tool_use", "id": "toolu_1", "name": "read", "input": { "path": "a", "extra": 1 } },
        ] }));
        assert_eq!(streamed, echoed);
        let other_call = digest(json!({ "role": "assistant", "content": [
            { "type": "text", "text": "Reading the file. Then the other." },
            { "type": "tool_use", "id": "toolu_2", "name": "read", "input": {} },
        ] }));
        assert_ne!(streamed, other_call);
        let other_text = digest(json!({ "role": "assistant", "content": "Reading another file." }));
        assert_ne!(streamed, other_text);
        assert_ne!(
            digest(json!({ "role": "user", "content": "same" })),
            digest(json!({ "role": "assistant", "content": "same" }))
        );
        assert_eq!(
            digest(
                json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t", "content": "out put" }] })
            ),
            digest(
                json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t", "content": [{ "type": "text", "text": "output" }] }] })
            )
        );
        assert_ne!(
            digest(
                json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t", "content": "one" }] })
            ),
            digest(
                json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t", "content": "two" }] })
            )
        );
    }

    #[test]
    fn settings_differ_by_model_effort_tokens_system_and_tools() {
        let tool = json!([{ "name": "t", "input_schema": { "type": "object" } }]);
        let base = conversation(&json!({
            "model": "m", "max_tokens": 10, "system": "s", "tools": tool,
            "messages": [{ "role": "user", "content": "hi" }],
        }));
        let same = conversation(&json!({
            "model": "m", "max_tokens": 10, "system": [{ "type": "text", "text": "s", "cache_control": {} }],
            "tools": [{ "name": "t", "input_schema": { "type": "object" }, "cache_control": {} }],
            "messages": [{ "role": "user", "content": "other" }],
        }));
        assert_eq!(base.settings.fingerprint(), same.settings.fingerprint());
        for changed in [
            json!({ "model": "n", "max_tokens": 10, "system": "s", "tools": tool }),
            json!({ "model": "m", "max_tokens": 11, "system": "s", "tools": tool }),
            json!({ "model": "m", "max_tokens": 10, "system": "S", "tools": tool }),
            json!({ "model": "m", "max_tokens": 10, "system": "s", "tools": [{ "name": "u", "input_schema": { "type": "object" } }] }),
            json!({ "model": "m", "max_tokens": 10, "system": "s", "output_config": { "effort": "low" }, "tools": tool }),
        ] {
            let mut body = changed.clone();
            body["messages"] = json!([{ "role": "user", "content": "hi" }]);
            assert_ne!(
                base.settings.fingerprint(),
                conversation(&body).settings.fingerprint(),
                "{changed}"
            );
        }
    }

    #[test]
    fn a_checkpoint_replays_the_history_and_tool_results_become_text() {
        let parsed = conversation(&json!({
            "model": "m",
            "messages": [
                { "role": "user", "content": "Fix the bug." },
                { "role": "assistant", "content": [
                    { "type": "text", "text": "Looking." },
                    { "type": "tool_use", "id": "toolu_1", "name": "read", "input": { "path": "a.rs" } },
                ] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1", "content": "fn main() {}", "is_error": true },
                    { "type": "text", "text": "Hurry." },
                ] },
            ],
        }));
        assert_eq!(
            checkpoint(&parsed.history).unwrap(),
            "<conversation-checkpoint>\nThis conversation continues from an earlier session. Its history so far:\n\
             \n<user>\nFix the bug.\n</user>\n\
             \n<assistant>\nLooking.\n<tool_use id=\"toolu_1\" name=\"read\">{\"path\":\"a.rs\"}</tool_use>\n</assistant>\n\
             </conversation-checkpoint>"
        );
        assert_eq!(
            user_content(Some("CP"), &parsed.tail),
            vec![
                json!({ "type": "text", "text": "CP" }),
                json!({ "type": "text", "text": "<tool_result id=\"toolu_1\">fn main() {}</tool_result>" }),
                json!({ "type": "text", "text": "Hurry." }),
            ]
        );
        assert_eq!(checkpoint(&[]), None);
    }
}
