//! A daemon that cannot start says why and exits 1, as desktop-lifetime.cjs checked for the
//! Electron app. `serve` and the desktop app share the start (`nsv_server::daemon`).
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]

use std::net::TcpListener;
use std::process::{Command, Stdio};

#[test]
fn a_taken_port_ends_the_start_with_exit_1_and_says_so() {
    let root = std::env::temp_dir().join(format!("nsv-startup-{}", std::process::id()));
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
    std::fs::remove_dir_all(&root).unwrap_or_default();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "Port {port} on 127.0.0.1 is already in use. Stop the other program, or choose another port with --port."
        )),
        "{stderr}"
    );
}
