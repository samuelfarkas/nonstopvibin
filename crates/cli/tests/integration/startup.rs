//! A daemon that cannot start says why and exits 1, as desktop-lifetime.cjs checked for the
//! Electron app. `serve` and the desktop app share the start (`nsv_server::daemon`).

use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nsv_testkit::{Port, TempDir, copy_executable};

#[test]
fn a_taken_port_ends_the_start_with_exit_1_and_says_so() {
    let directory = TempDir::new("startup");
    let root = directory.path();
    std::fs::create_dir_all(root.join("home")).unwrap();
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let output = Command::new(env!("CARGO_BIN_EXE_nonstopvibin"))
        .args(["serve", "--port", &port.to_string()])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.join("home"))
        .env("NONSTOPVIBIN_DATA_DIR", root.join("data"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "Port {port} on 127.0.0.1 is already in use. Stop the other program, or choose another port with --port."
        )),
        "{stderr}"
    );
}

/// A package that lost its core is still a package: it names its own core, and refuses the
/// development allowance that would start it without one.
#[tokio::test]
async fn a_package_without_its_core_names_that_core_and_refuses_development() {
    let directory = TempDir::new("startup-package");
    let root = directory.path();
    std::fs::create_dir_all(root.join("home")).unwrap();
    let package = root.join("package");
    let (executable, resources) = if cfg!(target_os = "macos") {
        (
            package.join("Contents/MacOS/nonstopvibin"),
            package.join("Contents/Resources"),
        )
    } else {
        (package.join("nonstopvibin"), package.clone())
    };
    std::fs::create_dir_all(resources.join("client")).unwrap();
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    copy_executable(Path::new(env!("CARGO_BIN_EXE_nonstopvibin")), &executable);
    let mut child = Command::new(&executable)
        .args(["serve", "--port", &Port::reserve().release().to_string()])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.join("home"))
        .env("NONSTOPVIBIN_DATA_DIR", root.join("data"))
        .env("NONSTOPVIBIN_DEVELOPMENT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("a package without its core started");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    assert_eq!(status.code(), Some(1), "{stderr}");
    let core = std::fs::canonicalize(&resources)
        .unwrap()
        .join("core/cli-proxy-api");
    assert_eq!(
        stderr,
        format!(
            "The proxy core is missing at {}. Reinstall NonstopVibin from its release archive.\n",
            core.display()
        )
    );
}
