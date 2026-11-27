//! The task that owns one `claude` process: it writes the user messages and control requests,
//! reads stdout through the [`Translator`] into the response being served, and reports each
//! turn's end to [`Sessions`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hyper::body::Bytes;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};

use crate::conversation::Digest;
use crate::sessions::{Sessions, remove};
use crate::translate::{Failure, Line, Step, Translator, parse_line};

/// How long a process gets to exit once its stdin closes, before SIGKILL.
pub(crate) const GRACE: Duration = Duration::from_secs(5);

/// How a response's status and headers are decided.
#[derive(Debug)]
pub(crate) enum Head {
    /// An event stream follows on the body channel.
    Stream,
    /// The whole non-streaming answer.
    Json(Bytes),
    Error(Failure),
}

/// The response a turn is written to. When the client hangs up, the body channel closes.
#[derive(Debug)]
pub(crate) struct Sink {
    pub(crate) head: Option<oneshot::Sender<Head>>,
    pub(crate) body: mpsc::Sender<Bytes>,
    pub(crate) stream: bool,
}

/// What a request asks the process for.
#[derive(Debug)]
pub(crate) enum Request {
    /// A user message starts a turn; `tail` is its digest.
    Turn {
        content: Vec<Value>,
        sink: Sink,
        tail: Digest,
    },
    /// The held turn continues once its calls are answered; `tail` is the answers' digest.
    Continue { sink: Sink, tail: Digest },
}

/// Why the process stops.
enum End {
    /// Asked to; `true` keeps its transcript.
    Stopped(bool),
    /// Its turn failed, was interrupted or went beyond what the harness saw.
    Broken,
    /// A side call answered.
    Answered,
    /// It exited or closed stdout.
    Exited,
}

/// One process and the response it is answering.
pub(crate) struct Driver {
    pub(crate) sessions: Arc<Sessions>,
    pub(crate) key: u64,
    pub(crate) child: Child,
    pub(crate) translator: Translator,
    /// The files it reads, removed once it exits.
    pub(crate) files: Vec<PathBuf>,
    /// The session id; `None` for a side call, which runs one turn and keeps no transcript.
    pub(crate) session: Option<String>,
    /// A turn is in progress, streaming or holding tool calls.
    pub(crate) turning: bool,
}

fn user(content: &[Value]) -> Vec<u8> {
    let mut line = json!({
        "type": "user",
        "message": { "role": "user", "content": content },
        "parent_tool_use_id": null,
        "session_id": "",
        "client_composed": true,
    })
    .to_string()
    .into_bytes();
    line.push(b'\n');
    line
}

const INTERRUPT: &[u8] =
    b"{\"type\":\"control_request\",\"request_id\":\"interrupt\",\"request\":{\"subtype\":\"interrupt\"}}\n";

/// Resolves once the client of `sink` hung up; never without a sink.
async fn hung_up(sink: Option<&Sink>) {
    match sink {
        Some(sink) => sink.body.closed().await,
        None => std::future::pending().await,
    }
}

impl Driver {
    /// Serves requests until the process stops, then cleans up after it.
    pub(crate) async fn run(
        mut self,
        mut requests: mpsc::Receiver<Request>,
        mut stop: oneshot::Receiver<bool>,
    ) {
        let (Some(mut stdin), Some(stdout)) = (self.child.stdin.take(), self.child.stdout.take())
        else {
            self.finish(End::Exited, None, None).await;
            return;
        };
        let mut lines = BufReader::new(stdout).lines();
        let mut sink: Option<Sink> = None;
        let mut tail: Option<Digest> = None;
        let mut interrupted = false;
        let end = loop {
            tokio::select! {
                biased;
                keep = &mut stop => break End::Stopped(keep.unwrap_or(false)),
                request = requests.recv() => {
                    let Some(request) = request else { break End::Stopped(false) };
                    let (next, digest, content) = match request {
                        Request::Turn { content, sink, tail } => (sink, tail, Some(content)),
                        Request::Continue { sink, tail } => (sink, tail, None),
                    };
                    self.translator.respond(next.stream);
                    sink = Some(next);
                    tail = Some(digest);
                    interrupted = false;
                    if let Some(content) = content {
                        self.turning = true;
                        if write(&mut stdin, &user(&content)).await.is_err() {
                            break End::Exited;
                        }
                    }
                }
                () = hung_up(sink.as_ref()), if !interrupted => {
                    interrupted = true;
                    if write(&mut stdin, INTERRUPT).await.is_err() {
                        break End::Exited;
                    }
                }
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { break End::Exited };
                    if let Some(end) = self.line(&line, &mut sink, &mut tail).await {
                        break end;
                    }
                }
            }
        };
        self.finish(end, sink, Some(stdin)).await;
    }

    /// Handles one stdout line; `Some` when the process must stop.
    async fn line(
        &mut self,
        line: &str,
        sink: &mut Option<Sink>,
        tail: &mut Option<Digest>,
    ) -> Option<End> {
        let line = parse_line(line);
        if let Line::Init { session } = &line
            && self.session.is_some()
        {
            self.sessions.session_id(self.key, session);
            self.session = Some(session.clone());
        }
        for step in self.translator.feed(&line) {
            let Some(current) = sink.as_mut() else {
                // Claude Code went on with no response to write to.
                return Some(End::Broken);
            };
            match step {
                Step::Send(bytes) => {
                    if let Some(head) = current.head.take() {
                        let _ = head.send(Head::Stream);
                    }
                    // A client that hung up is noticed by the loop, which interrupts.
                    let _ = current.body.send(bytes).await;
                }
                Step::Hold { calls, turn, body } => {
                    answer(current, body);
                    *sink = None;
                    let tail = tail.take()?;
                    self.sessions.held(self.key, [tail, turn], calls);
                }
                Step::Done { turn, body } => {
                    answer(current, body);
                    *sink = None;
                    self.turning = false;
                    if self.session.is_none() {
                        return Some(End::Answered);
                    }
                    if self.translator.diverged() {
                        return Some(End::Broken);
                    }
                    let tail = tail.take()?;
                    self.sessions.finished(self.key, [tail, turn]);
                }
                Step::Failed(failure) => {
                    if let (Some(failure), Some(head)) = (failure, current.head.take()) {
                        let _ = head.send(Head::Error(failure));
                    }
                    *sink = None;
                    return Some(End::Broken);
                }
            }
        }
        None
    }

    /// Ends the process: a turn in progress is interrupted first, since Claude Code holding tool
    /// calls does not exit when its stdin closes; then its stdin closes, it gets [`GRACE`] to
    /// exit, then SIGKILL. A response still open fails, its files go, and its transcript too
    /// unless it was kept.
    async fn finish(mut self, end: End, sink: Option<Sink>, mut stdin: Option<ChildStdin>) {
        if let Some(stdin) = stdin.as_mut()
            && self.turning
            && matches!(end, End::Stopped(_))
        {
            let _ = write(stdin, INTERRUPT).await;
        }
        drop(stdin);
        if tokio::time::timeout(GRACE, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
        if let Some(head) = sink.and_then(|mut sink| sink.head.take()) {
            let _ = head.send(Head::Error(Failure::new(
                502,
                "api_error",
                "Claude Code stopped before it answered.",
            )));
        }
        let keep = matches!(end, End::Stopped(true));
        let files = std::mem::take(&mut self.files);
        let transcript = self
            .session
            .as_deref()
            .filter(|_| !keep)
            .and_then(|session| self.sessions.paths.transcript(session));
        tokio::task::spawn_blocking(move || {
            remove(&files);
            remove(transcript.as_slice());
        });
        self.sessions.ended(self.key);
    }
}

/// A non-streaming response's whole answer.
fn answer(sink: &mut Sink, body: Option<Bytes>) {
    if let (Some(body), Some(head)) = (body, sink.head.take()) {
        let _ = head.send(Head::Json(body));
    }
}

async fn write(stdin: &mut ChildStdin, bytes: &[u8]) -> std::io::Result<()> {
    stdin.write_all(bytes).await?;
    stdin.flush().await
}

/// Reads stdout until `request_id`'s control response, for a probe.
pub(crate) async fn control(
    lines: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    request_id: &str,
) -> Option<Value> {
    while let Ok(Some(line)) = lines.next_line().await {
        if let Line::Control {
            request_id: answered,
            response,
        } = parse_line(&line)
            && answered == request_id
        {
            return Some(response);
        }
    }
    None
}
