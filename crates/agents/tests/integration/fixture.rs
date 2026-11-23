//! The agent connections main installed (`tests/fixtures/ts-data-dir`, see its README): status
//! recognises them, restore after a port change updates them as main did on launch
//! (`tests/golden/api.json`, "launch changes on disk"), and remove leaves the user's settings.

use std::collections::BTreeMap;
use std::fs::{self, Permissions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use nsv_agents::AgentSetup;
use nsv_domain::error::AppError;
use nsv_domain::registry::Agent;
use nsv_domain::wire::{self, Profile};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::common::{
    Broker, Placeholders, Scratch, changes, directory, home_only, read, run, snapshot,
};

const ORIGINAL_DATA: &str = "/tmp/nsv-ts-fixture/data";
const ORIGINAL_HOME: &str = "/tmp/nsv-ts-fixture/agent-home";
const TEAM: &str = "equipe-cafe-r-d-ete";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/ts-data-dir")
}

fn expected() -> Value {
    serde_json::from_str(&read(fixture().join("expected.json"))).unwrap()
}

fn copy_tree(from: &Path, to: &Path) {
    directory(to, 0o700);
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::write(&target, fs::read(entry.path()).unwrap()).unwrap();
        }
    }
}

/// The fixture's agent connections and agent home, copied with the modes main gave them
/// (Git keeps only the executable bit).
struct Copy {
    root: Scratch,
    data: PathBuf,
    home: PathBuf,
    profiles: Vec<Profile>,
    broker: Broker,
    setup: AgentSetup,
}

impl Copy {
    fn new() -> Self {
        let root = Scratch::new();
        let (data, home) = (root.root.join("data"), root.root.join("agent-home"));
        directory(&data, 0o700);
        copy_tree(
            &fixture().join("data/agent-connections"),
            &data.join("agent-connections"),
        );
        copy_tree(&fixture().join("agent-home"), &home);
        let expected = expected();
        for entry in expected["files"].as_array().unwrap() {
            let path = entry["path"].as_str().unwrap();
            if path.starts_with("data/agent-connections") || path.starts_with("agent-home") {
                let mode = u32::from_str_radix(entry["mode"].as_str().unwrap(), 8).unwrap();
                fs::set_permissions(root.root.join(path), Permissions::from_mode(mode)).unwrap();
            }
        }
        fs::set_permissions(&home, Permissions::from_mode(0o755)).unwrap();
        let profiles: Vec<Profile> = serde_json::from_value(expected["profiles"].clone()).unwrap();
        let broker = Broker::new(
            4318,
            AppError::new("Start this profile before connecting an agent.", 409),
        );
        for profile in &profiles {
            let secret = format!("{}:client", profile.id);
            broker.start(profile, expected["secrets"][&secret].as_str().unwrap());
        }
        let setup = AgentSetup::new(&data, broker.source(), home_only(&home)).unwrap();
        Self {
            root,
            data,
            home,
            profiles,
            broker,
            setup,
        }
    }

    fn profile(&self, slug: &str) -> &Profile {
        self.profiles
            .iter()
            .find(|profile| profile.slug.as_str() == slug)
            .unwrap()
    }

    fn files(&self) -> BTreeMap<String, Value> {
        let mut files = snapshot(&self.home, "<agent-home>");
        files.extend(snapshot(&self.data, "<data>"));
        files
    }

    /// A result main recorded, with the paths of this copy. Claude Code connections keep the
    /// project paths main recorded: their projects did not move with the data.
    fn moved(&self, result: &Value) -> wire::AgentSetup {
        let claude = result["agent"] == "claude";
        let data = self.data.to_string_lossy();
        let home = self.home.to_string_lossy();
        let mut result = result.clone();
        result["command"] = result["command"]
            .as_str()
            .unwrap()
            .replace(ORIGINAL_DATA, &data)
            .into();
        for file in result["files"].as_array_mut().unwrap() {
            let path = file.as_str().unwrap();
            *file = match (path.strip_prefix("data/"), path.strip_prefix("agent-home/")) {
                (Some(rest), _) => format!("{data}/{rest}"),
                (None, Some(rest)) if claude => format!("{ORIGINAL_HOME}/{rest}"),
                (None, Some(rest)) => format!("{home}/{rest}"),
                _ => panic!("{path}"),
            }
            .into();
        }
        if claude {
            // Each result was recorded at its install; both projects are connected now.
            result["projects"] = serde_json::json!([
                format!("{ORIGINAL_HOME}/projects/api server"),
                format!("{ORIGINAL_HOME}/projects/webapp"),
            ]);
        }
        serde_json::from_value(result).unwrap()
    }
}

fn agent(connection: &Value) -> Agent {
    Agent::from_id(connection["input"]["agent"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn typescript_connections_are_recognised_restored_and_removed() {
    let copy = Copy::new();
    let expected = expected();
    let connections = expected["agentConnections"].as_array().unwrap();
    assert_eq!(connections.len(), 6);

    // Recognised as main installed them. The helpers name main's socket directory, so the
    // connections that run one ask for a reconnect until startup rewrites them.
    for connection in connections {
        let profile = copy
            .profiles
            .iter()
            .find(|p| p.id.as_str() == connection["profileId"])
            .unwrap();
        let project = connection["input"]["projectDirectory"].as_str();
        let mut status = copy
            .setup
            .status(profile, agent(connection), project)
            .await
            .unwrap()
            .unwrap();
        let reconnect = status.needs_reconnect.take();
        assert_eq!(status, copy.moved(&connection["result"]), "{connection}");
        assert_eq!(
            reconnect,
            (agent(connection) != Agent::Opencode).then_some(true),
            "{connection}"
        );
    }

    // Startup on another port rewrites what is still exactly main's, as main's launch did, and
    // leaves the Claude Code projects that did not move for the user.
    let before = copy.files();
    let failures = copy.setup.restore(&copy.profiles, 24318).await.unwrap();
    let left = format!(
        "Could not automatically update claude for {TEAM}. Open Connect agents to resolve edited or missing files."
    );
    assert_eq!(failures, [left.clone(), left]);
    let placeholders = Placeholders::new([
        (
            copy.setup.socket_directory().to_string_lossy().into_owned(),
            "<socket-dir>".to_owned(),
        ),
        (
            copy.data.to_string_lossy().into_owned(),
            "<data>".to_owned(),
        ),
        (
            copy.home.to_string_lossy().into_owned(),
            "<agent-home>".to_owned(),
        ),
    ]);
    let golden: Vec<Value> = serde_json::from_str(&read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/api.json"),
    ))
    .unwrap();
    assert_eq!(golden[0]["name"], "launch changes on disk");
    let launched: BTreeMap<String, Value> = golden[0]["output"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(path, _)| {
            path.starts_with("<data>/agent-connections/") || path.starts_with("<agent-home>/")
        })
        .map(|(path, file)| (path.clone(), file.clone()))
        .collect();
    assert_eq!(launched.len(), 11);
    let restored =
        placeholders.apply(&serde_json::to_value(changes(&before, &copy.files())).unwrap());
    assert_eq!(restored, serde_json::to_value(&launched).unwrap());

    // Rewritten connections are current and their helpers answer through this socket.
    copy.broker.set_port(24318);
    for connection in connections.iter().filter(|c| agent(c) != Agent::Claude) {
        let profile = copy
            .profiles
            .iter()
            .find(|p| p.id.as_str() == connection["profileId"])
            .unwrap();
        let status = copy
            .setup
            .status(profile, agent(connection), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.needs_reconnect, None, "{connection}");
    }
    let team = copy.profile(TEAM).clone();
    let helper = copy
        .data
        .join(format!("agent-connections/{TEAM}/codex-key-24318"));
    let output = run(&helper.to_string_lossy(), &[], &copy.root.root).await;
    let key = format!(
        "{}\n",
        expected["secrets"][&format!("{}:client", team.id)]
            .as_str()
            .unwrap()
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), key);

    // Disconnecting removes only what main installed.
    let user_codex = read(copy.home.join(".codex/config.toml"));
    for agent in [Agent::Codex, Agent::Pi, Agent::Opencode] {
        copy.setup.remove(&team, agent, None).await.unwrap();
        assert_eq!(copy.setup.status(&team, agent, None).await.unwrap(), None);
    }
    for project in ["webapp", "api server"] {
        let project = format!("{ORIGINAL_HOME}/projects/{project}");
        copy.setup
            .remove(&team, Agent::Claude, Some(&project))
            .await
            .unwrap();
    }
    assert_eq!(read(copy.home.join(".codex/config.toml")), user_codex);
    let remaining: Vec<String> = copy.files().into_keys().collect();
    assert_eq!(
        remaining,
        [
            "<agent-home>/.codex/config.toml",
            "<agent-home>/.codex/nonstopvibin-personal-2.config.toml",
            "<agent-home>/projects/api server/.claude/settings.local.json",
            "<agent-home>/projects/webapp/.claude/settings.local.json",
            "<data>/agent-connections/personal-2/codex-key-24318",
            "<data>/agent-connections/personal-2/codex-key-4318",
            "<data>/agent-connections/personal-2/codex.json",
        ]
    );
    copy.setup.close().await.unwrap();
}

/// Disconnecting main's Claude Code connection leaves the user's own settings, which were in
/// the file before. The project is moved into the copy (its manifest re-keyed to the new path)
/// so the connection's files are found; everything else is the bytes main wrote.
#[tokio::test]
async fn typescript_claude_connection_disconnects_to_the_users_settings() {
    let copy = Copy::new();
    let team = copy.profile(TEAM).clone();
    let folder = copy.data.join("agent-connections").join(TEAM);
    let old = "claude-4d5871823f0addc331d14a9f";
    let project = copy.home.join("projects/webapp");
    let mut manifest: Value =
        serde_json::from_str(&read(folder.join(format!("{old}.json")))).unwrap();
    manifest["input"]["projectDirectory"] = project.to_string_lossy().into_owned().into();
    let digest = Sha256::digest(project.as_os_str().as_bytes());
    let new: String = digest
        .iter()
        .take(12)
        .fold("claude-".to_owned(), |name, byte| {
            format!("{name}{byte:02x}")
        });
    fs::remove_file(folder.join(format!("{old}.json"))).unwrap();
    fs::write(
        folder.join(format!("{new}.json")),
        format!("{manifest:#}\n"),
    )
    .unwrap();
    fs::rename(
        folder.join(format!("{old}.settings.json")),
        folder.join(format!("{new}.settings.json")),
    )
    .unwrap();

    copy.setup
        .remove(&team, Agent::Claude, Some(&project.to_string_lossy()))
        .await
        .unwrap();
    assert_eq!(
        read(project.join(".claude/settings.local.json")),
        "{\n  \"permissions\": {\n    \"allow\": [\n      \"Bash(ls:*)\"\n    ]\n  }\n}\n"
    );
    assert!(!folder.join(format!("{new}.json")).exists());
    assert!(!folder.join(format!("{new}.settings.json")).exists());
    copy.setup.close().await.unwrap();
}
