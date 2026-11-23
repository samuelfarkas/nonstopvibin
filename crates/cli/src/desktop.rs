//! The desktop app's daemon, as its shell sees it.

use nsv_desktop::{HostEvent, TrayStatus};
use nsv_domain::endpoint::Origin;
use nsv_domain::id::ProfileId;
use nsv_domain::secret::SessionToken;
use nsv_files::DataDir;
use nsv_net::Https;
use nsv_server::Daemon;
use nsv_shell::ShellHost;
use tokio::sync::{broadcast, watch};

pub(crate) struct Desktop(pub(crate) Daemon<Https>);

impl ShellHost for Desktop {
    fn origin(&self) -> &Origin {
        self.0.origin()
    }

    fn session_token(&self) -> &SessionToken {
        self.0.session_token()
    }

    fn data_directory(&self) -> &DataDir {
        self.0.data_directory()
    }

    fn tray_status(&self) -> watch::Receiver<TrayStatus> {
        self.0.tray_status()
    }

    fn events(&self) -> broadcast::Receiver<HostEvent> {
        self.0.events()
    }

    fn has_profile(&self, profile: &ProfileId) -> bool {
        self.0.has_profile(profile)
    }

    fn report(&self, message: &str) {
        self.0.report(message);
    }

    fn shutdown(&self) {
        self.0.stop();
    }

    async fn close(&self) -> Result<(), String> {
        self.0.close().await.map_err(|error| error.to_string())
    }
}
