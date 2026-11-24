//! The desktop app's daemon, as its shell sees it.

use nsv_files::DataDir;
use nsv_server::Daemon;
use nsv_shell::{HostEvent, Origin, ProfileId, SessionToken, ShellHost, TrayStatus};
use tokio::sync::{broadcast, watch};

/// The daemon, and the desktop part it was started with.
pub(crate) struct Desktop {
    pub(crate) daemon: Daemon,
    pub(crate) desktop: nsv_server::Desktop,
}

impl ShellHost for Desktop {
    fn origin(&self) -> &Origin {
        self.daemon.origin()
    }

    fn session_token(&self) -> &SessionToken {
        self.daemon.session_token()
    }

    fn data_directory(&self) -> &DataDir {
        self.daemon.data_directory()
    }

    fn tray_status(&self) -> watch::Receiver<TrayStatus> {
        self.desktop.tray_status()
    }

    fn events(&self) -> broadcast::Receiver<HostEvent> {
        self.desktop.events()
    }

    fn has_profile(&self, profile: &ProfileId) -> bool {
        self.daemon.has_profile(profile)
    }

    fn report(&self, message: &str) {
        self.daemon.report(message);
    }

    fn shutdown(&self) {
        self.daemon.stop();
    }

    async fn close(&self) -> Result<(), String> {
        self.daemon.close().await.map_err(|error| error.to_string())
    }
}
