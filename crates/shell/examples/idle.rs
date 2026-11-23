//! The shell alone and idle: the tray registered, no window. `tests/desktop.rs`'s idle
//! measurement runs it to price the daemon's desktop side.

use std::process::ExitCode;

use nsv_shell::{Login, Options, TrayStatus};
use nsv_testkit::shell::Host;

fn main() -> ExitCode {
    let data = std::env::temp_dir();
    let host = Host::new(4318, &data);
    host.status.send_replace(TrayStatus {
        running: vec!["Work".to_owned()],
        streaming: false,
    });
    let options = Options {
        helper: data.join("no-helper"),
        open_window: false,
        packaged: false,
        #[cfg(target_os = "linux")]
        login: Login::new(&data, data.join("idle")),
        #[cfg(target_os = "macos")]
        login: Login::new(),
    };
    nsv_shell::run_desktop(move |link| {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return 1;
        };
        u8::from(
            runtime
                .block_on(nsv_shell::run(&*host, options, link))
                .is_err(),
        )
    })
}
