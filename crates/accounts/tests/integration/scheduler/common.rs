//! A daemon's store, accounts, scheduler and pool on fake cores, under a clock that moves only
//! when a test moves it.

use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nsv_accounts::accounts::Accounts;
use nsv_accounts::scheduler::{Host, Scheduler, TIMING, Timing};
use nsv_core::pool::{BoxFuture, Pool};
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_testkit::paused::Paused;
use tokio::sync::watch;

/// The host: reports are kept, quota checks recorded and answered as a test says.
#[derive(Default)]
pub(crate) struct TestHost {
    pub(crate) reports: Mutex<Vec<String>>,
    pub(crate) signing_in: Mutex<Option<ProfileId>>,
    pub(crate) quota_checks: Mutex<Vec<ProfileId>>,
    /// While set, quota checks wait for it to turn true.
    pub(crate) quota_gate: Mutex<Option<watch::Receiver<bool>>>,
    /// Quota checks answer when shutdown begins, in the wakeup that tells of it.
    pub(crate) quota_at_shutdown: AtomicBool,
    /// Quota checks answer this error.
    pub(crate) quota_error: Mutex<Option<String>>,
    /// Quota checks that ran to their end and stored.
    pub(crate) quota_stored: Arc<Mutex<Vec<ProfileId>>>,
}

impl Host for TestHost {
    fn report(&self, message: String) {
        self.reports.lock().unwrap().push(message);
    }

    fn now(&self) -> i64 {
        nsv_domain::time::epoch_ms(std::time::SystemTime::now())
    }

    fn signing_in(&self, profile: &ProfileId) -> bool {
        self.signing_in.lock().unwrap().as_ref() == Some(profile)
    }

    fn refresh_quotas(
        &self,
        pool: Pool,
        profile: ProfileId,
    ) -> BoxFuture<'static, Result<(), AppError>> {
        self.quota_checks.lock().unwrap().push(profile.clone());
        let gate = self.quota_gate.lock().unwrap().clone();
        let shutdown = self
            .quota_at_shutdown
            .load(Ordering::SeqCst)
            .then(|| pool.subscribe());
        let error = self.quota_error.lock().unwrap().clone();
        let stored = Arc::clone(&self.quota_stored);
        Box::pin(async move {
            if let Some(mut gate) = gate {
                let _ = gate.wait_for(|open| *open).await;
            }
            if let Some(mut pool) = shutdown {
                let _ = pool.wait_for(|pool| pool.closing).await;
            }
            if let Some(error) = error {
                return Err(AppError::new(error, 502));
            }
            stored.lock().unwrap().push(profile);
            Ok(())
        })
    }
}

/// Short periods, so sequences take few steps of the paused clock.
pub(crate) fn short(idle: Duration) -> Timing {
    Timing {
        restart: nsv_accounts::restart::RestartTiming {
            coalesce: 200,
            ..TIMING.restart
        },
        usage_retry: Duration::from_millis(200),
        activity: Duration::from_secs(1),
        idle,
        ..*TIMING
    }
}

/// The scheduler and its pool over [`Paused`]'s store and fake cores.
pub(crate) struct World {
    paused: Paused,
    pub(crate) host: Arc<TestHost>,
    pub(crate) accounts: Arc<Accounts>,
    pub(crate) scheduler: Scheduler,
}

impl Deref for World {
    type Target = Paused;

    fn deref(&self) -> &Paused {
        &self.paused
    }
}

impl World {
    /// Must run on a runtime with a paused clock (`start_paused`); see [`Paused::new`].
    pub(crate) async fn new(timing: Timing) -> Self {
        Self::with_ready(timing, nsv_core::pool::READY).await
    }

    pub(crate) async fn with_ready(timing: Timing, ready: Duration) -> Self {
        let paused = Paused::new().await;
        let host = Arc::new(TestHost::default());
        let shared: Arc<dyn Host> = Arc::<TestHost>::clone(&host);
        let accounts = Arc::new(Accounts::new(paused.store.clone()));
        let (scheduler, ()) = Scheduler::new(
            paused.store.clone(),
            paused.binary.clone(),
            Arc::new(paused.backend()),
            ready,
            Arc::clone(&accounts),
            timing,
            |_| (shared, ()),
        );
        Self {
            paused,
            host,
            accounts,
            scheduler,
        }
    }

    pub(crate) fn pool(&self) -> &Pool {
        self.scheduler.pool()
    }

    /// Asserts the only wakeup ahead is the daily history cleanup, about a day away.
    #[track_caller]
    pub(crate) fn assert_only_cleanup_due(&self, why: &str) {
        let left = self
            .scheduler
            .wakes_at()
            .map(|at| at.duration_since(tokio::time::Instant::now()));
        assert!(
            left.is_some_and(
                |left| left > Duration::from_hours(23) && left <= Duration::from_hours(24)
            ),
            "{why}: the next wakeup is in {left:?}"
        );
    }

    /// Moves the clock until `condition` holds; panics after `limit`.
    pub(crate) async fn until(
        &self,
        what: &str,
        limit: Duration,
        condition: impl Fn(&Self) -> bool,
    ) {
        self.paused.until(what, limit, || condition(self)).await;
    }

    /// A gateway request through the scheduler: counted in, its dormant core woken, served by
    /// the core on the profile's port, counted out.
    pub(crate) async fn chat(&self, profile: &ProfileId, model: &str) -> u16 {
        self.scheduler.activity(profile, true);
        let woken = self.run(self.scheduler.wake(profile)).await;
        let status = match woken.and_then(|()| self.pool().port(profile)) {
            Ok(port) => self.fake.request(port, model),
            Err(error) => error.status,
        };
        self.scheduler.activity(profile, false);
        status
    }

    /// Usage rows stored for the profile.
    pub(crate) async fn usage(&self, profile: &ProfileId) -> Vec<nsv_domain::wire::UsageRecord> {
        let profile = profile.clone();
        self.store
            .call(move |db| db.usage(&profile, "", 10_000))
            .await
            .unwrap()
    }

    /// Moves the clock until the profile has `count` usage rows; panics after `limit` or on
    /// more rows.
    pub(crate) async fn until_usage(&self, profile: &ProfileId, count: usize, limit: Duration) {
        let started = tokio::time::Instant::now();
        loop {
            self.settle().await;
            let stored = self.usage(profile).await.len();
            assert!(stored <= count, "{stored} usage rows, expected {count}");
            if stored == count {
                return;
            }
            assert!(
                started.elapsed() <= limit,
                "timed out waiting for {count} usage rows"
            );
            tokio::time::advance(Duration::from_millis(50)).await;
        }
    }

    pub(crate) fn reports(&self) -> Vec<String> {
        self.host.reports.lock().unwrap().clone()
    }

    pub(crate) fn launches(&self, profile: &ProfileId) -> usize {
        self.fake.launches_of(profile).len()
    }
}
