//! One profile's `claude` processes: the live sessions, the records of stopped ones, the tool
//! calls they hold, and when they stop. The state is behind one lock that no `await` holds.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nsv_domain::id::ProfileId;
use nsv_domain::secret::McpToken;
use nsv_domain::sync::locked;
use nsv_files::DataDir;
use serde_json::Value;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::conversation::{Conversation, Digest, Fingerprint, Tool, ToolResult};
use crate::driver::Request;
use crate::process::Persistence;
use crate::route::{self, Route, Stopped};

/// Live processes per profile.
pub(crate) const LIVE_LIMIT: usize = 3;
/// Stopped sessions kept for resuming, per profile.
pub(crate) const RECORD_LIMIT: usize = 32;
/// An idle process stops after this.
pub(crate) const IDLE: Duration = Duration::from_mins(5);
/// A process holding tool calls the harness never answers stops after this.
pub(crate) const HOLDING: Duration = Duration::from_mins(30);

/// Where a profile's processes keep their files.
#[derive(Debug, Clone)]
pub(crate) struct Paths {
    /// `CLAUDE_CONFIG_DIR`: the sign-in and the transcripts.
    pub(crate) config: PathBuf,
    /// The empty working directory.
    pub(crate) work: PathBuf,
    /// System prompts and MCP configurations of running processes.
    pub(crate) run: PathBuf,
}

impl Paths {
    pub(crate) fn new(data: &DataDir, profile: &ProfileId) -> Self {
        Self {
            config: data.claude_agent_config(profile),
            work: data.claude_agent_work(profile),
            run: data.claude_agent_run(profile),
        }
    }

    /// Creates the directories owner-only. Blocks on the file system.
    pub(crate) fn prepare(&self) -> io::Result<()> {
        for directory in [&self.config, &self.work, &self.run] {
            nsv_files::owner_only_directory(directory, || Ok::<(), io::Error>(()))?;
        }
        Ok(())
    }

    /// Where Claude Code keeps the transcripts of sessions run in [`Paths::work`].
    pub(crate) fn transcripts(&self) -> PathBuf {
        let project: String = self
            .work
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        self.config.join("projects").join(project)
    }

    pub(crate) fn transcript(&self, session: &str) -> Option<PathBuf> {
        let uuid =
            session.len() == 36 && session.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
        uuid.then(|| self.transcripts().join(format!("{session}.jsonl")))
    }
}

/// Deletes the files a run of the daemon left: every transcript of the working directory and
/// every process file. No record names them any more. Blocks on the file system.
pub(crate) fn sweep(paths: &Paths) {
    for directory in [paths.transcripts(), paths.run.clone()] {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ours = directory == paths.run
                || path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl");
            if ours && entry.file_type().is_ok_and(|kind| kind.is_file()) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// What a live process is doing.
#[derive(Debug)]
pub(crate) enum Activity {
    /// Its process is being started for a request.
    Starting,
    /// A turn is streaming to a response.
    Streaming,
    /// Its turn stopped on these calls of the harness's tools.
    Holding { calls: Vec<String>, since: Instant },
    /// Its last turn ended.
    Idle { since: Instant },
}

/// A held tool call's answer, or the call waiting for it.
#[derive(Debug)]
enum Slot {
    Answered(ToolResult),
    Waiting {
        answer: oneshot::Sender<ToolResult>,
        request: Value,
    },
}

/// What a `tools/call` gets.
#[derive(Debug)]
pub(crate) enum Answer {
    Now(ToolResult),
    Later(oneshot::Receiver<ToolResult>),
}

/// A live process.
#[derive(Debug)]
struct Live {
    key: u64,
    /// The session id; `None` for a side call, which keeps no transcript.
    session: Option<String>,
    fingerprint: Fingerprint,
    seen: Vec<Digest>,
    activity: Activity,
    used: Instant,
    token: McpToken,
    tools: Arc<[Tool]>,
    calls: HashMap<String, Slot>,
    requests: Option<mpsc::Sender<Request>>,
    /// Stops the process; `true` keeps its transcript.
    stop: Option<oneshot::Sender<bool>>,
    task: Option<JoinHandle<()>>,
}

impl Live {
    /// Asks its driver to stop and answers the driver's task.
    fn stop(mut self, keep: bool) -> Option<JoinHandle<()>> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(keep);
        }
        self.task
    }

    fn deadline(&self) -> Option<Instant> {
        match self.activity {
            Activity::Idle { since } => Some(since + IDLE),
            Activity::Holding { since, .. } => Some(since + HOLDING),
            Activity::Starting | Activity::Streaming => None,
        }
    }
}

/// A stopped session that can be resumed.
#[derive(Debug)]
struct Record {
    session: String,
    seen: Vec<Digest>,
    /// Its process until it has exited; a resume waits for it.
    exiting: Option<JoinHandle<()>>,
}

#[derive(Debug, Default)]
struct State {
    next: u64,
    lives: Vec<Live>,
    records: VecDeque<Record>,
    /// The deadline task runs.
    timer: bool,
    closed: bool,
}

/// How a routed request proceeds.
#[derive(Debug)]
pub(crate) enum Plan {
    /// The session continues its held turn once the results reach its calls.
    Continue {
        key: u64,
        requests: mpsc::Sender<Request>,
        results: Vec<ToolResult>,
    },
    /// The user message goes into the idle session.
    Append {
        key: u64,
        requests: mpsc::Sender<Request>,
        content: Vec<Value>,
    },
    /// A new process starts, after `previous` exited.
    Start {
        key: u64,
        persistence: Persistence,
        content: Vec<Value>,
        token: McpToken,
        previous: Option<JoinHandle<()>>,
    },
}

/// Shutdown began.
#[derive(Debug)]
pub(crate) struct Closed;

/// One profile's processes; see the module documentation.
#[derive(Debug)]
pub(crate) struct Sessions {
    pub(crate) profile: ProfileId,
    pub(crate) paths: Paths,
    state: Mutex<State>,
    /// Notified whenever a process changes activity or goes: requests waiting for room and
    /// the deadline task look again.
    changed: Notify,
}

impl Sessions {
    pub(crate) fn new(profile: ProfileId, paths: Paths) -> Self {
        Self {
            profile,
            paths,
            state: Mutex::default(),
            changed: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        locked(&self.state)
    }

    /// Routes `conversation` and claims what serves it, waiting while every process streams.
    pub(crate) async fn claim(
        self: &Arc<Self>,
        conversation: &Conversation,
    ) -> Result<Plan, Closed> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(plan) = self.try_claim(conversation)? {
                return Ok(plan);
            }
            changed.await;
        }
    }

    fn try_claim(self: &Arc<Self>, conversation: &Conversation) -> Result<Option<Plan>, Closed> {
        let mut state = self.lock();
        if state.closed {
            return Err(Closed);
        }
        let (persistence, seen, previous, content) = match routed(&state, conversation) {
            Route::Continue { session, results } => {
                return Ok(
                    reclaim(&mut state, session).map(|(key, requests)| Plan::Continue {
                        key,
                        requests,
                        results,
                    }),
                );
            }
            Route::Append { session, message } => {
                return Ok(
                    reclaim(&mut state, session).map(|(key, requests)| Plan::Append {
                        key,
                        requests,
                        content: message,
                    }),
                );
            }
            Route::Resume {
                from: Stopped::Live(index),
                message,
            } => {
                let live = state.lives.remove(index);
                let session = live.session.clone().unwrap_or_default();
                let seen = live.seen.clone();
                (Persistence::Resume(session), seen, live.stop(true), message)
            }
            Route::Resume {
                from: Stopped::Record(index),
                message,
            } if room(&state) => {
                let Some(record) = state.records.remove(index) else {
                    return Ok(None);
                };
                let persistence = Persistence::Resume(record.session);
                (persistence, record.seen, record.exiting, message)
            }
            Route::Fresh { message } if room(&state) => {
                let session = nsv_domain::random::uuid().map_err(|_| Closed)?;
                let seen = conversation.history_digests().to_vec();
                (Persistence::New(session), seen, None, message)
            }
            Route::Side { message } if room(&state) => {
                (Persistence::None, Vec::new(), None, message)
            }
            Route::Resume { .. } | Route::Fresh { .. } | Route::Side { .. } => return Ok(None),
        };
        self.evict(&mut state);
        let token = McpToken::generate().map_err(|_| Closed)?;
        state.next += 1;
        let key = state.next;
        state.lives.push(Live {
            key,
            session: match &persistence {
                Persistence::New(id) | Persistence::Resume(id) => Some(id.clone()),
                Persistence::None => None,
            },
            fingerprint: conversation.fingerprint().clone(),
            seen,
            activity: Activity::Starting,
            used: Instant::now(),
            token: token.clone(),
            tools: Arc::clone(&conversation.settings.tools),
            calls: HashMap::new(),
            requests: None,
            stop: None,
            task: None,
        });
        if !state.timer {
            state.timer = true;
            tokio::spawn(Arc::clone(self).deadlines());
        }
        Ok(Some(Plan::Start {
            key,
            persistence,
            content,
            token,
            previous,
        }))
    }

    /// Makes room for one more process: the least recently used idle or holding one stops.
    fn evict(&self, state: &mut State) {
        if state.lives.len() < LIVE_LIMIT {
            return;
        }
        let Some(index) = state
            .lives
            .iter()
            .enumerate()
            .filter(|(_, live)| live.deadline().is_some())
            .min_by_key(|(_, live)| live.used)
            .map(|(index, _)| index)
        else {
            return;
        };
        let live = state.lives.remove(index);
        self.retire(state, live);
    }

    /// Stops `live`; an idle session's record is kept for resuming.
    fn retire(&self, state: &mut State, live: Live) {
        let idle = matches!(live.activity, Activity::Idle { .. });
        let session = live.session.clone().filter(|_| idle);
        let seen = live.seen.clone();
        let exiting = live.stop(idle);
        if let Some(session) = session {
            state.records.push_back(Record {
                session,
                seen,
                exiting,
            });
            while state.records.len() > RECORD_LIMIT {
                if let Some(forgotten) = state.records.pop_front() {
                    self.forget(&forgotten.session);
                }
            }
        }
        self.changed.notify_waiters();
    }

    /// Deletes a forgotten session's transcript.
    pub(crate) fn forget(&self, session: &str) {
        if let Some(path) = self.paths.transcript(session) {
            tokio::task::spawn_blocking(move || {
                let _ = std::fs::remove_file(path);
            });
        }
    }

    /// The process `key` started: its driver's channels and task.
    pub(crate) fn started(
        &self,
        key: u64,
        requests: mpsc::Sender<Request>,
        stop: oneshot::Sender<bool>,
        task: JoinHandle<()>,
    ) -> bool {
        let mut state = self.lock();
        let Some(live) = state.lives.iter_mut().find(|live| live.key == key) else {
            let _ = stop.send(false);
            return false;
        };
        live.requests = Some(requests);
        live.stop = Some(stop);
        live.task = Some(task);
        live.activity = Activity::Streaming;
        true
    }

    /// The process `key` reported its session id.
    pub(crate) fn session_id(&self, key: u64, session: &str) {
        if let Some(live) = self.lock().lives.iter_mut().find(|live| live.key == key)
            && live.session.is_some()
        {
            live.session = Some(session.to_owned());
        }
    }

    /// The turn of `key` stopped on the harness's `calls`: it has seen `tail` and `turn`.
    pub(crate) fn held(&self, key: u64, seen: [Digest; 2], calls: Vec<String>) {
        self.update(
            key,
            seen,
            Activity::Holding {
                calls,
                since: Instant::now(),
            },
        );
    }

    /// The turn of `key` ended: it has seen `tail` and `turn`.
    pub(crate) fn finished(&self, key: u64, seen: [Digest; 2]) {
        self.update(
            key,
            seen,
            Activity::Idle {
                since: Instant::now(),
            },
        );
    }

    fn update(&self, key: u64, seen: [Digest; 2], activity: Activity) {
        if let Some(live) = self.lock().lives.iter_mut().find(|live| live.key == key) {
            live.seen.extend(seen);
            live.activity = activity;
            live.used = Instant::now();
        }
        self.changed.notify_waiters();
    }

    /// The process `key` is gone, or never started: it leaves the live set.
    pub(crate) fn ended(&self, key: u64) {
        let mut state = self.lock();
        if let Some(index) = state.lives.iter().position(|live| live.key == key) {
            let live = state.lives.remove(index);
            drop(live.stop(false));
        }
        drop(state);
        self.changed.notify_waiters();
    }

    /// The live process whose MCP token `presented` is.
    pub(crate) fn by_token(&self, presented: &str) -> Option<(u64, Arc<[Tool]>)> {
        self.lock()
            .lives
            .iter()
            .find(|live| live.token.matches(presented))
            .map(|live| (live.key, Arc::clone(&live.tools)))
    }

    /// The harness's answer to `tool_use` if it arrived, else a wait for it.
    pub(crate) fn answer(&self, key: u64, tool_use: &str, request: &Value) -> Option<Answer> {
        let mut state = self.lock();
        let live = state.lives.iter_mut().find(|live| live.key == key)?;
        if let Some(Slot::Answered(result)) = live.calls.remove(tool_use) {
            return Some(Answer::Now(result));
        }
        let (answer, waiting) = oneshot::channel();
        live.calls.insert(
            tool_use.to_owned(),
            Slot::Waiting {
                answer,
                request: request.clone(),
            },
        );
        Some(Answer::Later(waiting))
    }

    /// Hands the harness's results to the calls of `key`: waiting ones answer now, the others
    /// when Claude Code makes them.
    pub(crate) fn deliver(&self, key: u64, results: Vec<ToolResult>) {
        let mut state = self.lock();
        let Some(live) = state.lives.iter_mut().find(|live| live.key == key) else {
            return;
        };
        for result in results {
            match live.calls.remove(&result.id) {
                Some(Slot::Waiting { answer, .. }) => {
                    let _ = answer.send(result);
                }
                Some(Slot::Answered(_)) | None => {
                    live.calls.insert(result.id.clone(), Slot::Answered(result));
                }
            }
        }
    }

    /// Claude Code cancelled the call it made as `request`: it is no longer held.
    pub(crate) fn cancel(&self, key: u64, request: &Value) {
        if let Some(live) = self.lock().lives.iter_mut().find(|live| live.key == key) {
            live.calls.retain(
                |_, slot| !matches!(slot, Slot::Waiting { request: held, .. } if held == request),
            );
        }
    }

    /// Stops every process and forgets every record, waiting for the processes to exit.
    pub(crate) async fn close(&self, closing: bool) {
        let tasks: Vec<JoinHandle<()>> = {
            let mut state = self.lock();
            state.closed |= closing;
            let lives = std::mem::take(&mut state.lives);
            let records = std::mem::take(&mut state.records);
            for record in &records {
                self.forget(&record.session);
            }
            lives
                .into_iter()
                .filter_map(|live| live.stop(false))
                .chain(records.into_iter().filter_map(|record| record.exiting))
                .collect()
        };
        self.changed.notify_waiters();
        for task in tasks {
            let _ = task.await;
        }
    }

    /// Stops processes whose idle or holding time ran out, sleeping until the next such
    /// deadline. It exits once no process is live, so an idle daemon has no timer.
    async fn deadlines(self: Arc<Self>) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let next = {
                let mut state = self.lock();
                let now = Instant::now();
                let expired: Vec<usize> = state
                    .lives
                    .iter()
                    .enumerate()
                    .filter(|(_, live)| live.deadline().is_some_and(|at| at <= now))
                    .map(|(index, _)| index)
                    .rev()
                    .collect();
                for index in expired {
                    let live = state.lives.remove(index);
                    self.retire(&mut state, live);
                }
                if state.lives.is_empty() {
                    state.timer = false;
                    return;
                }
                state.lives.iter().filter_map(Live::deadline).min()
            };
            match next {
                Some(at) => {
                    tokio::select! {
                        () = tokio::time::sleep_until(at) => {}
                        () = &mut changed => {}
                    }
                }
                None => changed.await,
            }
        }
    }
}

/// Routes `conversation` among the profile's live sessions and records.
fn routed(state: &State, conversation: &Conversation) -> Route {
    let lives: Vec<route::Live<'_>> = state
        .lives
        .iter()
        .map(|live| route::Live {
            fingerprint: &live.fingerprint,
            seen: &live.seen,
            activity: match &live.activity {
                Activity::Idle { .. } => route::Activity::Idle,
                Activity::Holding { calls, .. } => route::Activity::Holding(calls),
                Activity::Starting | Activity::Streaming => route::Activity::Busy,
            },
        })
        .collect();
    let records: Vec<&[Digest]> = state
        .records
        .iter()
        .map(|record| record.seen.as_slice())
        .collect();
    route::route(&lives, &records, conversation)
}

/// Claims the live session at `index` for a new response: its key and request channel.
fn reclaim(state: &mut State, index: usize) -> Option<(u64, mpsc::Sender<Request>)> {
    let live = state.lives.get_mut(index)?;
    let requests = live.requests.clone()?;
    live.activity = Activity::Streaming;
    live.used = Instant::now();
    Some((live.key, requests))
}

/// Whether one more process fits, if need be by stopping an idle or holding one.
fn room(state: &State) -> bool {
    state.lives.len() < LIVE_LIMIT || state.lives.iter().any(|live| live.deadline().is_some())
}

/// Removes `paths`, ignoring files already gone. Blocks on the file system.
pub(crate) fn remove(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_file(Path::new(path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcripts_live_under_the_work_directorys_project_name() {
        let paths = Paths {
            config: PathBuf::from("/data/claude-agent/p"),
            work: PathBuf::from("/data/claude-agent/work/p.1"),
            run: PathBuf::from("/data/claude-agent/run/p"),
        };
        assert_eq!(
            paths.transcripts(),
            Path::new("/data/claude-agent/p/projects/-data-claude-agent-work-p-1")
        );
        assert_eq!(
            paths.transcript("0b5a4f3e-1c2d-4e5f-8a9b-0c1d2e3f4a5b"),
            Some(PathBuf::from(
                "/data/claude-agent/p/projects/-data-claude-agent-work-p-1/0b5a4f3e-1c2d-4e5f-8a9b-0c1d2e3f4a5b.jsonl"
            ))
        );
        assert_eq!(
            paths.transcript("../../etc/passwd-0000000000000000000"),
            None
        );
    }
}
