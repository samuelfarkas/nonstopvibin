//! The deadline scheduler: one task that sleeps until the earliest deadline (usage drains,
//! account refresh while a UI watches, restart backoff, idle stop, retention) and wakes early
//! only when something changes one. With no deadline it never wakes. It owns the core pool:
//! lazy profiles start on demand and stop when idle, profiles with a subscription are kept
//! running, and every core's usage queue is drained into the store. A port of the scheduling
//! half of `usage.ts` and of `cores.ts`'s launch and shutdown.
//!
//! Its state is one plain struct behind a mutex that is never held across an await. Callers,
//! the pool's hooks and the tasks it spawns change it in short sections and tell the task
//! through a [`Notify`] when a deadline moved earlier.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
use std::time::{Duration, SystemTime};

use nsv_core::pool::{
    Backend, BoxFuture, CoreEvent, CoreId, Hooks, MANAGEMENT_TIMEOUT, Pool, PoolState,
};
use nsv_core::restart::{
    Action, Due, Facts, RESTART_TIMING, RestartEvent, RestartState, RestartTiming,
};
use nsv_core::upstream;
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::registry::registry;
use nsv_domain::sync::locked;
use nsv_domain::time::epoch_ms;
use nsv_net::Method;
use nsv_store::Store;
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use crate::accounts::Accounts;
use crate::usage::{PAGE, parse_page};

/// The scheduler's periods.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Restart backoff; its `coalesce` is also the shortest wait between wakeups, so a drain
    /// follows gateway activity by about that much.
    pub restart: RestartTiming,
    /// Account status refresh for running profiles, while a UI watches.
    pub accounts: Duration,
    /// How long one UI read of the state keeps account and quota polling on.
    pub watch: Duration,
    /// Quota refresh for running profiles, while a UI watches.
    pub quotas: Duration,
    /// Longest wait between retries of a usage drain that keeps failing.
    pub usage_backoff: Duration,
    /// How long after gateway activity the queue keeps being drained.
    pub activity: Duration,
    /// How long a profile without OAuth accounts runs its core after its last use.
    pub idle: Duration,
    /// The drain period while a request or WebSocket stays open.
    pub open: Duration,
    /// Retention cleanup, running cores or not.
    pub prune: Duration,
    /// How long the first UI read after a pause waits for fresh account status.
    pub fresh: Duration,
}

/// The daemon's periods. The quota refresh and the UI's poll come from the registry.
pub static TIMING: LazyLock<Timing> = LazyLock::new(|| {
    let registry = registry();
    Timing {
        restart: RESTART_TIMING,
        accounts: Duration::from_secs(20),
        // Three of the UI's polls: a hidden or closed UI stops polling the cores.
        watch: Duration::from_millis(registry.poll_ms) * 3,
        quotas: Duration::from_millis(registry.quota_refresh_ms),
        usage_backoff: Duration::from_secs(60),
        activity: Duration::from_secs(10),
        // Agents pause for minutes between bursts; a start takes about 0.35 s.
        idle: Duration::from_secs(600),
        open: Duration::from_secs(30),
        prune: Duration::from_hours(24),
        fresh: Duration::from_secs(2),
    }
});

/// What the scheduler needs from the rest of the daemon. Called with the scheduler's state
/// locked: an implementation must not call back into the scheduler.
pub trait Host: Send + Sync + 'static {
    /// One error-list entry.
    fn report(&self, message: String);

    /// A sign-in is pending in the profile's core: its core must not stop for being idle.
    fn signing_in(&self, profile: &ProfileId) -> bool;

    /// Syncs the profile's accounts and refreshes the quota of each account that is enabled
    /// and whose quota is not unavailable, while the profile stays usable. Due every
    /// [`Timing::quotas`] while a UI watches, and after each automatic start. Dropping the
    /// future abandons the check: nothing is stored after.
    ///
    /// # Errors
    /// What the user sees in the error list.
    fn refresh_quotas(
        &self,
        pool: Pool,
        profile: ProfileId,
    ) -> BoxFuture<'static, Result<(), AppError>>;
}

/// When a profile's queue is drained next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Next {
    Now,
    At(Instant),
    Never,
}

impl Next {
    fn due(self, now: Instant) -> bool {
        match self {
            Self::Now => true,
            Self::At(at) => at <= now,
            Self::Never => false,
        }
    }
}

/// One core's drain cadence: a new core's starts over, a readiness retry of the same keeps it.
#[derive(Debug)]
struct Cadence {
    core: CoreId,
    next: Next,
    active_until: Option<Instant>,
    failures: u32,
}

type Collection = watch::Receiver<Option<Result<usize, AppError>>>;

/// What the scheduler knows about one profile.
#[derive(Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts of usage.ts's per-profile sets"
)]
struct Track {
    cadence: Option<Cadence>,
    restart: Option<RestartState>,
    /// When the restart policy wants to hear the time.
    wake: Option<Due>,
    /// What the profile shows once the scheduler stopped starting it.
    shown: Option<String>,
    /// No OAuth account: its core is stopped until a request or a UI needs it.
    dormant: bool,
    /// When a gateway request last started or ended, or its core last started.
    last_used: Option<Instant>,
    /// An idle stop is in flight.
    sleeping: bool,
    /// The quota check after an automatic start is in flight.
    checking: bool,
    /// Gateway requests or `WebSocket` connections are open; their usage arrives later.
    open: bool,
    /// The latest outcome per key (auth index) and model from the current core.
    outcomes: HashMap<(String, String), (u16, Instant)>,
    /// The usage queue read in flight; the queue is destructive, so readers share it.
    collecting: Option<Collection>,
}

/// A profile as the store last showed it.
#[derive(Debug)]
struct Fact {
    id: ProfileId,
    name: String,
    enabled: bool,
    api: usize,
}

#[derive(Debug, Default)]
struct State {
    begun: bool,
    scheduled: Option<Instant>,
    watched_until: Option<Instant>,
    next_accounts: Option<Instant>,
    next_quotas: Option<Instant>,
    next_prune: Option<Instant>,
    /// A drain pass is in flight.
    polling: bool,
    /// An account, quota or retention pass is in flight.
    maintenance: bool,
    /// Profiles in creation order.
    facts: Vec<Fact>,
    tracks: BTreeMap<ProfileId, Track>,
}

impl State {
    fn watched(&self, now: Instant) -> bool {
        self.watched_until.is_some_and(|until| now < until)
    }

    fn track(&mut self, profile: &ProfileId) -> &mut Track {
        self.tracks.entry(profile.clone()).or_default()
    }

    fn fact(&self, profile: &ProfileId) -> Option<&Fact> {
        self.facts.iter().find(|fact| fact.id == *profile)
    }
}

struct Shared {
    state: Mutex<State>,
    notify: Notify,
    timing: Timing,
    epoch: Instant,
    store: Store,
    pool: Pool,
    accounts: Arc<Accounts>,
    host: Arc<dyn Host>,
    /// Drain passes, maintenance, automatic starts and idle stops in flight.
    work: watch::Sender<usize>,
}

/// A handle to the scheduler and the pool it drives.
#[derive(Clone)]
pub struct Scheduler(Arc<Shared>);

impl Scheduler {
    /// The scheduler and its pool, whose cores start from `binary` through `backend` and get
    /// `ready` to answer. Nothing is scheduled before [`Scheduler::launch`].
    #[must_use]
    pub fn new(
        store: Store,
        binary: PathBuf,
        backend: Arc<dyn Backend>,
        ready: Duration,
        accounts: Arc<Accounts>,
        host: Arc<dyn Host>,
        timing: Timing,
    ) -> Self {
        Self(Arc::new_cyclic(|shared| {
            let hooks = Arc::new(PoolHooks(Weak::clone(shared)));
            Shared {
                state: Mutex::new(State::default()),
                notify: Notify::new(),
                timing,
                epoch: Instant::now(),
                pool: Pool::new(store.clone(), binary, backend, ready, hooks),
                store,
                accounts,
                host,
                work: watch::Sender::new(0),
            }
        }))
    }

    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.0.pool
    }

    /// At launch, once orphaned cores are reaped: enabled profiles without an OAuth account
    /// stay dormant until a request needs them, the others start one after another (a failure
    /// is reported, not returned), then scheduling begins.
    ///
    /// # Errors
    /// The store could not be read, or shutdown began.
    pub async fn launch(&self) -> Result<(), AppError> {
        let facts = self.0.facts().await?;
        let eager: Vec<(ProfileId, String)> = {
            let mut state = self.0.lock();
            state.facts = facts;
            let enabled: Vec<(ProfileId, String)> = state
                .facts
                .iter()
                .filter(|fact| fact.enabled)
                .map(|fact| (fact.id.clone(), fact.name.clone()))
                .collect();
            enabled
                .into_iter()
                .filter(|(profile, _)| {
                    // A core already running (started before launch) is no longer dormant.
                    let dormant = self.0.lazy(profile) && !self.0.pool.alive(profile);
                    state.track(profile).dormant = dormant;
                    !dormant
                })
                .collect()
        };
        let mut reported = BTreeSet::new();
        for (profile, name) in eager {
            if let Err(error) = self.0.pool.start(&profile, false).await {
                if self.0.pool.closing() {
                    return Err(error);
                }
                self.0.host.report(format!("{name}: {}", error.message));
                reported.insert(profile);
            }
        }
        self.0.begin(&reported);
        Ok(())
    }

    /// Profiles or subscriptions may have changed: whether each profile is kept running is
    /// read again.
    ///
    /// # Errors
    /// The store could not be read.
    pub async fn changed(&self) -> Result<(), AppError> {
        let facts = self.0.facts().await?;
        let mut state = self.0.lock();
        state.facts = facts;
        let pool = self.0.pool.state();
        let now = Instant::now();
        let profiles: BTreeSet<ProfileId> = state
            .facts
            .iter()
            .map(|fact| fact.id.clone())
            .chain(state.tracks.keys().cloned())
            .collect();
        for profile in profiles {
            self.0.hear(&mut state, &pool, &profile, None, false, now);
        }
        self.0.schedule(&mut state, &pool, now);
        Ok(())
    }

    /// Gateway activity on the profile: a request opened or closed, and whether any is still
    /// open. Its queue is drained shortly after, and once more after the activity window.
    pub fn activity(&self, profile: &ProfileId, open: bool) {
        let mut state = self.0.lock();
        let now = Instant::now();
        let activity = self.0.timing.activity;
        let track = state.track(profile);
        track.open = open;
        track.last_used = Some(now);
        if let Some(cadence) = &mut track.cadence {
            cadence.active_until = Some(now + activity);
            cadence.next = Next::Now;
        }
        let pool = self.0.pool.state();
        self.0.schedule(&mut state, &pool, now);
    }

    /// Starts a dormant profile's core and waits for it; requests meanwhile share the start.
    /// The restart policy keeps it running from then on, so a failed start is retried as any
    /// other. Ends at once for a profile that is not dormant. The start runs to its end even if
    /// the caller stops waiting.
    ///
    /// # Errors
    /// The start's error.
    pub async fn wake(&self, profile: &ProfileId) -> Result<(), AppError> {
        if !self.is_dormant(profile) {
            return Ok(());
        }
        let shared = Arc::clone(&self.0);
        let profile = profile.clone();
        tokio::spawn(async move {
            let result = shared.pool.start(&profile, false).await;
            // A core that runs ended dormancy already; after a failed start, the policy retries.
            let mut state = shared.lock();
            let track = state.track(&profile);
            if track.dormant {
                track.dormant = false;
                let pool = shared.pool.state();
                let now = Instant::now();
                shared.hear(&mut state, &pool, &profile, None, false, now);
                shared.schedule(&mut state, &pool, now);
            }
            result
        })
        .await
        .map_err(AppError::internal)?
    }

    /// A UI read the state. Dormant cores start, so it shows them running. Account status and
    /// quota are polled only while one keeps reading, and the first read after a pause waits
    /// briefly for every running profile's account status.
    pub async fn watch(&self) {
        let (dormant, usable) = {
            let mut state = self.0.lock();
            let now = Instant::now();
            let resumed = !state.watched(now);
            state.watched_until = Some(now + self.0.timing.watch);
            if !state.begun {
                return;
            }
            let dormant: Vec<ProfileId> = state
                .tracks
                .iter()
                .filter(|(_, track)| track.dormant)
                .map(|(profile, _)| profile.clone())
                .collect();
            let pool = self.0.pool.state();
            let mut usable = Vec::new();
            if resumed {
                // Quota older than its period refreshes in the next maintenance pass.
                let quotas_due = state.next_quotas.is_some_and(|at| now >= at);
                state.next_accounts = Some(if quotas_due {
                    now
                } else {
                    now + self.0.timing.accounts
                });
                usable = pool
                    .profiles
                    .keys()
                    .filter(|profile| pool.usable(profile))
                    .cloned()
                    .collect();
            }
            self.0.schedule(&mut state, &pool, now);
            (dormant, usable)
        };
        let mut waits = Vec::new();
        for profile in dormant {
            let scheduler = self.clone();
            // A start that fails shows in the profile's state, which this read returns.
            waits.push(tokio::spawn(async move {
                let _ = scheduler.wake(&profile).await;
            }));
        }
        let mut syncs = Vec::new();
        for profile in usable {
            let shared = Arc::clone(&self.0);
            syncs.push(tokio::spawn(async move {
                if let Err(error) = shared.accounts.sync(&shared.pool, &profile).await {
                    let state = shared.lock();
                    shared.report_for(&state, &profile, &error.message);
                }
            }));
        }
        let all = async {
            for wait in waits.iter_mut().chain(syncs.iter_mut()) {
                // A task's failure is its own; each reports what it must.
                let _ = wait.await;
            }
        };
        let _ = tokio::time::timeout(self.0.timing.fresh, all).await;
        // A core slower than the wait answers the next poll instead.
        for sync in syncs {
            sync.abort();
        }
    }

    #[must_use]
    pub fn is_dormant(&self, profile: &ProfileId) -> bool {
        self.0
            .lock()
            .tracks
            .get(profile)
            .is_some_and(|track| track.dormant)
    }

    /// What a profile shows once the scheduler stopped starting it.
    #[must_use]
    pub fn gave_up(&self, profile: &ProfileId) -> Option<String> {
        self.0
            .lock()
            .tracks
            .get(profile)
            .and_then(|track| track.shown.clone())
    }

    /// When the scheduler next wakes; `None` while nothing is due.
    #[must_use]
    pub fn wakes_at(&self) -> Option<Instant> {
        self.0.lock().scheduled
    }

    /// From now on nothing starts, no request reaches a core, a start still waiting for its
    /// core gives up, and nothing more is scheduled.
    pub async fn begin_shutdown(&self) {
        // An error means the pool is gone, which is the state asked for.
        let _ = self.0.pool.begin_shutdown().await;
        self.0.notify.notify_one();
    }

    /// Waits for the drain pass, maintenance, automatic starts and idle stops in flight.
    pub async fn settled(&self) {
        let mut work = self.0.work.subscribe();
        // The sender lives in `self`, so the wait ends only when the work does.
        let _ = work.wait_for(|count| *count == 0).await;
    }

    /// Shuts the pool down after the scheduler's work: each core's usage is drained into the
    /// store before it is terminated. Close the store only after this.
    ///
    /// # Errors
    /// See [`Pool::stop_all`].
    pub async fn shutdown(&self) -> Result<(), AppError> {
        self.begin_shutdown().await;
        self.settled().await;
        self.0.pool.stop_all().await
    }
}

/// Counts one task in [`Shared::work`] until it ends, even by a panic.
struct Working(Arc<Shared>);

impl Drop for Working {
    fn drop(&mut self) {
        self.0.work.send_modify(|count| *count -= 1);
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        locked(&self.state)
    }

    /// Milliseconds since the scheduler began, the restart policy's clock.
    fn ms(&self, at: Instant) -> i64 {
        i64::try_from(at.saturating_duration_since(self.epoch).as_millis()).unwrap_or(i64::MAX)
    }

    fn instant(&self, ms: i64) -> Instant {
        self.epoch + Duration::from_millis(u64::try_from(ms).unwrap_or(0))
    }

    fn coalesce(&self) -> Duration {
        Duration::from_millis(u64::try_from(self.timing.restart.coalesce).unwrap_or(0))
    }

    /// No OAuth account: its core starts on demand and stops when idle. OAuth ones must keep
    /// refreshing tokens.
    fn lazy(&self, profile: &ProfileId) -> bool {
        self.accounts.oauth_count(profile).unwrap_or(0) == 0
    }

    /// The profiles in creation order, with each one's API-key accounts; the OAuth account
    /// counts go to [`Accounts`].
    async fn facts(&self) -> Result<Vec<Fact>, AppError> {
        let loaded = self
            .store
            .call(|db| {
                let mut facts = Vec::new();
                for profile in db.profiles()? {
                    let oauth = db.account_metadata(&profile.id)?.len();
                    let api = db.api_accounts(&profile.id)?.len();
                    facts.push((
                        Fact {
                            id: profile.id,
                            name: profile.name,
                            enabled: profile.enabled,
                            api,
                        },
                        oauth,
                    ));
                }
                Ok(facts)
            })
            .await
            .map_err(AppError::from)?;
        Ok(loaded
            .into_iter()
            .map(|(fact, oauth)| {
                self.accounts.set_oauth_count(&fact.id, oauth);
                fact
            })
            .collect())
    }

    fn spawn(self: &Arc<Self>, work: impl Future<Output = ()> + Send + 'static) {
        self.work.send_modify(|count| *count += 1);
        let working = Working(Arc::clone(self));
        tokio::spawn(async move {
            work.await;
            drop(working);
        });
    }

    fn report_for(&self, state: &State, profile: &ProfileId, message: &str) {
        if let Some(fact) = state.fact(profile) {
            self.host.report(format!("{}: {message}", fact.name));
        }
    }

    /// Starts scheduling; `reported`: profiles whose failed start the launch reported.
    fn begin(self: &Arc<Self>, reported: &BTreeSet<ProfileId>) {
        let mut state = self.lock();
        let now = Instant::now();
        state.begun = true;
        state.next_accounts = Some(now + self.timing.accounts);
        state.next_quotas = Some(now + self.timing.quotas);
        state.next_prune = Some(now + self.timing.prune);
        let pool = self.pool.state();
        let profiles: Vec<ProfileId> = state.facts.iter().map(|fact| fact.id.clone()).collect();
        for profile in profiles {
            let launched = reported.contains(&profile);
            self.hear(&mut state, &pool, &profile, None, launched, now);
        }
        self.schedule(&mut state, &pool, now);
        drop(state);
        tokio::spawn(Arc::clone(self).run());
    }

    async fn run(self: Arc<Self>) {
        loop {
            let notified = self.notify.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            if self.pool.closing() {
                return;
            }
            let scheduled = self.lock().scheduled;
            match scheduled {
                Some(at) => tokio::select! {
                    () = tokio::time::sleep_until(at) => self.tick(),
                    () = notified => {}
                },
                None => notified.await,
            }
        }
    }

    /// Gives the profile's restart policy `event`, after whether the profile is kept running if
    /// that changed. `reported`: a profile seen first, whose failure the launch reported.
    fn hear(
        self: &Arc<Self>,
        state: &mut State,
        pool: &PoolState,
        profile: &ProfileId,
        event: Option<RestartEvent>,
        reported: bool,
        now: Instant,
    ) {
        // Before `begin`, the launch is starting profiles; `begin` looks once it is done.
        if !state.begun || pool.closing {
            return;
        }
        let kept = state
            .fact(profile)
            .is_some_and(|fact| fact.api + self.accounts.oauth_count(profile).unwrap_or(0) > 0)
            && !state.track(profile).dormant;
        let known = state.track(profile).restart;
        let mut restart = known.unwrap_or_else(|| {
            RestartState::new(Facts {
                running: pool.running(profile),
                reported,
                kept,
                starting: pool.starting(profile),
                holding: pool.holding(profile),
            })
        });
        if known.is_none() || restart.kept != kept {
            restart = self.act(state, profile, restart, RestartEvent::Kept(kept), now);
        }
        if let Some(event) = event {
            self.act(state, profile, restart, event, now);
        }
    }

    /// One transition, carried out: its state is in place before a start can tell anything back.
    fn act(
        self: &Arc<Self>,
        state: &mut State,
        profile: &ProfileId,
        restart: RestartState,
        event: RestartEvent,
        now: Instant,
    ) -> RestartState {
        let step = restart.transition(event, self.ms(now), &self.timing.restart);
        let track = state.track(profile);
        track.restart = Some(step.state);
        track.wake = None;
        let mut reports = Vec::new();
        let mut start = false;
        for action in step.actions {
            match action {
                Action::Start => start = true,
                Action::Wake(due) => track.wake = Some(due),
                Action::Report(text) => reports.push(text),
                Action::Show(text) => track.shown = text,
            }
        }
        for text in reports {
            self.report_for(state, profile, &text);
        }
        if start {
            self.restart(profile.clone());
        }
        step.state
    }

    /// Starts one profile apart from maintenance and other profiles' starts, then checks its
    /// quota.
    fn restart(self: &Arc<Self>, profile: ProfileId) {
        let shared = Arc::clone(self);
        self.spawn(async move {
            if let Err(error) = shared.pool.start(&profile, true).await {
                // The pool tells how a start it began ended; this tells of one it refused.
                let mut state = shared.lock();
                let pool = shared.pool.state();
                let now = Instant::now();
                let event = RestartEvent::Started {
                    automatic: true,
                    running: false,
                    error: Some(error.message),
                };
                shared.hear(&mut state, &pool, &profile, Some(event), false, now);
                shared.schedule(&mut state, &pool, now);
                return;
            }
            // Restart decisions never wait for this check: it is not part of the policy.
            {
                let mut state = shared.lock();
                let track = state.track(&profile);
                if track.checking {
                    return;
                }
                track.checking = true;
            }
            // It missed its quota checks while down; shutdown abandons the one in flight, and
            // an answer that comes as shutdown begins is not stored either.
            let mut closing = shared.pool.subscribe();
            let checked = tokio::select! {
                biased;
                _ = closing.wait_for(|pool| pool.closing) => None,
                checked = shared.host.refresh_quotas(shared.pool.clone(), profile.clone()) => Some(checked),
            };
            let mut state = shared.lock();
            if let Some(Err(error)) = checked
                && !shared.pool.closing()
            {
                shared.report_for(&state, &profile, &error.message);
            }
            state.track(&profile).checking = false;
        });
    }

    /// When a running lazy profile becomes idle enough to stop; `None` while anything uses it.
    fn idle_at(&self, state: &State, pool: &PoolState, profile: &ProfileId) -> Option<Instant> {
        let track = state.tracks.get(profile);
        if !pool.usable(profile)
            || !self.lazy(profile)
            || track.is_some_and(|track| track.sleeping || track.open)
            || pool.starting(profile)
            || pool.holding(profile)
            || self.host.signing_in(profile)
        {
            return None;
        }
        let used = track
            .and_then(|track| track.last_used)
            .unwrap_or(self.epoch);
        let watched = state.watched_until.unwrap_or(self.epoch);
        Some(used.max(watched) + self.timing.idle)
    }

    /// Moves the next wakeup earlier if something is due before it, never sooner than the
    /// coalescing wait from now.
    fn schedule(&self, state: &mut State, pool: &PoolState, now: Instant) {
        if pool.closing {
            return;
        }
        let running: Vec<&ProfileId> = pool
            .profiles
            .keys()
            .filter(|profile| pool.running(profile))
            .collect();
        let mut due: Option<Instant> = None;
        let mut consider = |at: Instant| due = Some(due.map_or(at, |due| due.min(at)));
        if !state.maintenance {
            // Daily, even with nothing running: retention deletes rows by age.
            if let Some(at) = state.next_prune {
                consider(at);
            }
            // Account polling only while a core runs and a UI watches: nothing else reads them.
            if let Some(at) = state
                .next_accounts
                .filter(|_| !running.is_empty() && state.watched(now))
            {
                consider(at);
            }
        }
        // Only what the restart policy asked for: nothing for a profile that cannot start.
        for track in state.tracks.values() {
            match track.wake {
                Some(Due::Now) => consider(now),
                Some(Due::At(ms)) => consider(self.instant(ms)),
                None => {}
            }
        }
        for profile in pool.profiles.keys() {
            if let Some(at) = self.idle_at(state, pool, profile) {
                consider(at);
            }
        }
        if !state.polling {
            for profile in running {
                match state
                    .tracks
                    .get(profile)
                    .and_then(|track| track.cadence.as_ref())
                {
                    Some(Cadence {
                        next: Next::Now, ..
                    }) => consider(now),
                    Some(Cadence {
                        next: Next::At(at), ..
                    }) => consider(*at),
                    _ => {}
                }
            }
        }
        let Some(due) = due else {
            return;
        };
        let at = due.max(now + self.coalesce());
        if state.scheduled.is_none_or(|scheduled| at < scheduled) {
            state.scheduled = Some(at);
            self.notify.notify_one();
        }
    }

    fn tick(self: &Arc<Self>) {
        let mut state = self.lock();
        let pool = self.pool.state();
        let now = Instant::now();
        state.scheduled = None;
        if pool.closing {
            return;
        }
        if !state.polling {
            state.polling = true;
            self.spawn(Arc::clone(self).drain_pass());
        }
        let accounts_due = state.watched(now) && state.next_accounts.is_some_and(|at| at <= now);
        let prune_due = state.next_prune.is_some_and(|at| at <= now);
        if !state.maintenance && (accounts_due || prune_due) {
            let quotas_due = accounts_due && state.next_quotas.is_some_and(|at| at <= now);
            if accounts_due {
                state.next_accounts = Some(now + self.timing.accounts);
            }
            if quotas_due {
                state.next_quotas = Some(now + self.timing.quotas);
            }
            if prune_due {
                state.next_prune = Some(now + self.timing.prune);
            }
            state.maintenance = true;
            self.spawn(Arc::clone(self).maintenance(prune_due, accounts_due, quotas_due));
        }
        // After the drain pass and maintenance are under way, so a start's events see them.
        let ms = self.ms(now);
        let woken: Vec<ProfileId> = state
            .tracks
            .iter()
            .filter(|(_, track)| track.wake.is_some_and(|due| due.by(ms)))
            .map(|(profile, _)| profile.clone())
            .collect();
        for profile in woken {
            self.hear(
                &mut state,
                &pool,
                &profile,
                Some(RestartEvent::Time),
                false,
                now,
            );
        }
        let idle: Vec<ProfileId> = pool
            .profiles
            .keys()
            .filter(|profile| {
                self.idle_at(&state, &pool, profile)
                    .is_some_and(|at| at <= now)
            })
            .cloned()
            .collect();
        for profile in idle {
            state.track(&profile).sleeping = true;
            self.spawn(Arc::clone(self).idle_stop(profile));
        }
        self.schedule(&mut state, &pool, now);
    }

    /// Drains each usable core whose cadence is due, one after another.
    async fn drain_pass(self: Arc<Self>) {
        let profiles: Vec<ProfileId> = self.pool.state().profiles.keys().cloned().collect();
        for profile in profiles {
            let core = {
                let mut state = self.lock();
                let pool = self.pool.state();
                let now = Instant::now();
                if pool.closing {
                    break;
                }
                let open_period = self.timing.open;
                let track = state.track(&profile);
                let open = track.open;
                // A stopping profile's own stop drains its queue until a short page.
                let Some(cadence) = track
                    .cadence
                    .as_mut()
                    .filter(|cadence| pool.usable(&profile) && cadence.next.due(now))
                else {
                    continue;
                };
                // Usage only arises from gateway traffic, which marks activity. Once it has
                // been quiet for the whole window, this drain was the trailing one: sleep until
                // new activity or a stop (which drains). Open requests keep a slow cadence:
                // their usage lands as each response completes. Set before reading, so
                // activity meanwhile can bring it forward.
                cadence.next = if open {
                    Next::At(now + open_period)
                } else {
                    Next::Never
                };
                cadence.core
            };
            let result = self.collect(&profile).await;
            let mut state = self.lock();
            let now = Instant::now();
            let (coalesce, backoff) = (self.coalesce(), self.timing.usage_backoff);
            if let Some(cadence) = state
                .track(&profile)
                .cadence
                .as_mut()
                .filter(|cadence| cadence.core == core)
            {
                if let Ok(count) = &result {
                    cadence.failures = 0;
                    if *count == PAGE {
                        cadence.next = Next::Now;
                    } else if let Some(until) = cadence.active_until
                        && cadence.next != Next::Now
                        && now < until
                    {
                        // Once more when requests have been quiet for the whole window: the
                        // core can record a request's usage just after its response ends.
                        cadence.next = cadence.next.min(Next::At(until));
                    }
                } else {
                    cadence.failures += 1;
                    cadence.next = if cadence.failures == 1 {
                        Next::Now
                    } else {
                        let factor = 2_u32.saturating_pow(cadence.failures - 1);
                        Next::At(now + coalesce.saturating_mul(factor).min(backoff))
                    };
                }
            }
            if let Err(error) = result {
                self.report_for(&state, &profile, &error.message);
            }
        }
        let mut state = self.lock();
        state.polling = false;
        let pool = self.pool.state();
        self.schedule(&mut state, &pool, Instant::now());
    }

    /// Retention cleanup, then account status or quota of every usable profile.
    async fn maintenance(self: Arc<Self>, prune: bool, accounts: bool, quotas: bool) {
        if prune
            && let Err(error) = self
                .store
                .call(|db| db.prune_usage(SystemTime::now()))
                .await
        {
            self.host
                .report(format!("History cleanup: {}", AppError::from(error)));
        }
        if accounts {
            let profiles: Vec<ProfileId> = self.pool.state().profiles.keys().cloned().collect();
            for profile in profiles {
                let pool = self.pool.state();
                if pool.closing {
                    break;
                }
                if !pool.usable(&profile) {
                    continue;
                }
                let result = if quotas {
                    // Each account is checked as its turn comes; a stop or shutdown ends it.
                    self.host
                        .refresh_quotas(self.pool.clone(), profile.clone())
                        .await
                } else {
                    self.accounts.sync(&self.pool, &profile).await.map(|_| ())
                };
                if let Err(error) = result {
                    let state = self.lock();
                    self.report_for(&state, &profile, &error.message);
                }
            }
        }
        let mut state = self.lock();
        state.maintenance = false;
        let pool = self.pool.state();
        self.schedule(&mut state, &pool, Instant::now());
    }

    /// Stops an idle lazy profile's core. Its usage queue is drained first, and a failed read
    /// keeps the core running: stopping would lose that usage. A request that began meanwhile
    /// keeps it running too, and one that comes after the decision waits for the next core.
    async fn idle_stop(self: Arc<Self>, profile: ProfileId) {
        let stopped = self.sleep(&profile).await;
        let mut state = self.lock();
        state.track(&profile).sleeping = false;
        if let Err(error) = stopped {
            self.report_for(&state, &profile, &error.message);
        }
        // A stop that was held back is due again an idle period from now.
        let pool = self.pool.state();
        self.schedule(&mut state, &pool, Instant::now());
    }

    async fn sleep(self: &Arc<Self>, profile: &ProfileId) -> Result<(), AppError> {
        if let Err(error) = self.drain(profile).await {
            let mut state = self.lock();
            let message = format!("Usage before an idle stop: {}", error.message);
            self.report_for(&state, profile, &message);
            // The next attempt waits another idle period instead of every wakeup.
            state.track(profile).last_used = Some(Instant::now());
            return Ok(());
        }
        // Reserved first, so the decision and the stop's place in the pool's queue happen
        // with nothing in between: a request after it waits for the next core.
        let permit = self.pool.reserve().await?;
        let stop = {
            let mut state = self.lock();
            let pool = self.pool.state();
            let now = Instant::now();
            let track = state.track(profile);
            // A new core would send the next request to a key or model this one cools down.
            let cooling = track.outcomes.values().any(|(status, at)| {
                upstream::cooldown(*status).is_some_and(|period| now < *at + period)
            });
            if cooling {
                track.last_used = Some(now);
                return Ok(());
            }
            track.sleeping = false;
            if self
                .idle_at(&state, &pool, profile)
                .is_none_or(|at| at > now)
            {
                return Ok(());
            }
            state.track(profile).dormant = true;
            self.hear(&mut state, &pool, profile, None, false, now);
            permit.stop(profile)
        };
        stop.await
    }

    /// Reads the profile's queue until a page is not full.
    async fn drain(self: &Arc<Self>, profile: &ProfileId) -> Result<(), AppError> {
        // Full destructive pages must be stored before the core ends.
        while self.collect(profile).await? == PAGE {}
        Ok(())
    }

    /// Reads one page of the queue into the store. Concurrent reads share one, which runs to
    /// its end even if every reader stops waiting: the queue is destructive.
    async fn collect(self: &Arc<Self>, profile: &ProfileId) -> Result<usize, AppError> {
        let mut pending = {
            let mut state = self.lock();
            let track = state.track(profile);
            if let Some(pending) = &track.collecting {
                pending.clone()
            } else {
                let (done, pending) = watch::channel(None);
                track.collecting = Some(pending.clone());
                let shared = Arc::clone(self);
                let profile = profile.clone();
                tokio::spawn(async move {
                    let result = shared.read_page(&profile).await;
                    shared.lock().track(&profile).collecting = None;
                    done.send_replace(Some(result));
                });
                pending
            }
        };
        let ended = || AppError::internal("The usage queue read ended without an answer.");
        let answer = pending
            .wait_for(Option::is_some)
            .await
            .map_err(|_| ended())?
            .clone();
        answer.unwrap_or_else(|| Err(ended()))
    }

    async fn read_page(&self, profile: &ProfileId) -> Result<usize, AppError> {
        let page = self
            .pool
            .management(
                profile,
                &Method::GET,
                &format!("/usage-queue?count={PAGE}"),
                None,
                MANAGEMENT_TIMEOUT,
            )
            .await?;
        let records = parse_page(profile, &page, epoch_ms(SystemTime::now()))?;
        {
            let mut state = self.lock();
            let now = Instant::now();
            let outcomes = &mut state.track(profile).outcomes;
            // The queue is in completion order, so a later record is the later outcome.
            for record in &records {
                outcomes.insert(
                    (record.account.clone(), record.model.clone()),
                    (record.status_code, now),
                );
            }
        }
        let count = records.len();
        self.store
            .call(move |db| db.add_usage(&records))
            .await
            .map_err(AppError::from)?;
        Ok(count)
    }
}

/// The scheduler's side of the pool: it hears every core event, starts each new core's drain
/// cadence and syncs its accounts once it answers, and drains a core's usage before it stops.
struct PoolHooks(Weak<Shared>);

impl Hooks for PoolHooks {
    fn event(&self, profile: &ProfileId, event: CoreEvent) {
        let Some(shared) = self.0.upgrade() else {
            return;
        };
        let mut state = shared.lock();
        let pool = shared.pool.state();
        let now = Instant::now();
        // Whoever started it, a running core is no longer dormant, its idle time starts now,
        // and it holds no cooldowns.
        if let CoreEvent::Started { running: true, .. } = event {
            let track = state.track(profile);
            track.dormant = false;
            track.last_used = Some(now);
            track.outcomes.clear();
        }
        shared.hear(&mut state, &pool, profile, Some(event.into()), false, now);
        shared.schedule(&mut state, &pool, now);
    }

    fn ready(
        &self,
        pool: Pool,
        profile: ProfileId,
        core: CoreId,
    ) -> BoxFuture<'static, Result<(), AppError>> {
        let shared = self.0.upgrade();
        Box::pin(async move {
            let Some(shared) = shared else {
                return Ok(());
            };
            {
                let mut state = shared.lock();
                let track = state.track(&profile);
                if track
                    .cadence
                    .as_ref()
                    .is_none_or(|cadence| cadence.core != core)
                {
                    track.cadence = Some(Cadence {
                        core,
                        next: Next::Now,
                        active_until: None,
                        failures: 0,
                    });
                }
                let snapshot = shared.pool.state();
                shared.schedule(&mut state, &snapshot, Instant::now());
            }
            shared.accounts.sync(&pool, &profile).await.map(|_| ())
        })
    }

    fn before_stop(&self, _: Pool, profile: ProfileId) -> BoxFuture<'static, ()> {
        let shared = self.0.upgrade();
        Box::pin(async move {
            let Some(shared) = shared else {
                return;
            };
            if let Err(error) = shared.drain(&profile).await {
                shared
                    .host
                    .report(format!("Usage before stop: {}", error.message));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_periods_keep_their_values() {
        // Three of the UI's 4 s polls, and the quota refresh of `quotaRefreshMs`.
        assert_eq!(TIMING.watch, Duration::from_secs(12));
        assert_eq!(TIMING.quotas, Duration::from_secs(120));
    }
}
