//! The behaviours `tests/agent-setup.test.ts` covered beyond the goldens: live helpers, startup
//! restore, migrations, refusals of edits, symbolic links and unsafe paths.

use std::fs::{self, Permissions};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::SystemTime;

use nsv_agents::{AgentDirectories, AgentError, AgentSetup};
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileSlug;
use nsv_domain::registry::Agent;
use nsv_domain::wire::{self, AgentSetupInput, Profile};
use nsv_files::DataDir;
use serde_json::{Value, json};

use crate::common::{Broker, Scratch, directory, home_only, key, profile, read, run, setup};

const AGENTS: [Agent; 4] = [Agent::Pi, Agent::Codex, Agent::Opencode, Agent::Claude];

struct Fixture {
    root: Scratch,
    home: PathBuf,
    project: PathBuf,
    company: Profile,
    personal: Profile,
    broker: Broker,
    setup: AgentSetup,
}

impl Fixture {
    fn new() -> Self {
        let root = Scratch::new();
        let home = root.root.join("home");
        let project = root.root.join("project's $dollar");
        directory(&home, 0o755);
        directory(&project, 0o755);
        let company = profile(
            "6f1f3c7e-3c1b-4d55-9a2e-0d6a5f6b2c11",
            "work",
            "Work's $(touch should-not-exist)",
        );
        let personal = profile(
            "0c9a3a51-8d2e-4f4b-b5b6-5b8f2f3f9e22",
            "personal",
            "Work's $(touch should-not-exist)",
        );
        let broker = Broker::new(4318, AppError::new("Stopped or unknown profile", 500));
        broker.start(&company, &key('A'));
        broker.start(&personal, &key('B'));
        let setup = setup(&root.root, &broker, home_only(&home));
        Self {
            root,
            home,
            project,
            company,
            personal,
            broker,
            setup,
        }
    }

    fn restarted(&self) -> AgentSetup {
        setup(&self.root.root, &self.broker, home_only(&self.home))
    }

    fn input(agent: Agent, project: &Path) -> AgentSetupInput {
        AgentSetupInput {
            agent,
            project_directory: (agent == Agent::Claude)
                .then(|| project.to_string_lossy().into_owned()),
            other_providers: None,
        }
    }

    async fn install_in(
        &self,
        profile: &Profile,
        agent: Agent,
        project: &Path,
        model: &str,
    ) -> Result<wire::AgentSetup, AgentError> {
        let models = vec![model.to_owned(), "second-fixture".to_owned()];
        self.setup
            .install(profile, Self::input(agent, project), models)
            .await
    }

    async fn install(&self, agent: Agent) -> wire::AgentSetup {
        self.install_in(&self.company, agent, &self.project, "fixture-model")
            .await
            .unwrap()
    }

    async fn status(&self, agent: Agent) -> Option<wire::AgentSetup> {
        self.setup.status(&self.company, agent, None).await.unwrap()
    }

    async fn run(&self, helper: &str) -> Output {
        run(helper, &[], &self.project).await
    }

    async fn key_of(&self, helper: &str) -> String {
        let output = self.run(helper).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    async fn refusal_of(&self, helper: &str) -> String {
        let output = self.run(helper).await;
        assert!(!output.status.success());
        String::from_utf8(output.stderr).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Best effort: the socket directory lives outside the scratch directory.
        drop(fs::remove_dir_all(self.setup.socket_directory()));
    }
}

/// A setup's files by what each one is: the agent's configuration and the manifest first,
/// then its key helper and Claude Code's launch settings where it has them.
fn config(setup: &wire::AgentSetup) -> &str {
    &setup.files[0]
}

fn manifest(setup: &wire::AgentSetup) -> &str {
    &setup.files[1]
}

fn helper(setup: &wire::AgentSetup) -> &str {
    setup
        .files
        .iter()
        .find(|file| {
            file.rsplit_once("-key-")
                .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
        })
        .unwrap()
}

fn launch(setup: &wire::AgentSetup) -> &str {
    setup
        .files
        .iter()
        .find(|file| file.ends_with(".settings.json"))
        .unwrap()
}

fn json_file(path: impl AsRef<Path>) -> Value {
    serde_json::from_str(&read(path)).unwrap()
}

fn mode(path: impl AsRef<Path>) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn modified(path: &str) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

fn assert_refused(result: Result<impl std::fmt::Debug, AgentError>, text: &str) {
    match result {
        Err(error) => assert!(
            error.to_string().contains(text),
            "{error} does not say {text}"
        ),
        Ok(value) => panic!("expected a refusal saying {text}, got {value:?}"),
    }
}

#[tokio::test]
async fn installs_keep_secrets_out_of_files_and_helpers_answer_with_live_keys() {
    let f = Fixture::new();
    let selected = "vendor/mo'del\"$(touch should-not-exist)__NV_BASE____NV_KEY__{env:UNTRUSTED}{file:/fixture-do-not-read}";
    for agent in AGENTS {
        let installed = f
            .install_in(&f.company, agent, &f.project, selected)
            .await
            .unwrap();
        assert_eq!(f.status(agent).await, Some(installed.clone()));
        assert_eq!(installed.provider, "nonstopvibin-work");
        for path in &installed.files {
            assert!(!read(path).contains("nv_AAAA"), "{path} holds a key");
            assert_eq!(mode(path) & 0o077, 0, "{path}");
        }
        if agent != Agent::Opencode {
            assert_eq!(mode(helper(&installed)), 0o700);
            assert_eq!(f.key_of(helper(&installed)).await, key('A'));
        }
        if agent == Agent::Claude {
            let settings = json_file(config(&installed));
            let picker = json_file(launch(&installed));
            assert_eq!(
                installed.command,
                format!("claude --settings '{}'", launch(&installed))
            );
            assert_eq!(
                picker["modelPicker"]["options"],
                json!([{ "model": selected }, { "model": "second-fixture" }])
            );
            assert_eq!(
                settings["modelPicker"],
                Value::Null,
                "project settings ignore the picker"
            );
            assert_eq!(picker["env"], settings["env"]);
            let command = settings["apiKeyHelper"].as_str().unwrap();
            let output = run("/bin/sh", &["-c", command], &f.project).await;
            assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), key('A'));
        }
    }
    let entries: Vec<_> = fs::read_dir(&f.project)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        entries,
        [".claude"],
        "catalog and profile strings never execute shell code"
    );

    let other = f
        .install_in(&f.personal, Agent::Codex, &f.project, "fixture-model")
        .await
        .unwrap();
    let work = f.status(Agent::Codex).await.unwrap();
    assert_ne!(
        f.key_of(helper(&work)).await,
        f.key_of(helper(&other)).await
    );
    f.broker.start(&f.personal, &key('C'));
    assert_eq!(
        f.key_of(helper(&other)).await,
        key('C'),
        "a rotated key is answered at once"
    );

    f.broker.set_port(54321);
    let status = f
        .setup
        .status(&f.personal, Agent::Codex, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.needs_reconnect, Some(true));
    assert!(
        f.refusal_of(helper(&other))
            .await
            .contains("gateway port changed")
    );
    let updated = f
        .install_in(&f.personal, Agent::Codex, &f.project, "fixture-model")
        .await
        .unwrap();
    let status = f
        .setup
        .status(&f.personal, Agent::Codex, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.needs_reconnect, None);
    assert_eq!(f.key_of(helper(&updated)).await, key('C'));
    assert!(
        f.refusal_of(helper(&other))
            .await
            .contains("gateway port changed"),
        "sessions still holding the old helper fail closed"
    );
    f.setup.close().await.unwrap();
}

#[tokio::test]
async fn helpers_fail_closed_while_stopped_and_resume_through_a_restarted_socket() {
    let f = Fixture::new();
    let installed = f.install(Agent::Pi).await;
    f.broker.stop(&f.company);
    assert!(
        f.refusal_of(helper(&installed))
            .await
            .contains("Open nonstopvibin and start work.")
    );
    f.broker.start(&f.company, &key('A'));
    f.setup.close().await.unwrap();
    assert!(
        f.refusal_of(helper(&installed))
            .await
            .contains("Open nonstopvibin and start work.")
    );
    let restarted = f.restarted();
    assert_eq!(
        restarted
            .restore(std::slice::from_ref(&f.company), 4318)
            .await
            .unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(f.key_of(helper(&installed)).await, key('A'));
    restarted.close().await.unwrap();
}

#[tokio::test]
async fn status_refuses_a_stopped_profile_even_when_it_needs_reconnecting() {
    let f = Fixture::new();
    for agent in AGENTS {
        let installed = f.install(agent).await;
        let project = (agent == Agent::Claude).then(|| f.project.to_string_lossy().into_owned());
        fs::remove_file(config(&installed)).unwrap();
        f.broker.stop(&f.company);
        assert_refused(
            f.setup.status(&f.company, agent, project.as_deref()).await,
            "Stopped or unknown profile",
        );
        f.broker.start(&f.company, &key('A'));
        let status = f
            .setup
            .status(&f.company, agent, project.as_deref())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.needs_reconnect, Some(true), "{agent:?}");
    }
    f.setup.close().await.unwrap();
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario in the order the TypeScript test ran it"
)]
async fn startup_updates_stale_connections_of_stopped_profiles_and_skips_current_ones() {
    let f = Fixture::new();
    let mut installed = Vec::new();
    for agent in AGENTS {
        installed.push(f.install(agent).await);
    }
    let [pi, codex, _, claude] = installed.as_slice() else {
        panic!()
    };
    let second = f.root.root.join("second-project");
    directory(&second, 0o755);
    let second_claude = f
        .install_in(&f.company, Agent::Claude, &second, "fixture-model")
        .await
        .unwrap();
    let preference = "model = \"user-chosen-model\"\nmodel_reasoning_effort = \"high\"\n";
    fs::write(
        config(codex),
        format!("{preference}{}", read(config(codex))),
    )
    .unwrap();
    for connection in [claude, &second_claude] {
        let mut recorded = json_file(manifest(connection));
        let helper_content = format!(
            "{}\n# previous helper template\n",
            recorded["helperContent"].as_str().unwrap()
        );
        recorded["helperContent"] = helper_content.clone().into();
        fs::write(manifest(connection), recorded.to_string()).unwrap();
        fs::write(helper(connection), helper_content).unwrap();
    }
    let mut settings = json_file(config(claude));
    settings["env"]["USER_SETTING"] = "keep".into();
    settings["permissions"] = json!({ "allow": ["Read"] });
    fs::write(config(claude), settings.to_string()).unwrap();
    let mut old = json_file(manifest(pi));
    old["version"] = 6.into();
    old.as_object_mut().unwrap().remove("helperContent");
    let content = old["content"].as_str().unwrap().replace(
        "supportsMidConvoEffort: false",
        "supportsMidConvoEffort: false, supportsMidConvoToolChanges: false",
    );
    old["content"] = content.clone().into();
    fs::write(config(pi), content).unwrap();
    fs::write(manifest(pi), old.to_string()).unwrap();
    f.setup.close().await.unwrap();

    let restarted = f.restarted();
    assert_eq!(
        restarted
            .restore(std::slice::from_ref(&f.company), 4318)
            .await
            .unwrap(),
        Vec::<String>::new()
    );
    f.broker.stop(&f.company);
    f.broker.set_port(4418);
    let mut renamed = f.company.clone();
    renamed.name = "Renamed profile".to_owned();
    renamed.enabled = false;
    let profiles = [renamed.clone(), f.personal.clone()];
    assert_eq!(
        restarted.restore(&profiles, 4418).await.unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        restarted
            .status(&f.personal, Agent::Pi, None)
            .await
            .unwrap(),
        None,
        "startup installs nothing new"
    );
    f.broker.start(&f.company, &key('A'));
    let pi_helper = helper(
        &restarted
            .status(&renamed, Agent::Pi, None)
            .await
            .unwrap()
            .unwrap(),
    )
    .to_owned();
    f.broker.stop(&f.company);
    assert!(f.refusal_of(&pi_helper).await.contains("start"));
    f.broker.start(&f.company, &key('A'));
    let mut files = Vec::new();
    for agent in AGENTS {
        let current = restarted
            .status(&renamed, agent, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.needs_reconnect, None, "{agent:?}");
        assert_eq!(current.models, ["fixture-model", "second-fixture"]);
        let manifest = json_file(manifest(&current));
        assert_eq!(manifest["port"], 4418);
        if agent != Agent::Opencode {
            assert_eq!(
                manifest["helperContent"].as_str().unwrap(),
                read(helper(&current))
            );
            assert_eq!(f.key_of(helper(&current)).await, key('A'));
        }
        files.extend(current.files);
    }
    let other = restarted
        .status(&renamed, Agent::Claude, Some(&second.to_string_lossy()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(other.needs_reconnect, None);
    files.extend(other.files);
    assert!(read(config(codex)).starts_with(preference));
    let updated = json_file(config(claude));
    assert_eq!(updated["env"]["USER_SETTING"], "keep");
    assert_eq!(updated["permissions"], json!({ "allow": ["Read"] }));
    assert!(!read(config(pi)).contains("supportsMidConvoToolChanges: false"));
    assert!(
        f.refusal_of(helper(pi))
            .await
            .contains("gateway port changed"),
        "old-port sessions fail closed"
    );
    assert_eq!(
        json_file(config(&second_claude))["env"]["ANTHROPIC_BASE_URL"],
        "http://127.0.0.1:4418/p/work"
    );

    let before: Vec<_> = files.iter().map(|file| modified(file)).collect();
    assert_eq!(
        restarted.restore(&profiles, 4418).await.unwrap(),
        Vec::<String>::new()
    );
    let after: Vec<_> = files.iter().map(|file| modified(file)).collect();
    assert_eq!(
        before, after,
        "current connections are not rewritten at each startup"
    );
    restarted.close().await.unwrap();
}

#[tokio::test]
async fn startup_leaves_edited_or_removed_connections_and_updates_the_others() {
    let f = Fixture::new();
    let pi = f.install(Agent::Pi).await;
    let edited = format!("{}\n// user edit\n", read(config(&pi)));
    fs::write(config(&pi), &edited).unwrap();
    let codex = f.install(Agent::Codex).await;
    fs::remove_file(config(&codex)).unwrap();
    let claude = f.install(Agent::Claude).await;
    let edited_helper = format!("{}\n# user edit\n", read(helper(&claude)));
    let second = f.root.root.join("second-project");
    directory(&second, 0o755);
    let second_claude = f
        .install_in(&f.company, Agent::Claude, &second, "fixture-model")
        .await
        .unwrap();
    let mut removed = json_file(config(&second_claude));
    removed["env"]
        .as_object_mut()
        .unwrap()
        .remove("ANTHROPIC_BASE_URL");
    let removed = removed.to_string();
    fs::write(config(&second_claude), &removed).unwrap();
    fs::write(helper(&claude), &edited_helper).unwrap();
    let other = f
        .install_in(&f.personal, Agent::Pi, &f.project, "fixture-model")
        .await
        .unwrap();
    f.setup.close().await.unwrap();

    let restarted = f.restarted();
    let mut company = f.company.clone();
    company.name = "Changed".to_owned();
    let mut personal = f.personal.clone();
    personal.name = "Changed".to_owned();
    let failures = restarted
        .restore(&[company, personal.clone()], 4318)
        .await
        .unwrap();
    assert_eq!(failures.len(), 4, "{failures:?}");
    assert!(
        failures
            .iter()
            .all(|failure| failure.contains("work") && failure.contains("Connect agents"))
    );
    assert_eq!(read(config(&pi)), edited);
    assert!(!Path::new(config(&codex)).exists());
    assert_eq!(read(helper(&claude)), edited_helper);
    assert_eq!(read(config(&second_claude)), removed);
    assert!(read(config(&other)).contains("Changed"));
    assert_eq!(
        restarted
            .status(&personal, Agent::Pi, None)
            .await
            .unwrap()
            .unwrap()
            .needs_reconnect,
        None
    );
    restarted.close().await.unwrap();
}

#[tokio::test]
async fn startup_refuses_deleted_helpers_and_pickers_but_migrates_claude_without_a_picker() {
    let f = Fixture::new();
    let pi = f.install(Agent::Pi).await;
    let claude = f.install(Agent::Claude).await;
    fs::remove_file(helper(&pi)).unwrap();
    fs::remove_file(launch(&claude)).unwrap();
    let files = [
        config(&pi),
        manifest(&pi),
        config(&claude),
        manifest(&claude),
    ];
    let before: Vec<String> = files.iter().map(read).collect();
    assert_eq!(
        f.setup
            .restore(std::slice::from_ref(&f.company), 4318)
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(!Path::new(helper(&pi)).exists());
    assert!(!Path::new(launch(&claude)).exists());
    assert_eq!(files.iter().map(read).collect::<Vec<_>>(), before);
    f.setup.remove(&f.company, Agent::Pi, None).await.unwrap();
    let mut old = json_file(manifest(&claude));
    old["version"] = 2.into();
    fs::write(manifest(&claude), old.to_string()).unwrap();
    assert_eq!(
        f.setup
            .restore(std::slice::from_ref(&f.company), 4318)
            .await
            .unwrap(),
        Vec::<String>::new()
    );
    assert!(Path::new(launch(&claude)).exists());
    f.setup.close().await.unwrap();
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario in the order the TypeScript test ran it"
)]
async fn updates_and_disconnects_keep_preferences_and_refuse_edits_and_symbolic_links() {
    let f = Fixture::new();
    let pi_models = f.home.join(".pi/agent/models.json");
    let existing_pi = "// Existing commented configuration\n{\"providers\":{\"other\":{\"apiKey\":\"synthetic-existing\"}},\"userPreference\":true}";
    crate::common::file(&pi_models, existing_pi, 0o644);
    let claude_file = f.project.join(".claude/settings.local.json");
    let claude_existing = json!({
        "permissions": { "deny": ["Bash(rm *)"] },
        "env": {
            "USER_PREFERENCE": "keep",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "ANTHROPIC_MODEL": "fixture-model",
        },
    });
    crate::common::file(&claude_file, &claude_existing.to_string(), 0o644);
    for agent in [Agent::Pi, Agent::Claude] {
        let installed = f.install(agent).await;
        if agent == Agent::Pi {
            let mut old = json_file(manifest(&installed));
            assert_eq!(old["version"], 7);
            old["version"] = 6.into();
            let content = old["content"].as_str().unwrap().replace(
                "supportsMidConvoEffort: false",
                "supportsMidConvoEffort: false, supportsMidConvoToolChanges: false",
            );
            old["content"] = content.clone().into();
            fs::write(config(&installed), &content).unwrap();
            fs::write(manifest(&installed), old.to_string()).unwrap();
            assert_eq!(f.status(agent).await.unwrap().needs_reconnect, Some(true));
            f.install(agent).await;
            assert_eq!(f.status(agent).await.unwrap().needs_reconnect, None);
            let updated = json_file(manifest(&installed));
            assert_eq!(updated["version"], 7);
            assert!(
                !updated["content"]
                    .as_str()
                    .unwrap()
                    .contains("supportsMidConvoToolChanges: false")
            );
            // A version 4 connection named its file by profile ID.
            let legacy = f.home.join(format!(
                ".pi/agent/extensions/nonstopvibin-{}.js",
                f.company.id
            ));
            fs::write(&legacy, read(config(&installed))).unwrap();
            fs::remove_file(config(&installed)).unwrap();
            let mut old = updated;
            old["version"] = 4.into();
            fs::write(manifest(&installed), old.to_string()).unwrap();
            assert_eq!(f.status(agent).await.unwrap().needs_reconnect, Some(true));
        }
        f.install_in(&f.company, agent, &f.project, "new-model")
            .await
            .unwrap();
        let status = f.status(agent).await.unwrap();
        assert_eq!(status.needs_reconnect, None);
        assert_eq!(status.models, ["new-model", "second-fixture"]);
        f.setup.remove(&f.company, agent, None).await.unwrap();
        assert_eq!(f.status(agent).await, None);
        if agent == Agent::Pi {
            assert!(!Path::new(config(&installed)).exists());
            assert_eq!(read(&pi_models), existing_pi);
        } else {
            assert_eq!(json_file(&claude_file), claude_existing);
        }
        for file in installed.files.iter().skip(1) {
            assert!(!Path::new(file).exists(), "{file}");
        }
    }
    let installed = f.install(Agent::Pi).await;
    let edited = format!("{}\n// User edit\n", read(config(&installed)));
    fs::write(config(&installed), &edited).unwrap();
    assert_refused(
        f.setup.remove(&f.company, Agent::Pi, None).await,
        "edited outside",
    );
    assert_refused(
        f.install_in(&f.company, Agent::Pi, &f.project, "fixture-model")
            .await,
        "edited outside",
    );
    assert_eq!(read(config(&installed)), edited);
    fs::remove_file(config(&installed)).unwrap();
    symlink(&claude_file, config(&installed)).unwrap();
    assert_refused(
        f.install_in(&f.company, Agent::Pi, &f.project, "fixture-model")
            .await,
        "symbolic link",
    );
    assert_eq!(json_file(&claude_file), claude_existing);
    let mut escape = profile("5e0c1a2b-3c4d-4e5f-8a6b-7c8d9e0f1a2b", "escape", "Escape");
    symlink(&f.root.root, f.setup.directory().join("escape")).unwrap();
    assert_refused(
        f.install_in(&escape, Agent::Codex, &f.project, "m").await,
        "without symbolic links",
    );
    escape.slug = ProfileSlug::parse("-escape").unwrap();
    assert_eq!(
        f.install_in(&escape, Agent::Codex, &f.project, "m")
            .await
            .map_err(AppError::from)
            .unwrap_err()
            .status,
        400
    );
    f.setup.close().await.unwrap();
}

#[tokio::test]
async fn pi_extension_migration_keeps_edits_and_deleting_the_file_uninstalls() {
    let f = Fixture::new();
    let installed = f.install(Agent::Pi).await;
    let readable = f.home.join(".pi/agent/extensions/nonstopvibin-work.js");
    let legacy = f.home.join(format!(
        ".pi/agent/extensions/nonstopvibin-{}.js",
        f.company.id
    ));
    assert_eq!(Path::new(config(&installed)), readable);
    let content = read(&readable);
    let mut old = json_file(manifest(&installed));
    old["version"] = 5.into();
    fs::write(manifest(&installed), old.to_string()).unwrap();
    fs::remove_file(&readable).unwrap();
    fs::write(&legacy, format!("{content}\n// user edit")).unwrap();
    assert_refused(
        f.install_in(&f.company, Agent::Pi, &f.project, "fixture-model")
            .await,
        "edited outside",
    );
    assert!(!readable.exists());
    fs::write(&legacy, &content).unwrap();
    fs::write(&readable, "// unrelated extension").unwrap();
    assert_refused(
        f.install_in(&f.company, Agent::Pi, &f.project, "fixture-model")
            .await,
        "edited outside",
    );
    assert_eq!(read(&legacy), content);
    fs::remove_file(&readable).unwrap();
    f.install(Agent::Pi).await;
    assert!(!legacy.exists());
    fs::remove_file(&readable).unwrap(); // Deleting the obvious extension file uninstalls.
    assert_eq!(
        f.status(Agent::Pi).await.unwrap().needs_reconnect,
        Some(true)
    );
    f.setup.remove(&f.company, Agent::Pi, None).await.unwrap();
    assert_eq!(f.status(Agent::Pi).await, None);
    f.setup.close().await.unwrap();
}

#[tokio::test]
async fn special_files_and_shared_roots_are_refused() {
    let f = Fixture::new();
    let codex = f.home.join(".codex");
    directory(&codex, 0o700);
    let fifo = codex.join("nonstopvibin-work.config.toml");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    assert_refused(
        f.install_in(&f.company, Agent::Codex, &f.project, "m")
            .await,
        "ordinary file",
    );
    fs::remove_file(&fifo).unwrap();

    let shared = f.root.root.join("codex-shared");
    directory(&shared, 0o777);
    let custom = AgentSetup::new(
        &DataDir::new(f.root.root.join("other-data")),
        f.broker.source(),
        AgentDirectories {
            moved: [(Agent::Codex, shared)].into(),
            ..home_only(&f.home)
        },
    );
    let input = Fixture::input(Agent::Codex, &f.project);
    assert_refused(
        custom
            .install(&f.company, input, vec!["m".to_owned()])
            .await,
        "directory you own, without symbolic links or shared write access",
    );
    custom.close().await.unwrap();
    drop(fs::remove_dir_all(custom.socket_directory()));
    f.setup.close().await.unwrap();
}

#[tokio::test]
async fn missing_files_are_repaired_and_claude_project_boundaries_are_explicit() {
    let f = Fixture::new();
    let codex = f.install(Agent::Codex).await;
    fs::remove_file(config(&codex)).unwrap();
    assert_eq!(
        f.status(Agent::Codex).await.unwrap().needs_reconnect,
        Some(true)
    );
    f.install(Agent::Codex).await;
    assert_eq!(f.status(Agent::Codex).await.unwrap().needs_reconnect, None);

    let git = f.project.join(".git");
    directory(&git, 0o755);
    for nested in ["src", "..nested"] {
        let nested = f.project.join(nested);
        directory(&nested, 0o755);
        let result = f.install_in(&f.company, Agent::Claude, &nested, "m").await;
        assert_refused(result, "repository root");
    }
    let worktree = f.root.root.join("worktree");
    let metadata = git.join("worktrees/test");
    directory(&worktree, 0o755);
    directory(&metadata, 0o755);
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", metadata.display()),
    )
    .unwrap();
    fs::write(metadata.join("commondir"), "../..\n").unwrap();
    assert_refused(
        f.install_in(&f.company, Agent::Claude, &worktree, "m")
            .await,
        "Git worktree",
    );

    f.install(Agent::Claude).await;
    assert_refused(
        f.install_in(&f.personal, Agent::Claude, &f.project, "m")
            .await,
        "already exists",
    );
    let second = f
        .install_in(&f.company, Agent::Claude, &f.home, "second-model")
        .await
        .unwrap();
    let mut both = vec![
        f.home.to_string_lossy().into_owned(),
        f.project.to_string_lossy().into_owned(),
    ];
    both.sort();
    assert_eq!(second.projects, Some(both));
    let project = f.project.to_string_lossy().into_owned();
    let home = f.home.to_string_lossy().into_owned();
    let models = |status: Option<wire::AgentSetup>| status.unwrap().models;
    assert_eq!(
        models(
            f.setup
                .status(&f.company, Agent::Claude, Some(&project))
                .await
                .unwrap()
        ),
        ["fixture-model", "second-fixture"]
    );
    assert_eq!(
        models(
            f.setup
                .status(&f.company, Agent::Claude, Some(&home))
                .await
                .unwrap()
        ),
        ["second-model", "second-fixture"]
    );
    assert_refused(
        f.setup.remove(&f.company, Agent::Claude, None).await,
        "Choose which Claude project",
    );
    f.setup
        .remove(&f.company, Agent::Claude, Some(&project))
        .await
        .unwrap();
    assert_eq!(
        f.setup
            .status(&f.company, Agent::Claude, Some(&project))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        f.key_of(helper(&second)).await,
        key('A'),
        "the other project's helper stays"
    );
    f.setup
        .remove(&f.company, Agent::Claude, Some(&home))
        .await
        .unwrap();
    assert!(!Path::new(helper(&second)).exists());
    f.setup.close().await.unwrap();
}

#[tokio::test]
async fn a_deleted_claude_project_can_be_disconnected_without_hiding_the_others() {
    let f = Fixture::new();
    let gone = f.root.root.join("gone");
    crate::common::file(
        &gone.join(".claude/settings.local.json"),
        r#"{"env":{"ANTHROPIC_MODEL":"fixture-model"}}"#,
        0o600,
    );
    fs::set_permissions(&gone, Permissions::from_mode(0o755)).unwrap();
    f.install_in(&f.company, Agent::Claude, &gone, "fixture-model")
        .await
        .unwrap();
    let remaining = f.install(Agent::Claude).await;
    fs::remove_dir_all(&gone).unwrap();
    let gone = gone.to_string_lossy().into_owned();
    let status = f.status(Agent::Claude).await.unwrap();
    assert_eq!(status.project_directory.as_deref(), Some(gone.as_str()));
    assert_eq!(status.needs_reconnect, Some(true));
    f.setup
        .remove(&f.company, Agent::Claude, Some(&gone))
        .await
        .unwrap();
    assert_eq!(
        f.setup
            .status(&f.company, Agent::Claude, Some(&gone))
            .await
            .unwrap(),
        None
    );
    let project = f.project.to_string_lossy().into_owned();
    assert_eq!(
        f.status(Agent::Claude).await.unwrap().project_directory,
        Some(project)
    );
    assert_eq!(f.key_of(helper(&remaining)).await, key('A'));
    f.setup.close().await.unwrap();
}

#[tokio::test]
async fn reconnecting_a_single_model_setup_removes_only_its_own_model_overrides() {
    let f = Fixture::new();
    let installed = f.install(Agent::Claude).await;
    let mut old = json_file(manifest(&installed));
    let mut legacy: Value = serde_json::from_str(old["content"].as_str().unwrap()).unwrap();
    let env = legacy["env"].as_object_mut().unwrap();
    for name in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ] {
        env.insert(name.to_owned(), "old-model".into());
    }
    env.remove("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY");
    old["version"] = 1.into();
    old["input"]["model"] = "old-model".into();
    old["input"]["contextWindow"] = 32768.into();
    old["input"]["maxTokens"] = 4096.into();
    old.as_object_mut().unwrap().remove("models");
    old["content"] = legacy.to_string().into();
    fs::write(manifest(&installed), old.to_string()).unwrap();
    let mut user = legacy.clone();
    user["permissions"] = json!({ "deny": ["Bash(rm *)"] });
    user["model"] = "user-chosen-model".into();
    fs::write(config(&installed), user.to_string()).unwrap();
    assert_eq!(
        f.status(Agent::Claude).await.unwrap().needs_reconnect,
        Some(true)
    );
    f.install(Agent::Claude).await;
    let settings = json_file(config(&installed));
    for name in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ] {
        assert_eq!(settings["env"][name], Value::Null, "{name}");
    }
    assert_eq!(
        settings["env"]["CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"],
        "1"
    );
    assert_eq!(settings["model"], "user-chosen-model");
    assert_eq!(f.status(Agent::Claude).await.unwrap().needs_reconnect, None);
    f.setup
        .remove(&f.company, Agent::Claude, None)
        .await
        .unwrap();
    assert_eq!(
        json_file(config(&installed)),
        json!({ "model": "user-chosen-model", "permissions": { "deny": ["Bash(rm *)"] } })
    );
    f.setup.close().await.unwrap();
}

#[tokio::test]
async fn codex_runtime_state_and_model_preferences_survive_without_permitting_routing_edits() {
    let f = Fixture::new();
    let installed = f.install(Agent::Codex).await;
    let connection = read(config(&installed));
    let state = "[hooks.state]\n[hooks.state.\"/fixture/hooks.json:stop:0:0\"]\ntrusted_hash = \"fixture-hash\"\n\n[tui.model_availability_nux]\nfixture-model = true\n";
    let preference = "model = \"second-fixture\"\nmodel_reasoning_effort = \"high\"\n";
    fs::write(
        config(&installed),
        format!("{preference}{connection}{state}"),
    )
    .unwrap();
    assert_eq!(f.status(Agent::Codex).await.unwrap().needs_reconnect, None);
    f.install(Agent::Codex).await;
    assert_eq!(
        read(config(&installed)),
        format!("{preference}{connection}{state}"),
        "an unchanged connection keeps the file"
    );
    f.broker.set_port(4399);
    f.install(Agent::Codex).await;
    let updated = read(config(&installed));
    assert!(
        updated.starts_with(preference)
            && updated.ends_with(state)
            && updated.contains("127.0.0.1:4399")
    );
    fs::write(
        config(&installed),
        updated.replace("/p/work/v1", "/p/personal/v1"),
    )
    .unwrap();
    assert_refused(
        f.setup.status(&f.company, Agent::Codex, None).await,
        "edited outside",
    );
    assert_refused(
        f.setup.remove(&f.company, Agent::Codex, None).await,
        "edited outside",
    );
    assert_refused(
        f.install_in(&f.company, Agent::Codex, &f.project, "m")
            .await,
        "edited outside",
    );
    fs::write(config(&installed), &updated).unwrap();
    f.setup
        .remove(&f.company, Agent::Codex, None)
        .await
        .unwrap();
    assert_eq!(
        read(config(&installed)).trim(),
        format!("{preference}{state}").trim()
    );
    f.setup.close().await.unwrap();
}

#[tokio::test]
async fn claude_picker_sync_repairs_missing_files_and_refuses_edits_without_changing_the_route() {
    let f = Fixture::new();
    let installed = f.install(Agent::Claude).await;
    let picker = launch(&installed).to_owned();
    let input = Fixture::input(Agent::Claude, &f.project);
    f.setup
        .install(
            &f.company,
            input,
            vec!["new-gpt".to_owned(), "new-claude".to_owned()],
        )
        .await
        .unwrap();
    let synced = read(&picker);
    let options = json!([{ "model": "new-gpt" }, { "model": "new-claude" }]);
    assert_eq!(
        serde_json::from_str::<Value>(&synced).unwrap()["modelPicker"]["options"],
        options
    );
    let route = read(config(&installed));
    fs::write(&picker, format!("{synced}\n")).unwrap();
    assert_eq!(
        f.status(Agent::Claude).await.unwrap().needs_reconnect,
        Some(true)
    );
    assert_refused(
        f.install_in(&f.company, Agent::Claude, &f.project, "m")
            .await,
        "edited outside",
    );
    assert_refused(
        f.setup.remove(&f.company, Agent::Claude, None).await,
        "edited outside",
    );
    assert_eq!(read(config(&installed)), route);
    fs::remove_file(&picker).unwrap();
    assert_eq!(
        f.status(Agent::Claude).await.unwrap().needs_reconnect,
        Some(true)
    );
    let mut old = json_file(manifest(&installed));
    old["version"] = 2.into();
    fs::write(manifest(&installed), old.to_string()).unwrap();
    assert_eq!(
        f.status(Agent::Claude).await.unwrap().needs_reconnect,
        Some(true)
    );
    f.install(Agent::Claude).await;
    assert_eq!(f.status(Agent::Claude).await.unwrap().needs_reconnect, None);
    fs::remove_file(&picker).unwrap();
    symlink(config(&installed), &picker).unwrap();
    assert_refused(
        f.install_in(&f.company, Agent::Claude, &f.project, "m")
            .await,
        "symbolic link",
    );
    assert_eq!(read(config(&installed)), route);
    f.setup.close().await.unwrap();
}

#[tokio::test]
async fn slugs_from_older_truncation_rules_still_connect() {
    let f = Fixture::new();
    for slug in [
        format!("{}-", "a".repeat(39)),
        format!("{}--2", "a".repeat(39)),
    ] {
        let mut legacy = f.company.clone();
        legacy.slug = ProfileSlug::parse(&slug).unwrap();
        let input = Fixture::input(Agent::Codex, &f.project);
        let result = f
            .setup
            .install(&legacy, input, vec!["fixture-model".to_owned()])
            .await
            .unwrap();
        assert!(result.command.contains(&slug));
        assert!(result.files.iter().any(|file| file.contains(&slug)));
        f.setup.remove(&legacy, Agent::Codex, None).await.unwrap();
    }
    f.setup.close().await.unwrap();
}

#[tokio::test]
async fn a_second_change_while_one_runs_is_refused() {
    let f = Fixture::new();
    let (first, second) = tokio::join!(
        f.install(Agent::Codex),
        f.install_in(&f.company, Agent::Pi, &f.project, "m")
    );
    // `join!` polls the first change until it waits on its file work, holding the flag.
    assert_eq!(first.agent, Agent::Codex);
    let refused = AppError::from(second.unwrap_err());
    assert_eq!(refused.status, 409);
    assert_eq!(
        refused.to_string(),
        "Agent setup is already changing. Try again."
    );
    f.setup.close().await.unwrap();
}
