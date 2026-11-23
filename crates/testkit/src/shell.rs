//! A daemon stand-in for the shell: a [`ShellHost`] that knows one profile, records what the
//! shell reports and sends the events a test asks for.
#![expect(
    clippy::missing_panics_doc,
    clippy::unwrap_used,
    reason = "test support: a panic fails the test that called it, and is its report"
)]

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nsv_desktop::{HostEvent, TrayStatus};
use nsv_domain::endpoint::{Origin, loopback_origin};
use nsv_domain::id::ProfileId;
use nsv_domain::secret::SessionToken;
use nsv_files::DataDir;
use nsv_shell::ShellHost;
use tokio::sync::{broadcast, watch};

pub const TOKEN: &str = "session-token-for-the-shell-tests";
/// The one profile the host has.
pub const PROFILE: &str = "0b6f4f5e-6a7b-4c8d-9e0f-1a2b3c4d5e6f";

pub struct Host {
    origin: Origin,
    token: SessionToken,
    data: DataDir,
    pub status: watch::Sender<TrayStatus>,
    /// `None` once closed, which ends the shell.
    events: Mutex<Option<broadcast::Sender<HostEvent>>>,
    /// Subscribed up front, so events sent before the shell starts still reach it.
    first: Mutex<Option<broadcast::Receiver<HostEvent>>>,
    reports: Mutex<Vec<String>>,
    pub shut_down: AtomicBool,
}

impl Host {
    /// A daemon on `http://127.0.0.1:<port>` with its data in `data`.
    #[must_use]
    pub fn new(port: u16, data: &Path) -> Arc<Self> {
        let (events, first) = broadcast::channel(8);
        Arc::new(Self {
            origin: Origin::parse(&loopback_origin(port)).unwrap(),
            token: SessionToken::new(TOKEN),
            data: DataDir::new(data),
            status: watch::channel(TrayStatus::default()).0,
            events: Mutex::new(Some(events)),
            first: Mutex::new(Some(first)),
            reports: Mutex::new(Vec::new()),
            shut_down: AtomicBool::new(false),
        })
    }

    pub fn send(&self, event: HostEvent) {
        self.events
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(event)
            .unwrap();
    }

    /// Ends the shell, as a daemon that stops.
    pub fn close(&self) {
        self.events.lock().unwrap().take();
    }

    #[must_use]
    pub fn reports(&self) -> Vec<String> {
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
    fn data_directory(&self) -> &DataDir {
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
