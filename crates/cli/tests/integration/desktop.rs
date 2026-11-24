//! The desktop app as a process, started with `--background` as its autostart entry starts it.
//!
//! Linux only: without a display or a session bus it runs without a tray or windows, and a
//! stub `xdg-open` takes the session link it opens instead. On macOS it would show a status
//! item and real windows.
#![cfg(target_os = "linux")]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use nsv_testkit::{TempDir, write_executable};

/// A run's home, data directory and stub `xdg-open`.
struct Root(TempDir);

impl Root {
    fn new(name: &str) -> Self {
        let root = TempDir::new(&format!("desktop-{name}"));
        std::fs::create_dir_all(root.path().join("home")).unwrap();
        std::fs::create_dir_all(root.path().join("bin")).unwrap();
        write_executable(&root.path().join("bin/xdg-open"), b"#!/bin/sh\nexit 0\n");
        Self(root)
    }

    fn data(&self) -> PathBuf {
        self.0.path().join("data")
    }

    /// The desktop app on `port`, with no session bus to register a tray or notify on.
    fn desktop(&self, port: u16) -> Command {
        let root = self.0.path();
        let mut command = Command::new(env!("CARGO_BIN_EXE_nonstopvibin"));
        command
            .arg("--background")
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .env("HOME", root.join("home"))
            .env("NONSTOPVIBIN_DATA_DIR", self.data())
            .env("NONSTOPVIBIN_PORT", port.to_string())
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", root.join("no-bus").display()),
            )
            .stdin(Stdio::null());
        command
    }
}

#[test]
fn a_taken_port_tells_the_desktop_user_to_set_nonstopvibin_port() {
    let root = Root::new("port");
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let output = root.desktop(port).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "NonstopVibin could not start: Port {port} on 127.0.0.1 is already in use. Stop the other program, or set NONSTOPVIBIN_PORT to another port before starting NonstopVibin.\n"
        )),
        "{stderr}"
    );
}
