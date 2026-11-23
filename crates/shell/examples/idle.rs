//! The shell alone and idle: the tray registered, no window. `tests/desktop.rs`'s idle
//! measurement runs it to price the daemon's desktop side.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nsv_domain::helper::Origin;
use nsv_domain::id::ProfileId;
use nsv_domain::secret::SessionToken;
use nsv_shell::{HostEvent, Login, Options, ShellHost, TrayStatus};
use tokio::sync::{broadcast, watch};

struct Idle {
    origin: Origin,
    token: SessionToken,
    data: PathBuf,
    status: watch::Sender<TrayStatus>,
    events: broadcast::Sender<HostEvent>,
}

impl ShellHost for Idle {
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
        self.events.subscribe()
    }
    fn has_profile(&self, _profile: &ProfileId) -> bool {
        false
    }
    fn report(&self, _message: &str) {}
    fn shutdown(&self) {}
}

fn main() -> ExitCode {
    let Ok(origin) = Origin::parse("http://127.0.0.1:4318") else {
        return ExitCode::FAILURE;
    };
    let data = std::env::temp_dir();
    let host = Idle {
        origin,
        token: SessionToken::new("idle"),
        data: data.clone(),
        status: watch::channel(TrayStatus {
            running: vec!["Work".to_owned()],
            streaming: false,
        })
        .0,
        events: broadcast::channel(1).0,
    };
    let options = Options {
        helper: data.join("no-helper"),
        open_window: false,
        packaged: false,
        login: Login::new(&data, data.join("idle")),
    };
    nsv_shell::run_desktop(|link| {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return 1;
        };
        runtime.block_on(nsv_shell::run(&host, options, link));
        0
    })
}
