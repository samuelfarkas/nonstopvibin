//! The harness's tools as a minimal MCP server (Streamable HTTP, POST only): the JSON-RPC
//! messages Claude Code sends and the answers it gets. Serving and holding calls is the
//! engine's; this module only reads and writes the messages.

use hyper::body::Bytes;
use serde_json::{Value, json};

use crate::conversation::{Part, Tool, ToolResult};

/// JSON-RPC's "method not found".
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_REQUEST: i64 = -32600;

/// A JSON-RPC message from Claude Code.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Message {
    /// A request, answered with its `id`.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A notification, answered with 202.
    Notification { method: String, params: Value },
}

/// Parses one JSON-RPC message; `None` for anything else, such as a batch.
pub(crate) fn parse(body: &[u8]) -> Option<Message> {
    let Value::Object(mut message) = serde_json::from_slice(body).ok()? else {
        return None;
    };
    let method = message.get("method")?.as_str()?.to_owned();
    let params = message.remove("params").unwrap_or(Value::Null);
    Some(match message.remove("id") {
        Some(id) => Message::Request { id, method, params },
        None => Message::Notification { method, params },
    })
}

/// A `tools/call`: the tool use it answers and how to report progress.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Call {
    pub(crate) tool_use: String,
    pub(crate) progress: Option<Value>,
}

/// The tool use a `tools/call` answers, from Claude Code's `_meta`.
pub(crate) fn call(params: &Value) -> Option<Call> {
    let meta = params.get("_meta")?;
    Some(Call {
        tool_use: meta.get("claudecode/toolUseId")?.as_str()?.to_owned(),
        progress: meta.get("progressToken").cloned(),
    })
}

/// The request a `notifications/cancelled` cancels.
pub(crate) fn cancelled(params: &Value) -> Option<&Value> {
    params.get("requestId")
}

pub(crate) fn result(id: &Value, result: &Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

pub(crate) fn not_found(id: &Value, method: &str) -> Value {
    error(id, METHOD_NOT_FOUND, &format!("Method not found: {method}"))
}

pub(crate) fn invalid(id: &Value) -> Value {
    error(
        id,
        INVALID_REQUEST,
        "tools/call needs _meta.claudecode/toolUseId.",
    )
}

/// The answer to `initialize`: the protocol version the client asked for, and tools.
pub(crate) fn initialized(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("2025-06-18");
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "client", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// The answer to `tools/list`: the harness's tools as it offered them.
pub(crate) fn tools(tools: &[Tool]) -> Value {
    let tools: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({ "name": tool.name, "description": tool.description, "inputSchema": tool.schema })
        })
        .collect();
    json!({ "tools": tools })
}

/// The harness's tool result as an MCP `CallToolResult`.
pub(crate) fn outcome(result: &ToolResult) -> Value {
    let content: Vec<Value> = result
        .content
        .iter()
        .map(|part| match part {
            Part::Text(text) => json!({ "type": "text", "text": text }),
            Part::Image { media_type, data } => {
                json!({ "type": "image", "data": data, "mimeType": media_type })
            }
        })
        .collect();
    if result.is_error {
        json!({ "content": content, "isError": true })
    } else {
        json!({ "content": content })
    }
}

/// A progress notification for a held call: Claude Code waits as long as these arrive.
pub(crate) fn progress(token: &Value, seconds: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": { "progressToken": token, "progress": seconds },
    })
}

/// One message as an SSE event of a Streamable HTTP response.
pub(crate) fn event(message: &Value) -> Bytes {
    Bytes::from(format!("event: message\ndata: {message}\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_codes_messages_parse_into_requests_and_notifications() {
        let call = br#"{"method":"tools/call","params":{"name":"echo","arguments":{"x":9},"_meta":{"claudecode/toolUseId":"toolu_S","progressToken":2}},"jsonrpc":"2.0","id":2}"#;
        let Some(Message::Request { id, method, params }) = parse(call) else {
            panic!("not a request")
        };
        assert_eq!((id, method.as_str()), (json!(2), "tools/call"));
        assert_eq!(
            self::call(&params),
            Some(Call {
                tool_use: "toolu_S".to_owned(),
                progress: Some(json!(2)),
            })
        );
        assert_eq!(
            parse(br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2,"reason":"AbortError"}}"#),
            Some(Message::Notification {
                method: "notifications/cancelled".to_owned(),
                params: json!({ "requestId": 2, "reason": "AbortError" }),
            })
        );
        assert_eq!(parse(br"[]"), None);
        assert_eq!(parse(br#"{"jsonrpc":"2.0","id":1}"#), None);
        assert_eq!(self::call(&json!({ "name": "echo" })), None);
    }

    #[test]
    fn tool_results_map_to_call_results() {
        let result = ToolResult {
            id: "toolu_1".to_owned(),
            content: vec![
                Part::Text("done".to_owned()),
                Part::Image {
                    media_type: "image/png".to_owned(),
                    data: "iVBOR".to_owned(),
                },
            ],
            is_error: true,
        };
        assert_eq!(
            outcome(&result),
            json!({
                "content": [
                    { "type": "text", "text": "done" },
                    { "type": "image", "data": "iVBOR", "mimeType": "image/png" },
                ],
                "isError": true,
            })
        );
        let empty = ToolResult {
            content: Vec::new(),
            is_error: false,
            ..result
        };
        assert_eq!(outcome(&empty), json!({ "content": [] }));
    }

    #[test]
    fn the_server_offers_the_harnesses_tools_under_their_own_names() {
        let tools = [Tool {
            name: "apply_patch".to_owned(),
            description: "Applies a patch.".to_owned(),
            schema: json!({ "type": "object", "properties": { "input": { "type": "string" } } }),
        }];
        assert_eq!(
            self::tools(&tools),
            json!({ "tools": [{
                "name": "apply_patch",
                "description": "Applies a patch.",
                "inputSchema": { "type": "object", "properties": { "input": { "type": "string" } } },
            }] })
        );
        assert_eq!(
            initialized(&json!({ "protocolVersion": "2025-11-25" }))["protocolVersion"],
            "2025-11-25"
        );
        assert_eq!(
            not_found(&json!("server-discover-probe-1"), "server/discover")["error"]["code"],
            -32601
        );
        let frame = String::from_utf8(event(&progress(&json!(2), 30)).to_vec()).unwrap();
        let data = frame
            .strip_prefix("event: message\ndata: ")
            .and_then(|rest| rest.strip_suffix("\n\n"))
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(data).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": { "progressToken": 2, "progress": 30 },
            })
        );
    }
}
