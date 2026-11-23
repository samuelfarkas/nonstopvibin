//! A daemon's store, accounts, scheduler and pool on fake cores, under a clock that moves only
//! when a test moves it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nsv_accounts::accounts::Accounts;
use nsv_accounts::scheduler::{Host, Scheduler, TIMING, Timing};
use nsv_core::pool::{BoxFuture, Pool};
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::secret::ApiKey;
use nsv_domain::wire::{Account, ApiAccount};
use nsv_store::{Store, Vault};
use serde_json::json;
use tokio::sync::watch;

use crate::fake::Fake;

/// A directory under the system temporary directory, removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "nsv-accounts-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The host: reports are kept, quota checks recorded and answered as a test says.
#[derive(Default)]
pub(crate) struct TestHost {
    pub(crate) reports: Mutex<Vec<String>>,
    pub(crate) signing_in: Mutex<Option<ProfileId>>,
    pub(crate) quota_checks: Mutex<Vec<ProfileId>>,
    /// While set, quota checks wait for it to turn true.
    pub(crate) quota_gate: Mutex<Option<watch::Receiver<bool>>>,
    /// Quota checks answer this error.
    pub(crate) quota_error: Mutex<Option<String>>,
    /// Quota checks that ran to their end and stored.
    pub(crate) quota_stored: Arc<Mutex<Vec<ProfileId>>>,
}

impl Host for TestHost {
    fn report(&self, message: String) {
        self.reports.lock().unwrap().push(message);
    }

    fn signing_in(&self, profile: &ProfileId) -> bool {
        self.signing_in.lock().unwrap().as_ref() == Some(profile)
    }

    fn refresh_quotas(
        &self,
        _pool: Pool,
        profile: ProfileId,
    ) -> BoxFuture<'static, Result<(), AppError>> {
        self.quota_checks.lock().unwrap().push(profile.clone());
        let gate = self.quota_gate.lock().unwrap().clone();
        let error = self.quota_error.lock().unwrap().clone();
        let stored = Arc::clone(&self.quota_stored);
        Box::pin(async move {
            if let Some(mut gate) = gate {
                let _ = gate.wait_for(|open| *open).await;
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
        restart: nsv_core::restart::RestartTiming {
            coalesce: 200,
            ..TIMING.restart
        },
        activity: Duration::from_secs(1),
        idle,
        ..TIMING
    }
}

pub(crate) struct World {
    _root: TempDir,
    pub(crate) data: PathBuf,
    pub(crate) store: Store,
    pub(crate) fake: Fake,
    pub(crate) host: Arc<TestHost>,
    pub(crate) accounts: Arc<Accounts>,
    pub(crate) scheduler: Scheduler,
    hold: std::sync::mpsc::Sender<()>,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.hold.send(());
    }
}

impl World {
    /// Must run on a runtime with a paused clock (`start_paused`). Auto-advance stays off for
    /// the world's life: only [`World::pass`] moves the clock.
    pub(crate) async fn new(timing: Timing) -> Self {
        Self::with_ready(timing, nsv_core::pool::READY).await
    }

    pub(crate) async fn with_ready(timing: Timing, ready: Duration) -> Self {
        let (hold, release) = std::sync::mpsc::channel::<()>();
        // A running blocking task keeps the paused clock from jumping while the store thread
        // answers: time moves only when a test moves it.
        tokio::task::spawn_blocking(move || {
            let _ = release.recv();
        });
        let root = TempDir::new("world");
        let data = root.path().join("data");
        nsv_store::files::owner_only_directory(&data, || Ok::<(), std::io::Error>(())).unwrap();
        let store = Store::open(&data, Vault::open(&data).unwrap())
            .await
            .unwrap();
        let binary = root.path().join("cli-proxy-api");
        fs::write(&binary, "fake").unwrap();
        let fake = Fake::default();
        let host = Arc::new(TestHost::default());
        let shared: Arc<dyn Host> = Arc::<TestHost>::clone(&host);
        let accounts = Arc::new(Accounts::new(store.clone()));
        let scheduler = Scheduler::new(
            store.clone(),
            binary,
            Arc::new(fake.clone()),
            ready,
            Arc::clone(&accounts),
            shared,
            timing,
        );
        Self {
            _root: root,
            data,
            store,
            fake,
            host,
            accounts,
            scheduler,
            hold,
        }
    }

    pub(crate) fn pool(&self) -> &Pool {
        self.scheduler.pool()
    }

    /// A profile; `enabled` as a run that started it left it, with an API-key account and an
    /// OAuth subscription as asked.
    pub(crate) async fn profile(
        &self,
        name: &str,
        enabled: bool,
        api: bool,
        oauth: bool,
    ) -> ProfileId {
        let name = name.to_owned();
        self.store
            .call(move |db| {
                let mut profile = db.create_profile(&name)?;
                profile.enabled = enabled;
                db.save_profile(&profile)?;
                if api {
                    // Account ids are unique across profiles.
                    let account: ApiAccount = serde_json::from_value(json!({
                        "id": format!("key-{}", profile.id.as_str()), "name": "lazy-key", "provider": "custom",
                        "baseUrl": "http://127.0.0.1:9/v1", "prefix": "",
                        "models": [{ "id": "fixture-model", "protocol": "openai" }],
                        "disabled": false,
                    }))?;
                    db.save_api_account(&profile.id, &account, Some(&ApiKey::new("lazy-key")))?;
                }
                if oauth {
                    let account: Account = serde_json::from_value(json!({
                        "id": "claude.json", "name": "Claude", "provider": "claude",
                        "kind": "oauth", "disabled": false, "status": "Ready",
                        "priority": 0, "authIndex": "1",
                    }))?;
                    db.save_account_metadata(&profile.id, &[account])?;
                }
                Ok(profile.id)
            })
            .await
            .unwrap()
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

    /// Lets every task run until nothing moves without the clock. Work on the blocking pool,
    /// such as an account sync's credential scan, may still be running after it.
    pub(crate) async fn settle(&self) {
        for _ in 0..24 {
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            let _ = self.store.call(|_| Ok(())).await;
        }
    }

    /// Moves the clock by `total` in steps of at most `step`, settling after each.
    pub(crate) async fn pass_by(&self, total: Duration, step: Duration) {
        let mut left = total;
        self.settle().await;
        while !left.is_zero() {
            let now = step.min(left);
            tokio::time::advance(now).await;
            self.settle().await;
            left -= now;
        }
    }

    pub(crate) async fn pass(&self, total: Duration) {
        self.pass_by(total, Duration::from_millis(50)).await;
    }

    /// Moves the clock until `condition` holds; panics after `limit`.
    pub(crate) async fn until(
        &self,
        what: &str,
        limit: Duration,
        condition: impl Fn(&Self) -> bool,
    ) {
        let started = tokio::time::Instant::now();
        self.settle().await;
        while !condition(self) {
            assert!(started.elapsed() <= limit, "timed out waiting for {what}");
            tokio::time::advance(Duration::from_millis(50)).await;
            self.settle().await;
        }
    }

    /// Moves the clock while `future` runs, and answers what it answered.
    pub(crate) async fn run<T>(&self, future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        loop {
            tokio::select! {
                biased;
                value = &mut future => return value,
                () = async {
                    self.settle().await;
                    tokio::time::advance(Duration::from_millis(50)).await;
                } => {}
            }
        }
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
        self.fake.launches(profile).len()
    }
}
