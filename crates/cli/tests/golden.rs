//! `tests/golden/cli.json`'s command-line cases against the built `nonstopvibin`, set up as the
//! recorder set them up: a temporary HOME and data directory, a stub `systemctl` that answers
//! as a reachable user manager with the unit inactive, a daemon stub on 24319 that answers
//! `GET /api/state` for one token, and nothing on 24399.
//!
//! Every case must match.
//!
//! Linux only: the recording is the Linux headless CLI. Its `status` names the systemd unit and
//! its `service` commands drive systemd, and on macOS a bare launch opens the desktop app.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

const REPOSITORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const STATE_PORT: u16 = 24319;
const STALE_PORT: u16 = 24399;
const TOKEN: &str = "synthetic-session-token";

struct Root(PathBuf);

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Answers `GET /api/state` with this token, as a running daemon would; 401 otherwise.
fn state_stub() {
    let listener = TcpListener::bind(("127.0.0.1", STATE_PORT)).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                request.push(byte[0]);
            }
            let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
            let authorized = text.starts_with("get /api/state ")
                && text.contains(&format!("\r\nauthorization: bearer {TOKEN}\r\n"));
            let (status, body) = if authorized {
                (
                    "200 OK",
                    json!({ "version": "0.2.0", "coreVersion": "8.1.0", "profiles": [] })
                        .to_string(),
                )
            } else {
                ("401 Unauthorized", "{}".to_owned())
            };
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
}

fn session(port: u16) -> Value {
    json!({
        "pid": 4242, "port": port, "token": TOKEN, "allowHosts": [],
        "version": "0.2.0", "startedAt": "2026-11-22T11:00:00.000Z",
    })
}

fn write_session(data: &Path, text: &str, mode: u32) {
    std::fs::create_dir_all(data).unwrap();
    std::fs::set_permissions(data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = data.join("server.json");
    std::fs::write(&file, text).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// What the recorder did to the data directory before a case.
fn prepare(name: &str, root: &Path, units: &HashMap<String, String>) {
    let data = root.join("data");
    match name {
        "cli status stale" => write_session(&data, &session(STALE_PORT).to_string(), 0o600),
        "cli status wrong token" => {
            let mut wrong = session(STATE_PORT);
            wrong["token"] = json!("other");
            write_session(&data, &wrong.to_string(), 0o600);
        }
        "cli status malformed server.json" => write_session(&data, "{not json", 0o600),
        "cli status invalid server.json" => {
            let mut invalid = session(STATE_PORT);
            invalid["port"] = json!("x");
            write_session(&data, &invalid.to_string(), 0o600);
        }
        "cli status invalid allow host in server.json" => {
            let mut invalid = session(STATE_PORT);
            invalid["allowHosts"] = json!(["Bad Host"]);
            write_session(&data, &invalid.to_string(), 0o600);
        }
        "cli status group-readable server.json" => {
            write_session(&data, &session(STATE_PORT).to_string(), 0o644);
        }
        "cli status running" => {
            let mut running = session(STATE_PORT);
            running["allowHosts"] = json!(["myserver.tailnet.ts.net", "other.example.com"]);
            write_session(&data, &running.to_string(), 0o600);
        }
        "cli service unit same executable" => {
            let directory = root.join("home/.config/systemd/user");
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("nonstopvibin.service"), &units["plain"]).unwrap();
        }
        _ => {}
    }
}

fn run(root: &Path, args: &[String], environment: &serde_json::Map<String, Value>) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nonstopvibin"));
    command
        .args(args)
        .current_dir(root)
        .env_clear()
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .env("HOME", root.join("home"))
        .env("NONSTOPVIBIN_DATA_DIR", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("home/.config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"));
    for (key, value) in environment {
        command.env(key, value.as_str().unwrap());
    }
    let output = command.output().unwrap();
    let text =
        |bytes: &[u8]| String::from_utf8_lossy(bytes).replace(&*root.to_string_lossy(), "<root>");
    json!({
        "code": output.status.code().unwrap(),
        "stdout": text(&output.stdout),
        "stderr": text(&output.stderr),
    })
}

#[test]
fn the_cli_answers_as_the_typescript_cli_did() {
    let root = Root(std::env::temp_dir().join(format!("nsv-cli-golden-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&root.0);
    std::fs::create_dir_all(root.0.join("home")).unwrap();
    std::fs::create_dir_all(root.0.join("bin")).unwrap();
    let systemctl = root.0.join("bin/systemctl");
    std::fs::write(
        &systemctl,
        format!(
            "#!/bin/sh\necho \"$@\" >> \"{}/systemctl.log\"\ncase \"$*\" in *is-active*) echo inactive; exit 3;; *show-environment*) exit 0;; *) exit 0;; esac\n",
            root.0.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o755)).unwrap();
    state_stub();

    let cases: Vec<Value> = serde_json::from_slice(
        &std::fs::read(Path::new(REPOSITORY).join("tests/golden/cli.json")).unwrap(),
    )
    .unwrap();
    let plain = cases
        .iter()
        .find(|case| case["name"] == "renderUnit plain")
        .unwrap()["output"]
        .as_str()
        .unwrap()
        .replace(
            "/home/u/.local/bin/nonstopvibin",
            "/opt/nonstopvibin/nonstopvibin",
        );
    let units = HashMap::from([("plain".to_owned(), plain)]);

    let mut total = 0;
    let mut differing = Vec::new();
    for case in cases
        .iter()
        .filter(|case| case["name"].as_str().unwrap().starts_with("cli "))
    {
        total += 1;
        let name = case["name"].as_str().unwrap();
        prepare(name, &root.0, &units);
        let args: Vec<String> = case["input"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap().to_owned())
            .collect();
        let output = run(
            &root.0,
            &args,
            case["input"]["environment"].as_object().unwrap(),
        );
        if output != case["output"] {
            differing.push(format!("{name}: {output}"));
        }
    }
    #[expect(clippy::print_stderr, reason = "the replay's tally is its report")]
    {
        eprintln!(
            "cli.json: {}/{total} command-line cases match",
            total - differing.len()
        );
    }
    assert!(
        differing.is_empty(),
        "unexpected differences:\n{}",
        differing.join("\n")
    );
}
