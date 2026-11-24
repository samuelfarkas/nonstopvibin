//! Replays `tests/golden/gateway.json`, recorded through the TypeScript `Gateway.proxy`, against
//! this gateway with the same stub core, profiles and raw requests.
//!
//! Both sides are reduced to the recorder's shape and then normalised the same way: header
//! names lowercase, a repeated header joined with `", "` (Node joined them, hyper sends each
//! line), lists sorted by name, and the response framing headers (`content-length`,
//! `transfer-encoding`) left out because the echoed body's length follows the forwarded
//! header order. The catalog bodies come from the goldens themselves, since the host builds
//! them; the replay checks which agent, profile and labels the gateway picks. A case this
//! gateway answers differently on purpose must give the answer [`intentional`] states.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use nsv_domain::registry::Agent;
use nsv_domain::secret::KeyDigest;
use nsv_gateway::{Catalog, Refusal};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::common::{Header, RawRequest, TestHost, Wake, key, profile, send, spawn_gateway};

const GOLDEN: &str = include_str!("../../../../tests/golden/gateway.json");

/// What this gateway answers on purpose where it differs from the recording.
enum Intended {
    /// Bun's parser answered 400 to raw UTF-8 or `*` in the target. hyper accepts both and the
    /// gateway refuses the path with this status and message; the core receives nothing.
    Refused(u16, &'static str),
    /// Node added `Content-Length: 0` to an empty POST. hyper sends no length header, which
    /// RFC 9112 reads as no body, as Go does. Everything else is as recorded.
    NoContentLength,
}

const UNSUPPORTED: Intended = Intended::Refused(404, "Unsupported proxy endpoint.");
const FOREIGN: Intended = Intended::Refused(
    403,
    "This API key does not belong to the requested profile.",
);

/// The intended answer for a case this gateway answers differently, if it does.
fn intentional(name: &str) -> Option<Intended> {
    match name {
        "target *" => return Some(UNSUPPORTED),
        "catalog POST with header" => return Some(Intended::NoContentLength),
        _ => {}
    }
    let target = name
        .strip_prefix("route GET ")
        .or_else(|| name.strip_prefix("route POST "))?;
    // Each presents the work key: no endpoint under no prefix or its own, a foreign profile
    // under any other.
    match target.strip_suffix("/v1/responses/ünïcode")? {
        "" | "/p/work" | "/p/work/p/work" => Some(UNSUPPORTED),
        "/p/personal" | "/p/WORK" | "/p/work-" | "/p/team-2" | "/p/" | "/p" | "/p/w%6Frk" => {
            Some(FOREIGN)
        }
        _ => None,
    }
}

impl Intended {
    /// The comparable answer, from the recorded one.
    fn answer(&self, recorded: &Value) -> Value {
        match self {
            Self::Refused(status, message) => {
                json!({ "error": { "message": message, "status": status } })
            }
            Self::NoContentLength => {
                let mut answer = recorded.clone();
                if let Some(forwarded) = answer["output"]["forwardedHeaders"].as_array_mut() {
                    forwarded.retain(|pair| pair[0] != "content-length");
                }
                answer
            }
        }
    }
}

/// Response sets the stub core answers with, chosen by `x-golden-response`.
fn response_set(index: usize) -> (u16, Vec<(&'static str, &'static str)>) {
    let json = ("Content-Type", "application/json");
    match index {
        1 => (
            200,
            vec![
                json,
                ("Access-Control-Allow-Origin", "*"),
                ("Set-Cookie", "a=1; Path=/"),
                ("Set-Cookie", "b=2"),
                ("X-Request-Id", "req-1"),
            ],
        ),
        2 => (200, vec![json, ("X-Nonstopvibin-Profile", "spoofed")]),
        3 => (
            200,
            vec![
                ("content-type", "text/event-stream"),
                ("cache-control", "no-cache"),
                ("access-control-allow-credentials", "true"),
                ("access-control-expose-headers", "x-request-id"),
            ],
        ),
        4 => (
            429,
            vec![
                json,
                ("Retry-After", "7"),
                ("anthropic-ratelimit-requests-remaining", "0"),
            ],
        ),
        5 => (
            500,
            vec![
                ("Content-Type", "text/plain"),
                ("WWW-Authenticate", "Bearer realm=\"core\""),
            ],
        ),
        6 => (200, vec![json, ("X-Multi", "1"), ("X-Multi", "2")]),
        7 => (404, vec![json]),
        _ => (200, vec![json]),
    }
}

/// The raw stub core of the recorder: answers with the request line, headers and body it got.
/// It counts the connections it accepts.
async fn stub_core() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&accepted);
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            counted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 4096];
                let answer = loop {
                    let read = socket.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(answer) = echo(&buffer) {
                        break answer;
                    }
                };
                let _written = socket.write_all(&answer).await;
            });
        }
    });
    (address, accepted)
}

/// The stub's answer once `buffer` holds a whole request.
fn echo(buffer: &[u8]) -> Option<Vec<u8>> {
    let end = buffer.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buffer[..end]);
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .map(|line| {
            let (key, value) = line.split_once(':').unwrap_or((line, ""));
            (key.to_owned(), value.trim().to_owned())
        })
        .collect();
    let get = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    let body = String::from_utf8_lossy(&buffer[end + 4..]).into_owned();
    if get("transfer-encoding") == Some("chunked") {
        if !body.ends_with("0\r\n\r\n") {
            return None;
        }
    } else if body.len() < get("content-length").map_or(0, |n| n.parse().unwrap()) {
        return None;
    }
    let set = get("x-golden-response").map_or(0, |n| n.parse().unwrap());
    let (status, response_headers) = response_set(set);
    let payload =
        json!({ "requestLine": request_line, "headers": headers, "body": body }).to_string();
    let mut out = format!("HTTP/1.1 {status} Stub\r\n");
    for (key, value) in response_headers {
        write!(out, "{key}: {value}\r\n").unwrap();
    }
    write!(
        out,
        "Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )
    .unwrap();
    Some(out.into_bytes())
}

fn request(input: &Value) -> RawRequest {
    RawRequest {
        method: input["method"].as_str().unwrap().to_owned(),
        target: input["target"].as_str().unwrap().to_owned(),
        headers: headers(&input["headers"]),
        body: input["body"].as_str().unwrap_or_default().to_owned(),
    }
}

fn headers(value: &Value) -> Vec<Header> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            (
                pair[0].as_str().unwrap().to_owned(),
                pair[1].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// The recorder's profiles: the rotated profile's index still maps its old key, whose live
/// row now holds the new one.
fn golden_host(core: SocketAddr, cases: &[Value]) -> TestHost {
    let ids = [
        ("work", 'a', "11111111-1111-4111-8111-111111111111"),
        ("personal", 'b', "22222222-2222-4222-8222-222222222222"),
        ("team-2", 'c', "33333333-3333-4333-8333-333333333333"),
    ];
    let mut index: HashMap<_, _> = ids
        .iter()
        .map(|&(slug, letter, id)| (KeyDigest::of(&key(letter)), profile(id, slug, &key(letter))))
        .collect();
    let rotated = profile("44444444-4444-4444-8444-444444444444", "rotated", &key('e'));
    index.insert(KeyDigest::of(&key('d')), Arc::clone(&rotated));
    index.insert(KeyDigest::of(&key('e')), rotated);
    let recorded = |name: &str| {
        let case = cases.iter().find(|case| case["name"] == name).unwrap();
        serde_json::to_vec(&case["output"]["body"]).unwrap()
    };
    let personal = cases
        .iter()
        .find(|case| case["name"] == "catalog query personal profile without priced models")
        .unwrap();
    let work = ids[0].2.to_owned();
    let catalogs = HashMap::from([
        (
            (work.clone(), Agent::Codex),
            Ok(Catalog::List(recorded("catalog header \"codex\"").into())),
        ),
        (
            (work.clone(), Agent::Pi),
            Ok(Catalog::Entries(recorded("catalog query \"pi\"").into())),
        ),
        (
            (work, Agent::Opencode),
            Ok(Catalog::Entries(
                recorded("catalog query \"opencode\"").into(),
            )),
        ),
        (
            (ids[1].2.to_owned(), Agent::Pi),
            Err(Refusal::new(
                hyper::StatusCode::BAD_REQUEST,
                personal["error"]["message"].as_str().unwrap().to_owned(),
            )),
        ),
    ]);
    TestHost {
        index,
        wake: Wake::At(core),
        catalogs,
        activity: Arc::default(),
    }
}

/// Lowercase names, repeated headers joined, sorted by name, without `skip`.
fn normalise(headers: &[Header], skip: &[&str]) -> Value {
    let mut joined: Vec<(String, String)> = Vec::new();
    for (key, value) in headers {
        let key = key.to_ascii_lowercase();
        if skip.contains(&key.as_str()) {
            continue;
        }
        match joined.iter_mut().find(|(name, _)| *name == key) {
            Some((_, existing)) => {
                existing.push_str(", ");
                existing.push_str(value);
            }
            None => joined.push((key, value.clone())),
        }
    }
    joined.sort_by(|a, b| a.0.cmp(&b.0));
    json!(joined)
}

const FRAMING: [&str; 3] = ["content-length", "transfer-encoding", "date"];

/// A recorded or observed outcome in comparable form.
fn comparable(case: &Value) -> Value {
    if let Some(error) = case.get("error") {
        return json!({ "error": error });
    }
    let mut output = case["output"].clone();
    for (field, skip) in [
        ("forwardedHeaders", &[][..]),
        ("responseHeaders", &FRAMING[..]),
        ("headers", &FRAMING[..]),
    ] {
        if let Some(list) = output.get(field) {
            output[field] = normalise(&headers(list), skip);
        }
    }
    json!({ "output": output })
}

fn uuid_placeholder(value: &str) -> String {
    if nsv_domain::id::is_uuid(value) && value.as_bytes()[14] == b'4' {
        "<uuid>".to_owned()
    } else {
        value.to_owned()
    }
}

/// What the recorder would have written for this response.
fn observe(name: &str, response: &crate::common::RawResponse, core_port: u16) -> Value {
    let body = String::from_utf8_lossy(&response.body).into_owned();
    if let Ok(error) = serde_json::from_str::<Value>(&body)
        && error["error"]["type"] == "nonstopvibin_error"
    {
        return json!({ "error": {
            "message": error["error"]["message"], "status": error["error"]["code"],
        }});
    }
    if let Ok(received) = serde_json::from_str::<Value>(&body)
        && received.get("requestLine").is_some()
    {
        let mut output = json!({
            "kind": "proxied",
            "profile": response.header("x-nonstopvibin-profile"),
            "requestLine": received["requestLine"],
        });
        let detailed = !["route ", "query ", "credentials "]
            .iter()
            .any(|prefix| name.starts_with(prefix));
        if detailed {
            let forwarded: Vec<Header> = headers(&received["headers"])
                .into_iter()
                .map(|(key, value)| {
                    let value = if key.eq_ignore_ascii_case("x-opencode-session") {
                        uuid_placeholder(&value)
                    } else {
                        value.replace(&format!("127.0.0.1:{core_port}"), "127.0.0.1:<core-port>")
                    };
                    (key, value)
                })
                .collect();
            output["forwardedHeaders"] = json!(forwarded);
            output["forwardedBody"] = received["body"].clone();
            output["status"] = json!(response.status);
            output["responseHeaders"] = json!(response.headers);
        }
        return json!({ "output": output });
    }
    let body = if response.status == 200 {
        serde_json::from_str(&body).unwrap()
    } else {
        json!(body)
    };
    json!({ "output": {
        "kind": if response.status == 200 { "catalog" } else { "http" },
        "status": response.status,
        "headers": response.headers,
        "body": body,
    }})
}

#[tokio::test]
async fn replays_the_typescript_goldens() {
    let cases: Vec<Value> = serde_json::from_str(GOLDEN).unwrap();
    let (core, accepted) = stub_core().await;
    let (address, _gateway) = spawn_gateway(golden_host(core, &cases)).await;
    let mut passed = 0;
    let mut differing = Vec::new();
    let mut failures = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let connections = accepted.load(Ordering::SeqCst);
        let response = send(address, &request(&case["input"])).await;
        let reached_core = accepted.load(Ordering::SeqCst) != connections;
        let observed = json!({ "name": name, "input": case["input"] })
            .as_object()
            .unwrap()
            .clone()
            .into_iter()
            .chain(
                observe(name, &response, core.port())
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .collect::<serde_json::Map<_, _>>();
        let (recorded, observed) = (comparable(case), comparable(&Value::Object(observed)));
        let Some(intended) = intentional(name) else {
            if recorded == observed {
                passed += 1;
            } else {
                failures.push(format!(
                    "{name}\n  expected {recorded}\n  observed {observed}"
                ));
            }
            continue;
        };
        let expected = intended.answer(&recorded);
        if expected == recorded {
            failures.push(format!(
                "{name}: listed as different but intends the recording"
            ));
        } else if expected != observed {
            failures.push(format!(
                "{name}\n  intended {expected}\n  observed {observed}"
            ));
        } else if matches!(intended, Intended::Refused(..)) && reached_core {
            failures.push(format!(
                "{name}: refused, but the core received a connection"
            ));
        } else {
            differing.push(name);
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
    assert_eq!(passed + differing.len(), cases.len());
    assert_eq!((passed, differing.len()), (1268, 22));
}
