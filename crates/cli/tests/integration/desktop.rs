//! The desktop app as a process, started with `--background` as its autostart entry starts it.
//!
//! Linux only: without a display or a session bus it runs without a tray or windows, and a
//! stub `xdg-open` takes the session link it opens instead. On macOS it would show a status
//! item and real windows.
#![cfg(target_os = "linux")]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use nsv_testkit::{Port, TempDir, write_executable};
use serde_json::{Value, json};

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

    fn write_session(&self, text: &str) {
        nsv_files::write_atomic(&self.data().join("server.json"), text.as_bytes(), 0o600).unwrap();
    }
}

/// A test that fails part-way leaves no app behind.
struct App(Child);

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl App {
    async fn exited(&mut self) -> ExitStatus {
        let started = Instant::now();
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the app did not exit"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn stderr(&mut self) -> String {
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut self.0.stderr.take().unwrap(), &mut stderr).unwrap();
        stderr
    }
}

/// After an unclean exit, a second launch can read the old `server.json` before the starting
/// app replaces it: it waits for the app's own instead of failing on the old token.
#[tokio::test]
async fn a_second_launch_reaches_the_running_app_past_a_stale_server_json() {
    let root = Root::new("second");
    let port = Port::reserve().release();
    let spawn = || {
        App(root
            .desktop(port)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap())
    };
    let mut first = spawn();
    let started = Instant::now();
    let session = loop {
        if let Ok(text) = std::fs::read_to_string(root.data().join("server.json"))
            && serde_json::from_str::<Value>(&text).unwrap()["pid"] == first.0.id()
        {
            break text;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the app did not start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    root.write_session(
        &json!({
            "pid": 4242, "port": port, "token": "stale", "allowHosts": [],
            "version": "0.2.0", "startedAt": "2026-11-22T11:00:00.000Z",
        })
        .to_string(),
    );
    let mut second = spawn();
    tokio::time::sleep(Duration::from_secs(1)).await;
    root.write_session(&session);
    let status = second.exited().await;
    assert!(status.success(), "{status}: {}", second.stderr());
    assert_eq!(
        first.0.try_wait().unwrap(),
        None,
        "the first app still runs"
    );
    assert!(
        Command::new("kill")
            .args(["-TERM", &first.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let status = first.exited().await;
    assert!(status.success(), "{status}: {}", first.stderr());
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
