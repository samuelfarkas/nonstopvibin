//! A data directory's store and fake cores under tokio's paused clock, which moves only when
//! a test moves it.
#![expect(
    clippy::missing_panics_doc,
    clippy::unwrap_used,
    reason = "test support: a panic fails the test that called it, and is its report"
)]

use std::path::PathBuf;
use std::time::Duration;

use nsv_domain::id::ProfileId;
use nsv_domain::secret::ApiKey;
use nsv_domain::wire::{Account, ApiAccount};
use nsv_files::DataDir;
use nsv_store::{Store, Vault};
use serde_json::json;

use crate::TempDir;
use crate::fake::{Directory, FakeCores};

pub struct Paused {
    _root: TempDir,
    pub data: PathBuf,
    /// The core executable a pool is given; the fake cores never run it.
    pub binary: PathBuf,
    pub store: Store,
    pub fake: FakeCores,
    hold: std::sync::mpsc::Sender<()>,
}

impl Drop for Paused {
    fn drop(&mut self) {
        let _ = self.hold.send(());
    }
}

impl Paused {
    /// Must run on a runtime with a paused clock (`start_paused`). Auto-advance stays off for
    /// its life: only [`Paused::pass`], [`Paused::until`] and [`Paused::run`] move the clock.
    pub async fn new() -> Self {
        let (hold, release) = std::sync::mpsc::channel::<()>();
        // A running blocking task keeps the paused clock from jumping while the store thread
        // answers: time moves only when a test moves it.
        tokio::task::spawn_blocking(move || {
            let _ = release.recv();
        });
        let root = TempDir::new("world");
        let data = root.path().join("data");
        nsv_files::owner_only_directory(&data, || Ok::<(), std::io::Error>(())).unwrap();
        let directory = DataDir::new(&data);
        let store = Store::open(&directory, Vault::open(&directory).unwrap())
            .await
            .unwrap();
        let binary = root.path().join("cli-proxy-api");
        std::fs::write(&binary, "fake").unwrap();
        Self {
            _root: root,
            data,
            binary,
            store,
            fake: FakeCores::default(),
            hold,
        }
    }

    /// The fake cores of this store's data directory.
    #[must_use]
    pub fn backend(&self) -> Directory {
        Directory(self.fake.clone(), self.store.directory().clone())
    }

    /// A profile; `enabled` as a run that started it left it, with an API-key account and an
    /// OAuth subscription as asked.
    pub async fn profile(&self, name: &str, enabled: bool, api: bool, oauth: bool) -> ProfileId {
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

    /// Lets every task run until nothing moves without the clock. Work on the blocking pool,
    /// such as an account sync's credential scan, may still be running after it.
    pub async fn settle(&self) {
        for _ in 0..24 {
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            let _ = self.store.call(|_| Ok(())).await;
        }
    }

    /// Moves the clock by `total` in steps of at most `step`, settling after each.
    pub async fn pass_by(&self, total: Duration, step: Duration) {
        let mut left = total;
        self.settle().await;
        while !left.is_zero() {
            let now = step.min(left);
            tokio::time::advance(now).await;
            self.settle().await;
            left -= now;
        }
    }

    pub async fn pass(&self, total: Duration) {
        self.pass_by(total, Duration::from_millis(50)).await;
    }

    /// Moves the clock until `condition` holds; panics after `limit`.
    pub async fn until(&self, what: &str, limit: Duration, condition: impl Fn() -> bool) {
        let started = tokio::time::Instant::now();
        self.settle().await;
        while !condition() {
            assert!(started.elapsed() <= limit, "timed out waiting for {what}");
            tokio::time::advance(Duration::from_millis(50)).await;
            self.settle().await;
        }
    }

    /// Moves the clock while `future` runs, and answers what it answered.
    pub async fn run<T>(&self, future: impl Future<Output = T>) -> T {
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
}
