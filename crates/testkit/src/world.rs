//! The account features assembled as the daemon assembles them, over fake cores, a stub
//! provider fetch and stub releases, in real time.
#![expect(
    clippy::indexing_slicing,
    clippy::missing_panics_doc,
    clippy::unwrap_used,
    reason = "test support: a panic fails the test that called it, and is its report"
)]

use std::cell::Cell;
use std::future::ready;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nsv_accounts::cores::{Cores, CoresOptions};
use nsv_accounts::fetch::{Outside, Request};
use nsv_accounts::scheduler::{TIMING, Timing};
use nsv_core::binary::{CoreManifest, CoreTarget, Sha256Digest, bundled_pin};
use nsv_core::install::Releases;
use nsv_core::pool::{Backend, BoxFuture};
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::version::AppVersion;
use nsv_files::{DataDir, owner_only_directory};
use nsv_net::{Loopback, NetError, Response};
use nsv_store::{Store, Vault};
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore};

use crate::TempDir;
use crate::fake::{Directory, FakeCores, response};

pub async fn open_store(directory: &Path) -> Store {
    owner_only_directory(directory, || Ok::<(), std::io::Error>(())).unwrap();
    let data = DataDir::new(directory);
    Store::open(&data, Vault::open(&data).unwrap())
        .await
        .unwrap()
}

pub async fn profile(store: &Store, name: &str) -> ProfileId {
    let name = name.to_owned();
    store
        .call(move |db| Ok(db.create_profile(&name)?.id))
        .await
        .unwrap()
}

/// What a stub fetch was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub request: Request,
    pub loopback: Option<(Loopback, String)>,
}

type Answer = Box<dyn Fn(&str) -> Result<Response, NetError> + Send + Sync>;

/// The outside world of a test: answers each provider request by its URL and records it (a
/// gate holds answers until it opens), and serves [`StubFetch::releases`].
pub struct StubFetch {
    pub seen: Mutex<Vec<Seen>>,
    answer: Mutex<Answer>,
    gate: Option<Arc<Semaphore>>,
    pub releases: Arc<TestReleases>,
}

impl StubFetch {
    pub fn new(
        answer: impl Fn(&str) -> Result<Response, NetError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            answer: Mutex::new(Box::new(answer)),
            gate: None,
            releases: Arc::default(),
        }
    }

    /// Holds every answer until a permit is added to the returned gate.
    pub fn gated(
        answer: impl Fn(&str) -> Result<Response, NetError> + Send + Sync + 'static,
    ) -> (Self, Arc<Semaphore>) {
        let gate = Arc::new(Semaphore::new(0));
        let mut stub = Self::new(answer);
        stub.gate = Some(Arc::clone(&gate));
        (stub, gate)
    }

    pub fn answer(
        &self,
        answer: impl Fn(&str) -> Result<Response, NetError> + Send + Sync + 'static,
    ) {
        *self.answer.lock().unwrap() = Box::new(answer);
    }

    #[must_use]
    pub fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    async fn reply(&self, seen: Seen) -> Result<Response, NetError> {
        let url = seen.request.url.clone();
        self.seen.lock().unwrap().push(seen);
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        (self.answer.lock().unwrap())(&url)
    }
}

impl Outside for StubFetch {
    fn https(&self, request: Request) -> BoxFuture<'_, Result<Response, NetError>> {
        Box::pin(self.reply(Seen {
            request,
            loopback: None,
        }))
    }

    fn loopback(
        &self,
        to: Loopback,
        target: String,
        request: Request,
    ) -> BoxFuture<'_, Result<Response, NetError>> {
        Box::pin(self.reply(Seen {
            request,
            loopback: Some((to, target)),
        }))
    }
}

impl Releases for StubFetch {
    fn feed<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Response, NetError>> {
        self.releases.feed(url)
    }

    fn download<'a>(
        &'a self,
        url: &'a str,
        path: &'a Path,
    ) -> BoxFuture<'a, Result<Sha256Digest, AppError>> {
        self.releases.download(url, path)
    }
}

thread_local! {
    /// Moves this test's clock; each tokio test runs its tasks on its own thread.
    static SKEW: Cell<i64> = const { Cell::new(0) };
}

pub fn skew(milliseconds: i64) {
    SKEW.with(|skew| skew.set(milliseconds));
}

fn clock() -> i64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    i64::try_from(now.as_millis()).unwrap() + SKEW.with(Cell::get)
}

/// The feed and archives a test serves. The archive URL must be upstream's.
#[derive(Default)]
pub struct TestReleases {
    pub feed: Mutex<Value>,
    pub archive: Mutex<Vec<u8>>,
    pub downloads: Mutex<Vec<String>>,
    /// Holds downloads until notified; tells when one waits.
    pub hold: Mutex<Option<(Arc<Notify>, Arc<Notify>)>>,
}

impl Releases for TestReleases {
    fn feed<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Response, NetError>> {
        Box::pin(ready(Ok(response(
            200,
            self.feed.lock().unwrap().to_string(),
        ))))
    }

    fn download<'a>(
        &'a self,
        url: &'a str,
        path: &'a Path,
    ) -> BoxFuture<'a, Result<Sha256Digest, AppError>> {
        Box::pin(async move {
            self.downloads.lock().unwrap().push(url.to_owned());
            let hold = self.hold.lock().unwrap().clone();
            if let Some((waiting, release)) = hold {
                waiting.notify_one();
                release.notified().await;
            }
            let bytes = self.archive.lock().unwrap().clone();
            std::fs::write(path, &bytes).unwrap();
            Ok(Sha256Digest::of_file(path).unwrap())
        })
    }
}

pub struct World {
    pub directory: TempDir,
    pub store: Store,
    pub fake: FakeCores,
    pub fetch: Arc<StubFetch>,
    pub releases: Arc<TestReleases>,
    pub cores: Arc<Cores>,
    pub reports: Arc<Mutex<Vec<String>>>,
    pub accepted: Arc<AtomicUsize>,
    pub bundled: PathBuf,
}

/// The version of the core update a test releases.
pub const UPDATE: &str = "99.0.0";

/// `bytes` released as core `UPDATE`: its archive and a pin naming it for this target.
#[must_use]
pub fn release(work: &Path, bytes: &[u8]) -> (Vec<u8>, Value) {
    let stage = work.join(format!("stage-{}", bytes.len()));
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(stage.join("cli-proxy-api"), bytes).unwrap();
    let archive = stage.join("release.tar.gz");
    let packed = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&stage)
        .arg("cli-proxy-api")
        .status()
        .unwrap();
    assert!(packed.success());
    let target = CoreTarget::current().unwrap();
    let key = format!("{}_{}", target.platform(), target.arch());
    let mut pin: Value =
        serde_json::from_str(include_str!("../../core/core-release.json")).unwrap();
    pin["version"] = json!(UPDATE);
    pin["checksums"][&key] = json!(Sha256Digest::of_file(&archive).unwrap().to_string());
    pin["binaries"][&key] = json!(
        Sha256Digest::of_file(&stage.join("cli-proxy-api"))
            .unwrap()
            .to_string()
    );
    (std::fs::read(&archive).unwrap(), pin)
}

/// A bundled core of this test's own: a file with its manifest.
#[must_use]
pub fn bundled_core(directory: &Path) -> PathBuf {
    let core = directory.join("bundled");
    std::fs::create_dir_all(&core).unwrap();
    let binary = core.join("cli-proxy-api");
    std::fs::write(&binary, b"bundled core").unwrap();
    let hash = Sha256Digest::of_file(&binary).unwrap();
    let pin = bundled_pin().unwrap();
    CoreManifest::installed(pin, CoreTarget::current().unwrap(), hash, hash)
        .write(&core)
        .unwrap();
    binary
}

impl World {
    pub async fn new(name: &str) -> Self {
        Self::with(name, None, *TIMING).await
    }

    /// An installed build reading `feed`, or a development run without one.
    pub async fn with(name: &str, feed: Option<&str>, timing: Timing) -> Self {
        let directory = TempDir::new(name);
        let data = directory.path().join("data");
        let bundled = bundled_core(directory.path());
        Self::open(
            directory,
            &data,
            bundled,
            feed,
            timing,
            FakeCores::default(),
            AppVersion::current(),
        )
        .await
    }

    /// The features over `data` in an app at `app`, as a relaunch opens them.
    pub async fn open(
        directory: TempDir,
        data: &Path,
        bundled: PathBuf,
        feed: Option<&str>,
        timing: Timing,
        fake: FakeCores,
        app: AppVersion,
    ) -> Self {
        let store = open_store(data).await;
        let fetch = Arc::new(StubFetch::new(|_| Ok(response(404, "{}"))));
        let releases = Arc::clone(&fetch.releases);
        let reports = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(AtomicUsize::new(0));
        let backends_fake = fake.clone();
        let (reported, counted) = (Arc::clone(&reports), Arc::clone(&accepted));
        let cores = Arc::new(
            Cores::new(CoresOptions {
                store: store.clone(),
                binary: bundled.clone(),
                backends: Arc::new(move |data: &DataDir| -> Arc<dyn Backend> {
                    Arc::new(Directory(backends_fake.clone(), data.clone()))
                }),
                ready: Duration::from_secs(5),
                timing,
                outside: Arc::<StubFetch>::clone(&fetch),
                feed: feed.map(str::to_owned),
                require_pinned_core: false,
                allow_missing_core: false,
                clock,
                report: Arc::new(move |message| reported.lock().unwrap().push(message)),
                sign_in_accepted: Arc::new(move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                }),
                app,
            })
            .unwrap(),
        );
        Self {
            directory,
            store,
            fake,
            fetch,
            releases,
            cores,
            reports,
            accepted,
            bundled,
        }
    }

    pub async fn profile(&self, name: &str) -> ProfileId {
        profile(&self.store, name).await
    }

    #[must_use]
    pub fn data(&self) -> &Path {
        self.store.directory().path()
    }

    #[must_use]
    pub fn pending(&self) -> Vec<String> {
        names(&self.data().join("oauth-pending"))
    }

    #[must_use]
    pub fn credential_file(&self, profile: &ProfileId, name: &str) -> PathBuf {
        self.store.directory().credentials(profile).join(name)
    }
}

/// The entry names of a directory, sorted; none when it is missing.
#[must_use]
pub fn names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory).map_or_else(
        |_| Vec::new(),
        |listing| {
            listing
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        },
    );
    names.sort();
    names
}

/// A Claude credential for one seat of `organization`, as the provider returns it.
#[must_use]
pub fn claude(organization: &str, token: &str) -> Value {
    json!({
        "type": "claude",
        "email": "same@example.invalid",
        "access_token": format!("synthetic-access-{token}"),
        "refresh_token": format!("synthetic-refresh-{token}"),
        "account_uuid": ACCOUNT,
        "organization_uuid": organization,
        "organization_name": if organization == ORGANIZATION_A { "Example A" } else { "Example B" },
    })
}

pub const ACCOUNT: &str = "aaaaaaaa-0000-4000-8000-000000000001";
pub const ORGANIZATION_A: &str = "bbbbbbbb-0000-4000-8000-00000000000a";
pub const ORGANIZATION_B: &str = "bbbbbbbb-0000-4000-8000-00000000000b";
