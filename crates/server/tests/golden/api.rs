//! `tests/golden/api.json` against the running daemon: the TypeScript `Application` on a copy
//! of `tests/fixtures/ts-data-dir`, with no core binary, recorded over raw HTTP. The daemon
//! runs the same way here: port 24318, the fixture copied with its modes, a tiny client
//! directory, the recorder's clock and models.dev payload.
//!
//! Every case must match.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use nsv_agents::{AgentDirectories, AgentSetup};
use nsv_server::{CoreOptions, Daemon, Options, processes};
use nsv_store::ServerLock;
use serde_json::{Value, json};

use crate::common::{ModelsDev, Placeholders, REPOSITORY, TempDir, golden, now, send};

const PORT: u16 = 24318;

/// Text files under `root`, path to content and mode; SQLite files and the vault key by type.
fn tree(root: &Path, label: &str, files: &mut BTreeMap<String, Value>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = entries.map(Result::unwrap).collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let key = format!("{label}/{}", entry.file_name().to_string_lossy());
        let status = fs::symlink_metadata(&path).unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if status.is_dir() {
            tree(&path, &key, files);
        } else if name.ends_with(".sqlite")
            || name.ends_with(".sqlite-wal")
            || name.ends_with(".sqlite-shm")
            || name.ends_with("vault.key")
        {
            files.insert(key, json!({ "type": "binary" }));
        } else if status.is_file() {
            files.insert(
                key,
                json!({
                    "content": String::from_utf8_lossy(&fs::read(&path).unwrap()),
                    "mode": format!("0{:o}", status.permissions().mode() & 0o777),
                }),
            );
        } else {
            let kind = if status.file_type().is_socket() {
                "socket"
            } else {
                "other"
            };
            files.insert(key, json!({ "type": kind }));
        }
    }
}

fn snapshot(root: &Path) -> BTreeMap<String, Value> {
    let mut files = BTreeMap::new();
    tree(&root.join("data"), "<data>", &mut files);
    tree(&root.join("agent-home"), "<agent-home>", &mut files);
    files
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The fixture with the modes and empty directories git does not keep.
fn fixture(root: &Path) {
    let fixture = Path::new(REPOSITORY).join("tests/fixtures/ts-data-dir");
    copy_tree(&fixture.join("data"), &root.join("data"));
    copy_tree(&fixture.join("agent-home"), &root.join("agent-home"));
    let expected: Value =
        serde_json::from_slice(&fs::read(fixture.join("expected.json")).unwrap()).unwrap();
    for file in expected["files"].as_array().unwrap() {
        let path = root.join(file["path"].as_str().unwrap());
        if file["type"] == "directory" {
            fs::create_dir_all(&path).unwrap();
        }
        let mode = u32::from_str_radix(file["mode"].as_str().unwrap(), 8).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }
}

/// Import sources under the home the daemon scans: synthetic `CLIProxyAPI` files only.
fn import_sources(home: &Path) {
    let cli = home.join(".cli-proxy-api");
    fs::create_dir_all(&cli).unwrap();
    let write = |path: &Path, content: &str| fs::write(path, content).unwrap();
    write(
        &cli.join("claude-one.json"),
        &json!({
            "type": "claude", "email": "ada@example.com",
            "account_uuid": "AAAAAAAA-0000-4000-8000-000000000001",
            "organization_uuid": "bbbbbbbb-0000-4000-8000-000000000001",
            "organization_name": "Acme",
        })
        .to_string(),
    );
    write(
        &cli.join("codex-one.json"),
        &json!({ "type": "codex", "email": "ada@example.com" }).to_string(),
    );
    write(
        &cli.join("gemini.json"),
        &json!({ "type": "gemini-cli", "email": "x@example.com" }).to_string(),
    );
    write(&cli.join("broken.json"), "{");
    write(&cli.join("notes.txt"), "ignored");
    let gui = home.join(".local/share/com.cpa.gui/oauth");
    fs::create_dir_all(&gui).unwrap();
    write(
        &gui.join("kimi.json"),
        &json!({ "type": "kimi" }).to_string(),
    );
}

/// The recorder's client: an index, fingerprinted assets and odd files, all from one instant.
fn client(root: &Path) {
    let client = root.join("client");
    fs::create_dir_all(client.join("folder.d")).unwrap();
    for (path, content) in [
        ("index.html", "<!doctype html><title>golden</title>"),
        ("assets/index-AbCd1234.js", "console.log(1);"),
        ("assets/index-AbCd1234.css", "body{}"),
        ("assets/font-Zz_-9999.woff2", "font"),
        ("assets/short-abc.js", "short hash"),
        ("assets/nested/deep-AbCd1234.js", "nested"),
        ("logo.svg", "<svg/>"),
        ("icon.png", "png"),
        ("data.json", "{}"),
        ("file with space.txt", "space"),
        ("unknown.xyz", "x"),
    ] {
        let file = client.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, content).unwrap();
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
            .unwrap();
    }
    fs::write(root.join("secret.txt"), "outside the client").unwrap();
    fs::write(root.join("client-evil.txt"), "sibling prefix").unwrap();
}

/// The recorder's body for `<N bytes: {"name":"x…x"}>`.
fn body(input: &Value) -> Vec<u8> {
    let Some(text) = input["body"].as_str() else {
        return Vec::new();
    };
    if let Some(size) = text
        .strip_prefix('<')
        .and_then(|rest| rest.split_once(" bytes"))
        .and_then(|(size, _)| size.parse::<usize>().ok())
    {
        let filler = size - r#"{"name":""}"#.len();
        return format!(r#"{{"name":"{}"}}"#, "x".repeat(filler)).into_bytes();
    }
    text.as_bytes().to_vec()
}

/// Headers that frame a response rather than describe it: Bun chunked every JSON answer,
/// hyper sends a length; `date` and `keep-alive` vary.
fn described(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "date"
                    | "etag"
                    | "keep-alive"
                    | "connection"
                    | "transfer-encoding"
                    | "content-length"
            )
        })
        .cloned()
        .collect()
}

/// Import ids hash each file's path, which the recorder's scratch directory fixed.
fn without_import_ids(value: &mut Value) {
    if let Some(sources) = value.as_array_mut() {
        for source in sources {
            if let Some(id) = source.get_mut("id") {
                *id = json!("<id>");
            }
        }
    }
}

fn pairs(value: &Value) -> Vec<(String, String)> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            (
                pair[0].as_str().unwrap().to_owned(),
                pair[1].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// The daemon on the fixture, as the recorder started the `Application`.
async fn launch(root: &Path) -> (Daemon<ModelsDev>, Placeholders) {
    let (data, agent_home) = (root.join("data"), root.join("agent-home"));
    let agents = AgentDirectories {
        home: agent_home.clone(),
        codex: None,
        opencode: None,
        pi: None,
    };
    let options = Options {
        directory: data.clone(),
        port: PORT,
        allow_hosts: Vec::new(),
        development: false,
        desktop: false,
        client: root.join("client"),
        core: CoreOptions {
            binary: root.join("no-core"),
            pinned: false,
            feed: None,
            allow_missing: true,
        },
        agents: agents.clone(),
        backends: processes(),
        fetch: ModelsDev::new(),
        clock: now,
    };
    let lock = ServerLock::acquire(&data).unwrap();
    let daemon = Daemon::start(options, lock).await.unwrap();
    daemon.restore().await.unwrap();
    let refuse = |_: &_| Err(nsv_domain::error::AppError::new("unused", 500));
    let socket = AgentSetup::new(&data, Arc::new(refuse), agents)
        .unwrap()
        .socket_directory()
        .to_path_buf();
    let mut placeholders = Placeholders::default();
    let session = daemon.session().unwrap();
    placeholders.add(session.token.expose_secret(), "<token>");
    placeholders.add(socket.to_string_lossy(), "<socket-dir>");
    placeholders.add(data.to_string_lossy(), "<data>");
    placeholders.add(agent_home.to_string_lossy(), "<agent-home>");
    placeholders.add(root.to_string_lossy(), "<root>");
    (daemon, placeholders)
}

/// What launch changed on disk. The lock and `server.json` are the daemon's own here; the
/// TypeScript CLI, not its `Application`, wrote them.
fn launched(before: &BTreeMap<String, Value>, after: &BTreeMap<String, Value>) -> Value {
    let mut changed = serde_json::Map::new();
    for (path, file) in after {
        if before.get(path) != Some(file) {
            changed.insert(path.clone(), file.clone());
        }
    }
    for path in before.keys().filter(|path| !after.contains_key(*path)) {
        changed.insert(path.clone(), Value::Null);
    }
    changed.retain(|path, _| {
        !path.starts_with("<data>/server.json") && !path.starts_with("<data>/server.lock.sqlite")
    });
    Value::Object(changed)
}

/// Sends one recorded request; a difference is described for the report.
async fn replay(case: &Value, placeholders: &mut Placeholders) -> Result<(), String> {
    let (name, input, expected) = (
        case["name"].as_str().unwrap(),
        &case["input"],
        &case["output"],
    );
    let headers: Vec<(String, String)> = pairs(&input["headers"])
        .into_iter()
        .map(|(key, value)| (key, placeholders.fill(&value)))
        .collect();
    let response = send(
        PORT,
        input["method"].as_str().unwrap(),
        input["target"].as_str().unwrap(),
        &headers,
        &body(input),
    )
    .await;
    // Every static ETag starts with the launch's asset version.
    if let Some(etag) = response.header("etag")
        && !placeholders.has("<asset-version>")
    {
        let version = etag.trim_start_matches("W/\"").get(..36).unwrap();
        placeholders.add(version, "<asset-version>");
    }
    let mut actual = serde_json::from_slice(&response.body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&response.body).into_owned()));
    let mut wanted = expected["body"].clone();
    if name == "GET import-sources" {
        without_import_ids(&mut actual);
        without_import_ids(&mut wanted);
    }
    let etag = response.header("etag").map(|etag| placeholders.text(etag));
    let same = u64::from(response.status) == expected["status"].as_u64().unwrap()
        && described(&response.headers) == described(&pairs(&expected["headers"]))
        && etag.as_deref() == expected["etag"].as_str()
        && placeholders.json(&actual) == wanted;
    if same {
        Ok(())
    } else {
        let text = placeholders.text(&String::from_utf8_lossy(&response.body));
        Err(format!(
            "{} {:?} {}",
            response.status,
            described(&response.headers),
            text.chars().take(400).collect::<String>()
        ))
    }
}

#[tokio::test]
async fn the_daemon_answers_as_the_typescript_application_did() {
    let root = TempDir::new("golden-api");
    let root = root.path();
    fixture(root);
    import_sources(&root.join("agent-home"));
    client(root);
    let before = snapshot(root);
    let (daemon, mut placeholders) = launch(root).await;
    let launch = placeholders.json(&launched(&before, &snapshot(root)));

    // The catalog loads in the background on the first priced read: the recorder's warm-up.
    let token = format!("Bearer {}", daemon.session().unwrap().token.expose_secret());
    let usage = "/api/profiles/6e4156ab-2acf-4d89-9a81-264f3b3da50a/usage";
    let warm = send(
        PORT,
        "GET",
        usage,
        &[("Authorization".to_owned(), token)],
        b"",
    )
    .await;
    assert_eq!(warm.status, 200);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let cases = golden("api");
    let mut passed = 0;
    let mut differing = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let outcome = if name == "launch changes on disk" {
            if launch == case["output"] {
                Ok(())
            } else {
                Err(launch.to_string())
            }
        } else {
            replay(case, &mut placeholders).await
        };
        match outcome {
            Ok(()) => passed += 1,
            Err(difference) => differing.push(format!("{name}: {difference}")),
        }
    }
    daemon.close().await.unwrap();
    check(passed, cases.len(), &differing);
}

/// Every case matches.
fn check(passed: usize, total: usize, differing: &[String]) {
    #[expect(clippy::print_stderr, reason = "the replay's tally is its report")]
    {
        eprintln!("api.json: {passed}/{total} match");
    }
    assert!(
        differing.is_empty(),
        "unexpected differences:\n{}",
        differing.join("\n")
    );
}
