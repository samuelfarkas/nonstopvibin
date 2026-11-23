//! The account features assembled as the daemon assembles them, over fake cores, a stub
//! provider fetch and stub releases, in real time.

use std::cell::Cell;
use std::future::{Future, ready};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nsv_accounts::cores::{Cores, CoresOptions};
use nsv_accounts::scheduler::{TIMING, Timing};
use nsv_core::binary::{CoreManifest, CoreTarget, Sha256Digest, bundled_pin};
use nsv_core::install::Releases;
use nsv_core::pool::Backend;
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_net::{NetError, Response};
use nsv_store::Store;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::common::{StubFetch, TempDir, open_store, profile, response};
use crate::fake_core::{Directory, FakeCores};

thread_local! {
    /// Moves this test's clock; each tokio test runs its tasks on its own thread.
    static SKEW: Cell<i64> = const { Cell::new(0) };
}

pub(crate) fn skew(milliseconds: i64) {
    SKEW.with(|skew| skew.set(milliseconds));
}

fn clock() -> i64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    i64::try_from(now.as_millis()).unwrap() + SKEW.with(Cell::get)
}

/// The feed and archives a test serves. The archive URL must be upstream's.
#[derive(Default)]
pub(crate) struct TestReleases {
    pub(crate) feed: Mutex<Value>,
    pub(crate) archive: Mutex<Vec<u8>>,
    pub(crate) downloads: Mutex<Vec<String>>,
    /// Holds downloads until notified; tells when one waits.
    pub(crate) hold: Mutex<Option<(Arc<Notify>, Arc<Notify>)>>,
}

impl Releases for TestReleases {
    fn feed(&self, _: &str) -> impl Future<Output = Result<Response, NetError>> + Send {
        ready(Ok(response(200, self.feed.lock().unwrap().to_string())))
    }

    async fn download(&self, url: &str, path: &Path) -> Result<Sha256Digest, AppError> {
        self.downloads.lock().unwrap().push(url.to_owned());
        let hold = self.hold.lock().unwrap().clone();
        if let Some((waiting, release)) = hold {
            waiting.notify_one();
            release.notified().await;
        }
        let bytes = self.archive.lock().unwrap().clone();
        std::fs::write(path, &bytes).unwrap();
        Ok(Sha256Digest::of_file(path).unwrap())
    }
}

pub(crate) type TestCores = Cores<Arc<StubFetch>, Arc<TestReleases>>;

pub(crate) struct World {
    pub(crate) directory: TempDir,
    pub(crate) store: Store,
    pub(crate) fake: FakeCores,
    pub(crate) fetch: Arc<StubFetch>,
    pub(crate) releases: Arc<TestReleases>,
    pub(crate) cores: Arc<TestCores>,
    pub(crate) reports: Arc<Mutex<Vec<String>>>,
    pub(crate) accepted: Arc<AtomicUsize>,
    pub(crate) bundled: PathBuf,
}

/// The version of the core update a test releases.
pub(crate) const UPDATE: &str = "99.0.0";

/// `bytes` released as core `UPDATE`: its archive and a pin naming it for this target.
pub(crate) fn release(work: &Path, bytes: &[u8]) -> (Vec<u8>, Value) {
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
        serde_json::from_str(include_str!("../../../../src/server/core-release.json")).unwrap();
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
pub(crate) fn bundled_core(directory: &Path) -> PathBuf {
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
    pub(crate) async fn new(name: &str) -> Self {
        Self::with(name, None, TIMING).await
    }

    /// An installed build reading `feed`, or a development run without one.
    pub(crate) async fn with(name: &str, feed: Option<&str>, timing: Timing) -> Self {
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
        )
        .await
    }

    /// The features over `data`, as a relaunch opens them.
    pub(crate) async fn open(
        directory: TempDir,
        data: &Path,
        bundled: PathBuf,
        feed: Option<&str>,
        timing: Timing,
        fake: FakeCores,
    ) -> Self {
        let store = open_store(data).await;
        let fetch = Arc::new(StubFetch::new(|_| Ok(response(404, "{}"))));
        let releases = Arc::new(TestReleases::default());
        let reports = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(AtomicUsize::new(0));
        let backends_fake = fake.clone();
        let (reported, counted) = (Arc::clone(&reports), Arc::clone(&accepted));
        let cores = Arc::new(
            Cores::new(CoresOptions {
                store: store.clone(),
                binary: bundled.clone(),
                backends: Arc::new(move |data: &Path| -> Arc<dyn Backend> {
                    Arc::new(Directory(backends_fake.clone(), data.to_owned()))
                }),
                ready: Duration::from_secs(5),
                timing,
                fetch: Arc::clone(&fetch),
                releases: Arc::clone(&releases),
                feed: feed.map(str::to_owned),
                require_pinned_core: false,
                allow_missing_core: false,
                clock,
                report: Arc::new(move |message| reported.lock().unwrap().push(message)),
                sign_in_accepted: Arc::new(move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                }),
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

    pub(crate) async fn profile(&self, name: &str) -> ProfileId {
        profile(&self.store, name).await
    }

    pub(crate) fn data(&self) -> &Path {
        self.store.directory()
    }

    /// The names under `oauth-pending/`.
    pub(crate) fn pending(&self) -> Vec<String> {
        names(&self.data().join("oauth-pending"))
    }

    pub(crate) fn credential_file(&self, profile: &ProfileId, name: &str) -> PathBuf {
        nsv_store::layout::credential_directory(self.data(), profile).join(name)
    }
}

/// The entry names of a directory, sorted; none when it is missing.
pub(crate) fn names(directory: &Path) -> Vec<String> {
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
pub(crate) fn claude(organization: &str, token: &str) -> Value {
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

pub(crate) const ACCOUNT: &str = "aaaaaaaa-0000-4000-8000-000000000001";
pub(crate) const ORGANIZATION_A: &str = "bbbbbbbb-0000-4000-8000-00000000000a";
pub(crate) const ORGANIZATION_B: &str = "bbbbbbbb-0000-4000-8000-00000000000b";
