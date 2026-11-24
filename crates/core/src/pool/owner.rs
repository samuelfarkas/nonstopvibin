//! The pool's owner task and the tasks it spawns. The owner never awaits: it applies one
//! command, publishes the new state, tells the hooks, and hands anything slow to a task that
//! reports back through the same queue.

use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_net::Method;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

use super::backend::{CoreHandle, exit_text};
use super::{
    Context, Core, CoreEvent, CoreId, CoreKeys, Phase, PoolState, ProfileCore, RETRY, management,
};
use crate::config::write_config;

type Reply = oneshot::Sender<Result<(), AppError>>;
/// Asks a core's task to terminate it, and answers once it exited.
type Terminate = oneshot::Sender<io::Result<()>>;

pub(super) enum Command {
    Start {
        profile: ProfileId,
        automatic: bool,
        reply: Reply,
    },
    Stop {
        profile: ProfileId,
        reply: Reply,
    },
    /// A credential swap begins; answers whether the core was running or starting.
    Hold {
        profile: ProfileId,
        reply: oneshot::Sender<bool>,
    },
    /// The swap ended; `restart` starts the core again.
    Release {
        profile: ProfileId,
        restart: bool,
        reply: Reply,
    },
    UseBinary {
        binary: PathBuf,
        reply: oneshot::Sender<()>,
    },
    BeginShutdown {
        reply: oneshot::Sender<()>,
    },
    /// From a core's task: its process runs (or could not launch, then `phase` says why).
    Launched {
        profile: ProfileId,
        core: Core,
    },
    /// From a core's task: it answered; the owner acknowledges once it is published.
    Answered {
        profile: ProfileId,
        id: CoreId,
        published: oneshot::Sender<()>,
    },
    /// From a core's task: it is terminating a core that did not become ready.
    Terminating {
        profile: ProfileId,
        id: CoreId,
    },
    /// From a core's task: the process exited; `text` is the user's message if unexpected.
    Exited {
        profile: ProfileId,
        id: CoreId,
        text: String,
    },
    /// From a core's task: the start ended.
    StartEnded {
        profile: ProfileId,
        result: Result<(), AppError>,
    },
    /// From a stop's task.
    StopEnded {
        profile: ProfileId,
        result: Result<(), AppError>,
    },
}

struct Start {
    automatic: bool,
    core: CoreId,
    waiters: Vec<Reply>,
}

#[derive(Default)]
struct Slot {
    core: Option<Core>,
    /// Reaches the task of the latest core while it runs.
    terminate: Option<oneshot::Sender<Terminate>>,
    start: Option<Start>,
    /// The waiters of the stop in flight.
    stop: Option<Vec<Reply>>,
    /// A credential swap holds it stopped.
    held: bool,
    /// Starts that arrived during a stop, begun once it ends.
    after_stop: Vec<(bool, Reply)>,
    /// Stops that arrived during a start, begun once it ends.
    after_start: Vec<Reply>,
}

impl Slot {
    fn running(&self) -> bool {
        self.core.as_ref().is_some_and(|core| core.phase.answered())
    }

    fn alive(&self) -> bool {
        self.core.as_ref().is_some_and(|core| core.phase.alive())
    }

    fn holding(&self) -> bool {
        self.stop.is_some() || self.held
    }
}

pub(super) struct Owner {
    context: Arc<Context>,
    queue: mpsc::Receiver<Command>,
    sender: mpsc::WeakSender<Command>,
    published: watch::Sender<Arc<PoolState>>,
    closing: bool,
    binary: Arc<Path>,
    next: u64,
    slots: BTreeMap<ProfileId, Slot>,
}

impl Owner {
    pub(super) fn new(
        context: Arc<Context>,
        queue: mpsc::Receiver<Command>,
        sender: mpsc::WeakSender<Command>,
        published: watch::Sender<Arc<PoolState>>,
    ) -> Self {
        let binary = Arc::clone(&published.borrow().binary);
        Self {
            context,
            queue,
            sender,
            published,
            closing: false,
            binary,
            next: 0,
            slots: BTreeMap::new(),
        }
    }

    pub(super) async fn run(mut self) {
        while let Some(command) = self.queue.recv().await {
            self.apply(command);
        }
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Start {
                profile,
                automatic,
                reply,
            } => self.start(profile, automatic, reply),
            Command::Stop { profile, reply } => self.stop(profile, reply),
            Command::Hold { profile, reply } => {
                let slot = self.slots.entry(profile.clone()).or_default();
                let was = slot.running() || slot.start.is_some();
                slot.held = true;
                self.publish();
                self.emit(&profile, CoreEvent::Holding(true));
                let _ = reply.send(was);
            }
            Command::Release {
                profile,
                restart,
                reply,
            } => {
                let slot = self.slots.entry(profile.clone()).or_default();
                slot.held = false;
                let holding = slot.holding();
                self.publish();
                self.emit(&profile, CoreEvent::Holding(holding));
                if restart && !self.closing {
                    self.start(profile, false, reply);
                } else {
                    let _ = reply.send(Ok(()));
                }
            }
            Command::UseBinary { binary, reply } => {
                self.binary = Arc::from(binary);
                self.publish();
                let _ = reply.send(());
            }
            Command::BeginShutdown { reply } => {
                self.closing = true;
                self.publish();
                let _ = reply.send(());
            }
            Command::Launched { profile, core } => {
                let failure = match &core.phase {
                    Phase::Failed(text) => Some(text.to_string()),
                    _ => None,
                };
                self.slots.entry(profile.clone()).or_default().core = Some(core);
                self.publish();
                if let Some(error) = failure {
                    self.emit(&profile, CoreEvent::Exited { error: Some(error) });
                }
            }
            Command::Answered {
                profile,
                id,
                published,
            } => {
                if let Some(core) = self.current(&profile, id)
                    && core.phase == Phase::Starting
                {
                    core.phase = Phase::Running;
                    self.publish();
                }
                let _ = published.send(());
            }
            Command::Terminating { profile, id } => {
                if let Some(core) = self.current(&profile, id)
                    && core.phase.alive()
                {
                    core.phase = Phase::Stopping {
                        answered: core.phase.answered(),
                    };
                    self.publish();
                }
            }
            Command::Exited { profile, id, text } => self.exited(&profile, id, &text),
            Command::StartEnded { profile, result } => self.start_ended(profile, &result),
            Command::StopEnded { profile, result } => self.stop_ended(&profile, &result),
        }
    }

    /// The profile's latest core if it is `id`: a replaced core's late news changes nothing.
    fn current(&mut self, profile: &ProfileId, id: CoreId) -> Option<&mut Core> {
        self.slots
            .get_mut(profile)?
            .core
            .as_mut()
            .filter(|core| core.id == id)
    }

    fn publish(&self) {
        let profiles = self
            .slots
            .iter()
            .map(|(profile, slot)| {
                (
                    profile.clone(),
                    ProfileCore {
                        core: slot.core.clone(),
                        starting: slot.start.is_some(),
                        holding: slot.holding(),
                    },
                )
            })
            .collect();
        self.published.send_replace(Arc::new(PoolState {
            closing: self.closing,
            binary: Arc::clone(&self.binary),
            profiles,
        }));
    }

    fn emit(&self, profile: &ProfileId, event: CoreEvent) {
        self.context.hooks.event(profile, event);
    }

    fn start(&mut self, profile: ProfileId, automatic: bool, reply: Reply) {
        let closing = self.closing;
        let slot = self.slots.entry(profile.clone()).or_default();
        if slot.stop.is_some() {
            slot.after_stop.push((automatic, reply));
            return;
        }
        if closing {
            let _ = reply.send(Err(AppError::shutting_down()));
            return;
        }
        if slot.held {
            let _ = reply.send(Err(AppError::new(
                "This profile is reconnecting a subscription. Try again in a moment.",
                409,
            )));
            return;
        }
        if let Some(start) = &mut slot.start {
            start.waiters.push(reply);
            return;
        }
        if slot.running() {
            let _ = reply.send(Ok(()));
            return;
        }
        let Some(sender) = self.sender.upgrade() else {
            let _ = reply.send(Err(AppError::shutting_down()));
            return;
        };
        self.next += 1;
        let id = CoreId(self.next);
        let (terminate, requests) = oneshot::channel();
        // Any earlier core is gone: a start begins only when none answers and no stop is in
        // flight, and an abandoned start's core only stays during shutdown, when none begins.
        slot.terminate = Some(terminate);
        slot.start = Some(Start {
            automatic,
            core: id,
            waiters: vec![reply],
        });
        self.publish();
        self.emit(&profile, CoreEvent::Starting { automatic });
        tokio::spawn(
            Launch {
                context: Arc::clone(&self.context),
                sender,
                profile,
                id,
                binary: Arc::clone(&self.binary),
            }
            .run(requests),
        );
    }

    fn start_ended(&mut self, profile: ProfileId, result: &Result<(), AppError>) {
        let slot = self.slots.entry(profile.clone()).or_default();
        let Some(start) = slot.start.take() else {
            return;
        };
        if let (Err(error), Some(core)) = (result, &mut slot.core)
            && core.id == start.core
            && core.phase == Phase::Stopped
        {
            core.phase = Phase::Failed(Arc::from(error.message.as_str()));
        }
        // Whether its core runs as it ends: one can exit after answering, before the start ends.
        let running = slot.running();
        let error = if running {
            None
        } else {
            match (result, &slot.core) {
                (Err(error), _) => Some(error.message.clone()),
                (Ok(()), Some(core)) => match &core.phase {
                    Phase::Failed(text) => Some(text.to_string()),
                    _ => None,
                },
                (Ok(()), None) => None,
            }
        };
        let stops = std::mem::take(&mut slot.after_start);
        self.publish();
        self.emit(
            &profile,
            CoreEvent::Started {
                automatic: start.automatic,
                running,
                error,
            },
        );
        for waiter in start.waiters {
            let _ = waiter.send(result.clone());
        }
        if !stops.is_empty() {
            self.begin_stop(profile, stops);
        }
    }

    fn exited(&mut self, profile: &ProfileId, id: CoreId, text: &str) {
        let Some(core) = self.current(profile, id) else {
            return;
        };
        let error = match core.phase {
            Phase::Stopping { .. } => {
                core.phase = Phase::Stopped;
                None
            }
            Phase::Starting | Phase::Running => {
                let text = exited_message(text);
                core.phase = Phase::Failed(Arc::from(text.as_str()));
                Some(text)
            }
            Phase::Stopped | Phase::Failed(_) => return,
        };
        self.publish();
        self.emit(profile, CoreEvent::Exited { error });
    }

    fn stop(&mut self, profile: ProfileId, reply: Reply) {
        let slot = self.slots.entry(profile.clone()).or_default();
        if slot.start.is_some() {
            slot.after_start.push(reply);
            return;
        }
        if let Some(waiters) = &mut slot.stop {
            waiters.push(reply);
            return;
        }
        self.begin_stop(profile, vec![reply]);
    }

    fn begin_stop(&mut self, profile: ProfileId, waiters: Vec<Reply>) {
        let sender = self.sender.upgrade();
        let slot = self.slots.entry(profile.clone()).or_default();
        slot.stop = Some(waiters);
        let alive = slot.alive();
        let mut answered = false;
        if let Some(core) = &mut slot.core
            && alive
        {
            answered = core.phase.answered();
            core.phase = Phase::Stopping { answered };
        }
        let terminate = slot.terminate.take();
        self.publish();
        self.emit(&profile, CoreEvent::Holding(true));
        match (alive, sender) {
            (true, Some(sender)) => {
                let context = Arc::clone(&self.context);
                tokio::spawn(async move {
                    // A failed spawn has no process to end.
                    if answered {
                        context.hooks.before_stop(profile.clone()).await;
                    }
                    let result = match terminate {
                        Some(terminate) => terminated(terminate).await,
                        None => Ok(()),
                    };
                    let _ = sender.send(Command::StopEnded { profile, result }).await;
                });
            }
            _ => self.stop_ended(&profile, &Ok(())),
        }
    }

    fn stop_ended(&mut self, profile: &ProfileId, result: &Result<(), AppError>) {
        let slot = self.slots.entry(profile.clone()).or_default();
        let waiters = slot.stop.take().unwrap_or_default();
        let holding = slot.holding();
        let starts = std::mem::take(&mut slot.after_stop);
        self.publish();
        self.emit(profile, CoreEvent::Holding(holding));
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
        for (automatic, reply) in starts {
            self.start(profile.clone(), automatic, reply);
        }
    }
}

/// Asks the core's task to terminate it and waits until it exited.
async fn terminated(terminate: oneshot::Sender<Terminate>) -> Result<(), AppError> {
    let (reply, answer) = oneshot::channel();
    if terminate.send(reply).is_err() {
        // Its task ended: the core exited already.
        return Ok(());
    }
    match answer.await {
        Ok(Ok(())) | Err(_) => Ok(()),
        Ok(Err(error)) => Err(AppError::internal(format!(
            "Could not stop the proxy: {error}"
        ))),
    }
}

/// Core ports, 10001 to 20000: below the system's ephemeral range (from 32768 on Linux, 49152
/// on macOS), which outgoing connections and `bind(0)` draw from. A port found free here stays
/// free until the core binds it unless a program binds that very number. An ephemeral one could
/// go to any connection meanwhile, and the core would exit with "address already in use".
const FIRST_PORT: u32 = 10_001;
const PORTS: u32 = 10_000;

/// A core port on 127.0.0.1 nothing listened on a moment ago. Calls walk the range from a place
/// the process id picks, so concurrent starts and other instances get different ports.
fn unused_port() -> io::Result<u16> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let start = std::process::id().wrapping_mul(7_919);
    for _ in 0..PORTS {
        let offset = start.wrapping_add(NEXT.fetch_add(1, Ordering::Relaxed)) % PORTS;
        let port = u16::try_from(FIRST_PORT + offset).map_err(io::Error::other)?;
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            Ok(_) => return Ok(port),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::ErrorKind::AddrInUse.into())
}

/// What ends a readiness wait early.
enum Interrupt {
    Exited(io::Result<ExitStatus>),
    Abandoned,
}

/// Why a readiness wait ended without a ready core.
enum Unready {
    Exited(io::Result<ExitStatus>),
    Abandoned,
    /// The deadline passed; the last failure.
    Late(String),
}

impl From<Interrupt> for Unready {
    fn from(interrupt: Interrupt) -> Self {
        match interrupt {
            Interrupt::Exited(status) => Self::Exited(status),
            Interrupt::Abandoned => Self::Abandoned,
        }
    }
}

/// The error a profile shows after its core exited on its own.
fn exited_message(how: &str) -> String {
    format!(
        "Proxy exited ({how}). Profiles with subscriptions restart automatically, or use Restart."
    )
}

/// One core, from its launch until it exits.
struct Launch {
    context: Arc<Context>,
    sender: mpsc::Sender<Command>,
    profile: ProfileId,
    id: CoreId,
    binary: Arc<Path>,
}

impl Launch {
    async fn tell(&self, command: Command) {
        // The owner outlives every sender; if it is gone, nobody is left to tell.
        let _ = self.sender.send(command).await;
    }

    async fn end_start(&self, result: Result<(), AppError>) {
        self.tell(Command::StartEnded {
            profile: self.profile.clone(),
            result,
        })
        .await;
    }

    async fn run(self, requests: oneshot::Receiver<Terminate>) {
        let (port, keys) = match self.prepare().await {
            Ok(prepared) => prepared,
            Err(error) => return self.end_start(Err(error)).await,
        };
        let mut handle = match self.context.backend.launch(&self.binary, &self.profile) {
            Ok(handle) => handle,
            Err(error) => {
                let text = format!("Could not launch proxy: {error}");
                self.tell(Command::Launched {
                    profile: self.profile.clone(),
                    core: Core {
                        id: self.id,
                        port,
                        phase: Phase::Failed(Arc::from(text.as_str())),
                        binary: Arc::clone(&self.binary),
                        keys,
                    },
                })
                .await;
                return self.end_start(Err(AppError::new(text, 503))).await;
            }
        };
        self.tell(Command::Launched {
            profile: self.profile.clone(),
            core: Core {
                id: self.id,
                port,
                phase: Phase::Starting,
                binary: Arc::clone(&self.binary),
                keys: Arc::clone(&keys),
            },
        })
        .await;
        handle.record().await;
        match self.readiness(port, &keys, handle.as_mut()).await {
            Ok(()) => self.end_start(Ok(())).await,
            Err(Unready::Exited(status)) => {
                let text = exit_text(&status);
                let error = exited_message(&text);
                self.exited(text).await;
                return self.end_start(Err(AppError::new(error, 503))).await;
            }
            // Its core runs on until shutdown stops it.
            Err(Unready::Abandoned) => self.end_start(Err(AppError::shutting_down())).await,
            Err(Unready::Late(last)) => {
                self.tell(Command::Terminating {
                    profile: self.profile.clone(),
                    id: self.id,
                })
                .await;
                let status = handle.terminate().await;
                self.exited(exit_text(&status)).await;
                let error = format!("Proxy did not become ready. {last}");
                return self.end_start(Err(AppError::new(error, 503))).await;
            }
        }
        tokio::select! {
            status = handle.wait() => self.exited(exit_text(&status)).await,
            request = requests => {
                let status = handle.terminate().await;
                self.exited(exit_text(&status)).await;
                if let Ok(reply) = request {
                    let _ = reply.send(status.map(|_| ()));
                }
            }
        }
    }

    async fn exited(&self, text: String) {
        self.tell(Command::Exited {
            profile: self.profile.clone(),
            id: self.id,
            text,
        })
        .await;
    }

    /// The binary exists, the profile too, and its config is written for a free port.
    async fn prepare(&self) -> Result<(u16, Arc<CoreKeys>), AppError> {
        if !self.binary.exists() {
            return Err(AppError::new(
                "The bundled proxy core is missing. Run bun run core:install, then start the profile.",
                503,
            ));
        }
        let port = unused_port().map_err(|error| {
            AppError::new(format!("Could not allocate core port: {error}"), 503)
        })?;
        let config = write_config(&self.context.store, self.profile.clone(), port)
            .await
            .map_err(AppError::from)?;
        if self.context.closing() {
            return Err(AppError::shutting_down());
        }
        Ok((
            port,
            Arc::new(CoreKeys {
                management: config.management_key,
                core: config.core_key,
            }),
        ))
    }

    /// Polls the management API until the core answers, then runs the ready hook; both retry
    /// until the deadline.
    async fn readiness(
        &self,
        port: u16,
        keys: &CoreKeys,
        handle: &mut dyn CoreHandle,
    ) -> Result<(), Unready> {
        let deadline = Instant::now() + self.context.ready;
        let mut last = String::new();
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(Unready::Late(last));
            }
            let answer = management(
                self.context.backend.as_ref(),
                port,
                &keys.management,
                &Method::GET,
                "/config",
                None,
                deadline - now,
            );
            let failure = match race(handle, &self.context, answer).await? {
                Ok(_) => {
                    // An answering core has finished its exec: record it if the first read
                    // came too early.
                    handle.record().await;
                    let ready = async {
                        self.answered().await?;
                        let hooks = &self.context.hooks;
                        hooks.ready(self.profile.clone(), self.id).await
                    };
                    match race(handle, &self.context, ready).await? {
                        Ok(()) => return Ok(()),
                        Err(error) => error,
                    }
                }
                Err(error) => error,
            };
            last = failure.message;
            race(handle, &self.context, tokio::time::sleep(RETRY)).await?;
        }
    }

    /// The core answered: the owner publishes it running.
    async fn answered(&self) -> Result<(), AppError> {
        let (published, done) = oneshot::channel();
        self.tell(Command::Answered {
            profile: self.profile.clone(),
            id: self.id,
            published,
        })
        .await;
        done.await.map_err(|_| AppError::shutting_down())
    }
}

/// Runs `step` until it ends, the core exits, or shutdown abandons the start.
async fn race<T>(
    handle: &mut dyn CoreHandle,
    context: &Context,
    step: impl Future<Output = T>,
) -> Result<T, Interrupt> {
    tokio::select! {
        biased;
        status = handle.wait() => Err(Interrupt::Exited(status)),
        () = context.abandoned() => Err(Interrupt::Abandoned),
        value = step => Ok(value),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn core_ports_differ_and_come_from_below_the_ephemeral_range() {
        let range = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap();
        let ephemeral: u16 = range.split_whitespace().next().unwrap().parse().unwrap();
        let (first, second) = (unused_port().unwrap(), unused_port().unwrap());
        assert!(
            first < ephemeral && second < ephemeral,
            "{first} {second} {range}"
        );
        assert_ne!(first, second);
    }
}
