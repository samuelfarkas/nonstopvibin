//! Pass-through behaviour the goldens do not record: hang-ups, the in-flight cap, an
//! unreachable core and the WebSocket relay.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use nsv_domain::id::ProfileId;
use nsv_gateway::Activity;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};

use crate::common::{
    RawRequest, TestHost, WORK, Wake, encode, key, parse_head, send, spawn_gateway, within,
};

const LIMIT: Duration = Duration::from_millis(100);

fn work() -> ProfileId {
    ProfileId::parse(WORK).unwrap()
}

async fn read_until(socket: &mut TcpStream, buffer: &mut Vec<u8>, needle: &[u8]) {
    let mut chunk = [0_u8; 4096];
    while !buffer.windows(needle.len()).any(|window| window == needle) {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "EOF before {:?}", String::from_utf8_lossy(needle));
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// A core that answers every request with an SSE stream that sends one event and then
/// nothing, and counts the connections it still has open.
async fn silent_stream() -> (SocketAddr, Arc<AtomicUsize>) {
    silent_core(true).await
}

/// A core that keeps every connection open until the gateway closes it, answering with one
/// SSE event when `answer`, or never answering.
async fn silent_core(answer: bool) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let open = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&open);
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            counted.fetch_add(1, Ordering::SeqCst);
            let counted = Arc::clone(&counted);
            tokio::spawn(async move {
                let mut head = Vec::new();
                read_until(&mut socket, &mut head, b"\r\n\r\n").await;
                if answer {
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                          transfer-encoding: chunked\r\n\r\n\
                          d\r\ndata: hello\n\n\r\n",
                        )
                        .await
                        .unwrap();
                }
                let mut chunk = [0_u8; 4096];
                while socket.read(&mut chunk).await.is_ok_and(|read| read > 0) {}
                counted.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    (address, open)
}

fn stream_request(port: u16, keep_alive: bool) -> Vec<u8> {
    let request = RawRequest {
        method: "POST".to_owned(),
        target: "/v1/responses".to_owned(),
        headers: vec![
            ("Authorization".to_owned(), format!("Bearer {}", key('a'))),
            ("Accept".to_owned(), "text/event-stream".to_owned()),
        ],
        body: "{}".to_owned(),
    };
    let raw = encode(port, &request);
    if keep_alive {
        // Agents keep connections alive; `encode` asks for close as the golden client did.
        String::from_utf8(raw)
            .unwrap()
            .replace("Connection: close\r\n", "")
            .into_bytes()
    } else {
        raw
    }
}

/// An agent connection whose stream has delivered its first event.
async fn open_stream(gateway: SocketAddr, keep_alive: bool) -> TcpStream {
    let mut agent = TcpStream::connect(gateway).await.unwrap();
    agent
        .write_all(&stream_request(gateway.port(), keep_alive))
        .await
        .unwrap();
    read_until(&mut agent, &mut Vec::new(), b"data: hello").await;
    agent
}

#[tokio::test]
async fn a_hang_up_during_a_silent_stream_closes_the_upstream() {
    for keep_alive in [true, false] {
        let (core, open) = silent_stream().await;
        let (address, gateway) = spawn_gateway(TestHost::work(Wake::At(core))).await;
        let agent = open_stream(address, keep_alive).await;
        assert_eq!(
            (gateway.in_flight(&work()), open.load(Ordering::SeqCst)),
            (1, 1)
        );
        drop(agent);
        within(LIMIT, "the upstream close", || {
            open.load(Ordering::SeqCst) == 0
        })
        .await;
        within(LIMIT, "the release", || gateway.in_flight(&work()) == 0).await;
    }
}

#[tokio::test]
async fn a_hang_up_before_the_core_answers_closes_the_upstream() {
    let (core, open) = silent_core(false).await;
    let (address, gateway) = spawn_gateway(TestHost::work(Wake::At(core))).await;
    let mut agent = TcpStream::connect(address).await.unwrap();
    agent
        .write_all(&stream_request(address.port(), true))
        .await
        .unwrap();
    within(LIMIT, "the upstream request", || {
        open.load(Ordering::SeqCst) == 1
    })
    .await;
    drop(agent);
    within(LIMIT, "the upstream close", || {
        open.load(Ordering::SeqCst) == 0
    })
    .await;
    within(LIMIT, "the release", || gateway.in_flight(&work()) == 0).await;
}

#[tokio::test]
async fn a_hang_up_while_the_core_wakes_releases_the_count() {
    let host = TestHost::work(Wake::Never);
    let (address, gateway) = spawn_gateway(host).await;
    let mut agent = TcpStream::connect(address).await.unwrap();
    agent
        .write_all(&stream_request(address.port(), true))
        .await
        .unwrap();
    within(LIMIT, "the count", || gateway.in_flight(&work()) == 1).await;
    drop(agent);
    within(LIMIT, "the release", || gateway.in_flight(&work()) == 0).await;
}

#[tokio::test]
async fn a_profile_holds_at_most_32_requests() {
    let (core, open) = silent_stream().await;
    let host = TestHost::work(Wake::At(core));
    let activity = Arc::clone(&host.activity);
    let (address, gateway) = spawn_gateway(host).await;
    let mut agents = Vec::new();
    for _ in 0..32 {
        agents.push(open_stream(address, true).await);
    }
    assert_eq!(gateway.in_flight(&work()), 32);
    let request = RawRequest {
        method: "GET".to_owned(),
        target: "/v1/models".to_owned(),
        headers: vec![("Authorization".to_owned(), format!("Bearer {}", key('a')))],
        body: String::new(),
    };
    let refused = send(address, &request).await;
    assert_eq!(refused.status, 429);
    // `Retry-After` ahead of the error JSON's headers, as `server.ts` set it.
    let described: Vec<(&str, &str)> = refused
        .headers
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .filter(|(key, _)| !matches!(*key, "connection" | "content-length" | "date"))
        .collect();
    assert_eq!(
        described,
        [
            ("retry-after", "2"),
            ("content-type", "application/json"),
            ("cache-control", "no-store"),
            ("x-content-type-options", "nosniff"),
        ]
    );
    assert_eq!(
        String::from_utf8_lossy(&refused.body),
        r#"{"error":{"message":"This profile has 32 requests in flight. Retry shortly.","type":"nonstopvibin_error","code":429}}"#
    );
    drop(agents.pop());
    within(LIMIT, "one release", || gateway.in_flight(&work()) == 31).await;
    agents.push(open_stream(address, true).await);
    drop(agents);
    within(LIMIT, "every release", || gateway.in_flight(&work()) == 0).await;
    within(LIMIT, "every upstream close", || {
        open.load(Ordering::SeqCst) == 0
    })
    .await;
    let activity: Vec<Activity> = activity.lock().unwrap().iter().map(|(_, a)| *a).collect();
    let opened = activity
        .iter()
        .filter(|a| matches!(a, Activity::Opened { .. }))
        .count();
    let closed = activity
        .iter()
        .filter(|a| matches!(a, Activity::Closed { .. }))
        .count();
    // 33 streams and the refusal, which is never counted in.
    assert_eq!((opened, closed), (33, 33));
    let edges: Vec<_> = activity
        .iter()
        .filter(|a| {
            matches!(
                a,
                Activity::Opened { first: true } | Activity::Closed { last: true }
            )
        })
        .collect();
    assert_eq!(
        edges,
        [
            &Activity::Opened { first: true },
            &Activity::Closed { last: true }
        ]
    );
}

#[tokio::test]
async fn an_unreachable_core_is_a_502_with_the_remedy() {
    // Bound but not listening: connections are refused, and the port stays ours.
    let closed = TcpSocket::new_v4().unwrap();
    closed.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let core = closed.local_addr().unwrap();
    let (address, gateway) = spawn_gateway(TestHost::work(Wake::At(core))).await;
    let response = send(
        address,
        &RawRequest {
            method: "POST".to_owned(),
            target: "/v1/messages".to_owned(),
            headers: vec![("x-api-key".to_owned(), key('a'))],
            body: "{}".to_owned(),
        },
    )
    .await;
    assert_eq!(response.status, 502);
    let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(
        body["error"]["message"],
        "The profile proxy is unavailable. Restart the profile from the sidebar if NonstopVibin offers it; otherwise quit and reopen NonstopVibin."
    );
    assert_eq!(gateway.in_flight(&work()), 0);
}

/// A WebSocket core: checks the handshake it gets, switches protocols with a cookie and a CORS
/// grant the agent must not see, then echoes bytes until EOF. With `refuse`, it answers 426.
async fn websocket_core(refuse: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        read_until(&mut socket, &mut head, b"\r\n\r\n").await;
        let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
        assert!(head.starts_with("get /v1/responses http/1.1\r\n"), "{head}");
        for line in [
            "connection: upgrade",
            "upgrade: websocket",
            "authorization: bearer core-work",
            "sec-websocket-key: x",
        ] {
            assert!(
                head.contains(&format!("\r\n{line}\r\n")),
                "{line} in {head}"
            );
        }
        assert!(
            !head.contains("x-api-key") && !head.contains("nv_"),
            "{head}"
        );
        if refuse {
            socket
                .write_all(b"HTTP/1.1 426 Upgrade Required\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
            return;
        }
        socket
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
                  Sec-WebSocket-Accept: y\r\nSet-Cookie: a=1\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
            )
            .await
            .unwrap();
        let (mut read, mut write) = socket.split();
        let _echoed = tokio::io::copy(&mut read, &mut write).await;
    });
    address
}

fn handshake(port: u16, key: &str) -> Vec<u8> {
    format!(
        "GET /p/work/v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-api-key: {key}\r\n\
         Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: x\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    )
    .into_bytes()
}

#[tokio::test]
async fn an_upgrade_relays_the_handshake_and_bytes() {
    let core = websocket_core(false).await;
    let (address, gateway) = spawn_gateway(TestHost::work(Wake::At(core))).await;
    let mut agent = TcpStream::connect(address).await.unwrap();
    // A frame in the same write as the handshake must still arrive after it.
    let mut first = handshake(address.port(), &key('a'));
    first.extend_from_slice(b"\x81\x04ping");
    agent.write_all(&first).await.unwrap();
    let mut received = Vec::new();
    read_until(&mut agent, &mut received, b"\r\n\r\n").await;
    let (response, start) = parse_head(&received).unwrap();
    assert_eq!(response.status, 101);
    assert_eq!(response.header("upgrade"), Some("websocket"));
    assert_eq!(response.header("connection"), Some("Upgrade"));
    assert_eq!(response.header("sec-websocket-accept"), Some("y"));
    assert_eq!(response.header("x-nonstopvibin-profile"), Some("work"));
    assert_eq!(response.header("set-cookie"), None);
    assert_eq!(response.header("access-control-allow-origin"), None);
    let mut echoed = received[start..].to_vec();
    read_until(&mut agent, &mut echoed, b"ping").await;
    agent.write_all(b"\x81\x04pong").await.unwrap();
    read_until(&mut agent, &mut echoed, b"pong").await;
    assert_eq!(gateway.in_flight(&work()), 1);
    drop(agent);
    within(LIMIT, "the release", || gateway.in_flight(&work()) == 0).await;
}

#[tokio::test]
async fn a_refused_upgrade_is_a_proxy_error() {
    for (core, key, status) in [
        (websocket_core(true).await, key('a'), 426),
        (websocket_core(true).await, key('z'), 401),
    ] {
        let (address, gateway) = spawn_gateway(TestHost::work(Wake::At(core))).await;
        let mut agent = TcpStream::connect(address).await.unwrap();
        agent
            .write_all(&handshake(address.port(), &key))
            .await
            .unwrap();
        let mut raw = Vec::new();
        agent.read_to_end(&mut raw).await.unwrap();
        let (response, start) = parse_head(&raw).unwrap();
        assert_eq!(
            (response.status, response.reason.as_str()),
            (status, "Proxy error")
        );
        assert_eq!(response.header("connection"), Some("close"));
        assert_eq!(response.header("content-length"), Some("0"));
        assert_eq!(start, raw.len());
        assert_eq!(gateway.in_flight(&work()), 0);
    }
}
