//! The debug `nonstopvibin serve` on a temporary data directory and HOME, with the built
//! interface (`dist/client`) and the pinned core (`.vendor/core`) of this checkout, as
//! crates/cli/tests/integration/serve.rs runs it. Its management API takes the place of the
//! store.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use std::{env, fs};

use nsv_testkit::Port;
use serde_json::{Value, json};

use crate::common::{TIMEOUT, pause, wait_until};

pub(crate) struct Daemon {
    child: Child,
    pub(crate) port: u16,
    pub(crate) token: String,
    root: PathBuf,
}

/// The `nonstopvibin` binary beside the window helper, where `cargo build -p nsv-cli` puts it.
fn binary() -> PathBuf {
    let helper = Path::new(env!("CARGO_BIN_EXE_nonstopvibin-window"));
    let binary = helper.with_file_name(format!("nonstopvibin{}", env::consts::EXE_SUFFIX));
    assert!(
        binary.is_file(),
        "{binary:?} is missing; build it first with `cargo build -p nsv-cli`"
    );
    binary
}

/// An OpenAI-compatible upstream that lists one model and answers anything else with an
/// empty completion. It lives as long as the test process.
fn stub_upstream() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buffer = [0; 8192];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(buffer.get(..read).unwrap_or_default());
            let body = if request.contains("/models") {
                json!({ "object": "list", "data": [{ "id": "stub-model", "object": "model" }] })
            } else {
                json!({ "id": "stub", "object": "chat.completion", "choices": [] })
            }
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap_or_default();
        }
    });
    port
}

impl Daemon {
    pub(crate) fn serve(name: &str) -> Self {
        let root = env::temp_dir().join(format!("nsv-ui-{name}-{}", std::process::id()));
        fs::remove_dir_all(&root).unwrap_or_default();
        fs::create_dir_all(root.join("home")).unwrap();
        let port = Port::reserve().release();
        let child = Command::new(binary())
            .args(["serve", "--port", &port.to_string()])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.join("home"))
            .env("NONSTOPVIBIN_DATA_DIR", root.join("data"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut daemon = Self {
            child,
            port,
            token: String::new(),
            root,
        };
        let session = daemon.root.join("data/server.json");
        wait_until("serve writes server.json", || {
            let Ok(text) = fs::read_to_string(&session) else {
                return false;
            };
            let session: Value = serde_json::from_str(&text).unwrap();
            if session["pid"] != daemon.child.id() {
                return false;
            }
            session["token"]
                .as_str()
                .unwrap()
                .clone_into(&mut daemon.token);
            true
        });
        daemon
    }

    /// A management call; the status and the JSON answer.
    pub(crate) fn api(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let body = body.map_or_else(String::new, Value::to_string);
        write!(
            stream,
            "{method} /api{path} HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.port,
            self.token,
            body.len()
        )
        .unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        let (head, body) = answer.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    /// A profile named `name` with one API-key account on a stub upstream, running.
    pub(crate) fn running_profile(&self, name: &str) {
        let (status, profile) = self.api("POST", "/profiles", Some(&json!({ "name": name })));
        assert_eq!(status, 201, "{profile}");
        let id = profile["id"].as_str().unwrap();
        let account = json!({
            "name": "Layout fixture",
            "provider": "custom",
            "baseUrl": format!("http://127.0.0.1:{}/v1", stub_upstream()),
            "apiKey": "synthetic-key",
            "prefix": "",
            "models": [{ "id": "stub-model", "protocol": "openai" }],
        });
        let path = format!("/profiles/{id}/api-account");
        let (status, state) = self.api("POST", &path, Some(&account));
        assert_eq!(status, 201, "{state}");
        let (status, started) = self.api("POST", &format!("/profiles/{id}/start"), None);
        assert!(status < 300, "{started}");
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let (_, state) = self.api("GET", "/state", None);
            if state["profiles"][0]["runtime"] == "running" {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the profile did not start: {state}"
            );
            pause(Duration::from_millis(100));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // SIGTERM lets it stop its core; a kill would leave the core running.
        let stopped = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .is_ok_and(|status| status.success());
        let deadline = Instant::now() + TIMEOUT;
        while stopped && Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                break;
            }
            pause(Duration::from_millis(50));
        }
        self.child.kill().unwrap_or_default();
        self.child.wait().unwrap_or_else(|error| panic!("{error}"));
        fs::remove_dir_all(&self.root).unwrap_or_default();
    }
}
