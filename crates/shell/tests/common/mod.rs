//! The shell on a thread of its own, and the waits the tests share.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use nsv_shell::{Login, Options};
use nsv_testkit::shell::Host;

pub(crate) const TIMEOUT: Duration = Duration::from_secs(20);

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
