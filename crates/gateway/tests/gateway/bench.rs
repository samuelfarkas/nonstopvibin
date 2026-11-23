//! CPU the gateway spends per streamed event and per large request, read from its own
//! thread's `/proc` schedstat. The stub core and the client run on other threads, so the
//! numbers are the gateway's alone:
//!
//! ```text
//! cargo test --release -p nsv-gateway --test gateway bench -- --ignored --nocapture
//! ```

use std::net::SocketAddr;
use std::sync::mpsc;
use std::time::Duration;

use nsv_domain::id::ProfileId;
use nsv_gateway::Gateway;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::common::{TestHost, WORK, Wake, key, spawn_gateway};

const EVENTS: usize = 200;
const ROUNDS: usize = 7;
/// A text delta as Anthropic streams it, 180 bytes with its framing.
const EVENT: &[u8] = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"The quick brown fox jumps over the lazy dog, again and again.\"}}\n\n";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// The core: `x-bench-events: N` streams N events 5 ms apart; a POST answers with the number
/// of body bytes it read.
async fn core(listener: TcpListener) {
    while let Ok((mut socket, _)) = listener.accept().await {
        tokio::spawn(async move {
            let mut buffer = Vec::new();
            let mut chunk = vec![0_u8; 64 * 1024];
            let end = loop {
                let read = socket.read(&mut chunk).await.unwrap();
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    break end;
                }
            };
            let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
            let header = |name: &str| {
                head.lines()
                    .find_map(|line| line.strip_prefix(&format!("{name}: ")))
                    .map(|value| value.trim().parse::<usize>().unwrap())
            };
            if let Some(events) = header("x-bench-events") {
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n")
                    .await
                    .unwrap();
                let mut frame = format!("{:x}\r\n", EVENT.len()).into_bytes();
                frame.extend_from_slice(EVENT);
                frame.extend_from_slice(b"\r\n");
                for _ in 0..events {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    socket.write_all(&frame).await.unwrap();
                }
                socket.write_all(b"0\r\n\r\n").await.unwrap();
                return;
            }
            let length = header("content-length").unwrap();
            let mut read = buffer.len() - end - 4;
            while read < length {
                read += socket.read(&mut chunk).await.unwrap();
            }
            let body = read.to_string();
            let answer = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(answer.as_bytes()).await.unwrap();
        });
    }
}

/// The floor: a byte relay on the gateway's thread, with the same sockets and wakeups but no
/// HTTP, routing or header work.
async fn raw_relay(core: SocketAddr) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut agent, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                agent.set_nodelay(true).unwrap();
                let mut upstream = TcpStream::connect(core).await.unwrap();
                upstream.set_nodelay(true).unwrap();
                let _relayed = tokio::io::copy_bidirectional(&mut agent, &mut upstream).await;
            });
        }
    });
    address
}

/// The id of the thread named `bench-gateway`.
fn gateway_tid() -> String {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .find(|tid| {
            std::fs::read_to_string(format!("/proc/self/task/{tid}/comm"))
                .is_ok_and(|name| name.trim() == "bench-gateway")
        })
        .unwrap()
}

/// Nanoseconds on CPU, and read and write syscalls, of one thread.
fn thread_counters(tid: &str) -> (u64, u64, u64) {
    let schedstat = std::fs::read_to_string(format!("/proc/self/task/{tid}/schedstat")).unwrap();
    let cpu = schedstat
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let io = std::fs::read_to_string(format!("/proc/self/task/{tid}/io")).unwrap();
    let field = |name: &str| {
        io.lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    (cpu, field("syscr:"), field("syscw:"))
}

#[derive(Clone, Copy)]
enum Scenario {
    Stream(usize),
    Upload(usize),
}

/// Sends one request through the gateway and checks every event or byte arrived.
async fn run(gateway: SocketAddr, scenario: Scenario) {
    let mut stream = TcpStream::connect(gateway).await.unwrap();
    let key = key('a');
    let (head, body) = match scenario {
        Scenario::Stream(events) => (
            format!(
                "POST /v1/messages HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {key}\r\nx-bench-events: {events}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n"
            ),
            b"{}".to_vec(),
        ),
        Scenario::Upload(bytes) => (
            format!(
                "POST /v1/messages HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {key}\r\nContent-Length: {bytes}\r\nConnection: close\r\n\r\n"
            ),
            vec![b'x'; bytes],
        ),
    };
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(&body).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    match scenario {
        Scenario::Stream(events) => {
            let received = response.windows(7).filter(|w| w == b"event: ").count();
            assert_eq!(received, events);
        }
        Scenario::Upload(bytes) => {
            assert!(response.ends_with(format!("\r\n\r\n{bytes}").as_bytes()));
        }
    }
}

fn median_range(mut values: Vec<f64>) -> String {
    values.sort_by(f64::total_cmp);
    format!(
        "median {:.1}, range {:.1}–{:.1}",
        values[values.len() / 2],
        values[0],
        values[values.len() - 1]
    )
}

#[test]
#[ignore = "benchmark; run it in release with --ignored --nocapture"]
#[expect(clippy::print_stderr, reason = "the benchmark reports its numbers")]
#[expect(
    clippy::cast_precision_loss,
    reason = "nanosecond and syscall counts of one run stay far below 2^52"
)]
fn gateway_cpu_per_event_and_request() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let core_address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        runtime().block_on(async move { core(TcpListener::from_std(listener).unwrap()).await });
    });
    let (sender, receiver) = mpsc::channel::<(String, SocketAddr, SocketAddr, Gateway<TestHost>)>();
    let thread = std::thread::Builder::new().name("bench-gateway".to_owned());
    let _gateway_thread = thread.spawn(move || {
        runtime().block_on(async move {
            let tid = gateway_tid();
            let (address, gateway) = spawn_gateway(TestHost::work(Wake::At(core_address))).await;
            let raw = raw_relay(core_address).await;
            sender.send((tid, address, raw, gateway)).unwrap();
            std::future::pending::<()>().await;
        });
    });
    let (tid, gateway_address, raw_address, gateway) = receiver.recv().unwrap();
    let work = ProfileId::parse(WORK).unwrap();
    let mut scenarios = Vec::new();
    for (side, address) in [("gateway", gateway_address), ("raw relay", raw_address)] {
        scenarios.extend([
            (
                format!("{side}, stream of 0 events"),
                address,
                Scenario::Stream(0),
            ),
            (
                format!("{side}, stream of 200 events"),
                address,
                Scenario::Stream(EVENTS),
            ),
            (
                format!("{side}, 290 KB request"),
                address,
                Scenario::Upload(290 * 1024),
            ),
            (
                format!("{side}, 1 MB request"),
                address,
                Scenario::Upload(1024 * 1024),
            ),
        ]);
    }
    let client = runtime();
    // Warm-up: first connections, allocator growth and lazy statics.
    for (_, address, scenario) in &scenarios {
        client.block_on(run(*address, *scenario));
    }
    let mut cpu = vec![Vec::new(); scenarios.len()];
    let mut syscalls = vec![Vec::new(); scenarios.len()];
    for _ in 0..ROUNDS {
        for (index, (_, address, scenario)) in scenarios.iter().enumerate() {
            let before = thread_counters(&tid);
            client.block_on(async {
                run(*address, *scenario).await;
                // The gateway finishes its side after the client reads EOF.
                while gateway.in_flight(&work) > 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            });
            let after = thread_counters(&tid);
            cpu[index].push((after.0 - before.0) as f64 / 1000.0);
            syscalls[index].push((after.1 - before.1 + after.2 - before.2) as f64);
        }
    }
    for (index, (name, _, _)) in scenarios.iter().enumerate() {
        eprintln!(
            "{name}: CPU µs {}; read+write syscalls (recv and send are not counted) {}",
            median_range(cpu[index].clone()),
            median_range(syscalls[index].clone())
        );
    }
    let per_event = |values: &[f64], base: &[f64]| -> Vec<f64> {
        values
            .iter()
            .zip(base)
            .map(|(all, zero)| (all - zero) / EVENTS as f64)
            .collect()
    };
    for (side, base) in [("gateway", 0), ("raw relay", 4)] {
        eprintln!(
            "{side}: per event all-in (200-event stream / 200) CPU µs {}; marginal ((200 − 0 events) / 200) CPU µs {}, write syscalls {}",
            median_range(
                cpu[base + 1]
                    .iter()
                    .map(|all| all / EVENTS as f64)
                    .collect()
            ),
            median_range(per_event(&cpu[base + 1], &cpu[base])),
            median_range(per_event(&syscalls[base + 1], &syscalls[base]))
        );
    }
}
