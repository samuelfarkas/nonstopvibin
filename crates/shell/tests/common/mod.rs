//! A daemon stand-in for the shell, and the waits the tests share.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nsv_domain::helper::Origin;
use nsv_domain::id::ProfileId;
use nsv_domain::secret::SessionToken;
use nsv_shell::{HostEvent, Login, Options, ShellHost, TrayStatus};
use tokio::sync::{broadcast, watch};

pub(crate) const TOKEN: &str = "session-token-for-the-shell-tests";
pub(crate) const PROFILE: &str = "0b6f4f5e-6a7b-4c8d-9e0f-1a2b3c4d5e6f";
pub(crate) const TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) struct Host {
    origin: Origin,
    token: SessionToken,
    data: PathBuf,
    pub(crate) status: watch::Sender<TrayStatus>,
    /// `None` once closed, which ends the shell.
    events: Mutex<Option<broadcast::Sender<HostEvent>>>,
    /// Subscribed up front, so events sent before the shell starts still reach it.
    first: Mutex<Option<broadcast::Receiver<HostEvent>>>,
    pub(crate) reports: Mutex<Vec<String>>,
    pub(crate) shut_down: AtomicBool,
}

impl Host {
    pub(crate) fn new(port: u16, data: &Path) -> Arc<Self> {
        let (events, first) = broadcast::channel(8);
        Arc::new(Self {
            origin: Origin::parse(&format!("http://127.0.0.1:{port}")).unwrap(),
            token: SessionToken::new(TOKEN),
            data: data.to_owned(),
            status: watch::channel(TrayStatus::default()).0,
            events: Mutex::new(Some(events)),
            first: Mutex::new(Some(first)),
            reports: Mutex::new(Vec::new()),
            shut_down: AtomicBool::new(false),
        })
    }

    pub(crate) fn send(&self, event: HostEvent) {
        self.events
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(event)
            .unwrap();
    }

    /// Ends the shell, as a daemon that stops.
    pub(crate) fn close(&self) {
        self.events.lock().unwrap().take();
    }

    pub(crate) fn reports(&self) -> Vec<String> {
        self.reports.lock().unwrap().clone()
    }
}

impl ShellHost for Host {
    fn origin(&self) -> &Origin {
        &self.origin
    }
    fn session_token(&self) -> &SessionToken {
        &self.token
    }
    fn data_directory(&self) -> &Path {
        &self.data
    }
    fn tray_status(&self) -> watch::Receiver<TrayStatus> {
        self.status.subscribe()
    }
    fn events(&self) -> broadcast::Receiver<HostEvent> {
        let first = self.first.lock().unwrap().take();
        first.unwrap_or_else(|| self.events.lock().unwrap().as_ref().unwrap().subscribe())
    }
    fn has_profile(&self, profile: &ProfileId) -> bool {
        profile.as_str() == PROFILE
    }
    fn report(&self, message: &str) {
        self.reports.lock().unwrap().push(message.to_owned());
    }
    fn shutdown(&self) {
        self.shut_down.store(true, Ordering::SeqCst);
    }
    fn close(&self) -> impl Future<Output = Result<(), String>> {
        std::future::ready(Ok(()))
    }
}

pub(crate) fn directory(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("nsv-shell-{name}-{}", std::process::id()));
    drop(fs::remove_dir_all(&path));
    fs::create_dir_all(&path).unwrap();
    path
}

pub(crate) fn options(
    helper: PathBuf,
    open_window: bool,
    packaged: bool,
    config: &Path,
) -> Options {
    Options {
        helper,
        open_window,
        packaged,
        login: Login::new(config, PathBuf::from("/opt/NonstopVibin/nonstopvibin")),
    }
}

/// Runs the shell on its own thread and runtime, as the daemon's main thread would.
pub(crate) fn spawn_shell(host: &Arc<Host>, options: Options) -> thread::JoinHandle<()> {
    let host = Arc::clone(host);
    thread::spawn(move || {
        let code = nsv_shell::run_desktop(|link| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime
                .block_on(nsv_shell::run(&*host, options, link))
                .unwrap();
            0
        });
        assert_eq!(code, std::process::ExitCode::SUCCESS);
    })
}

#[expect(clippy::disallowed_methods, reason = "test threads, not a runtime")]
pub(crate) fn pause(duration: Duration) {
    thread::sleep(duration);
}

/// Polls `ready` every 50 ms until it holds or `TIMEOUT` passes.
pub(crate) fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        pause(Duration::from_millis(50));
    }
}

/// Whether `pid` has exited (a zombie has closed its pipes too).
pub(crate) fn exited(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z'))
    })
}
