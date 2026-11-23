//! Replays `tests/golden/agents.json`, recorded from the TypeScript server's `AgentSetup` (see
//! `tests/golden/README.md` and `tests/golden/areas/agents.ts`): each scenario's results,
//! errors and file changes, the credential socket's raw replies and its key helper, the offered
//! models and catalog entries, the setup input rules and pi's controls source.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nsv_agents::{
    AgentDirectories, AgentError, AgentSetup, Connection, catalog_entries, offered_models,
    validate_setup,
};
use nsv_domain::error::AppError;
use nsv_domain::registry::Agent;
use nsv_domain::secret::ClientKey;
use nsv_domain::wire::{AgentSetupInput, CatalogModel, Model, Profile};
use nsv_files::DataDir;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::common::{Placeholders, Scratch, changes, directory, file, key, run, snapshot};

const WORK: &str = "11111111-1111-4111-8111-111111111111";
const PERSONAL: &str = "22222222-2222-4222-8222-222222222222";

fn cases() -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/agents.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn work() -> Profile {
    serde_json::from_value(json!({
        "id": WORK,
        "slug": "work",
        "name": "Work: R&D 'quoted'",
        "strategy": "fill-first",
        "sessionAffinity": true,
        "enabled": true,
        "createdAt": "2026-01-01T00:00:00.000Z",
    }))
    .unwrap()
}

fn personal() -> Profile {
    let mut profile = work();
    profile.id = nsv_domain::id::ProfileId::parse(PERSONAL).unwrap();
    profile.slug = nsv_domain::id::ProfileSlug::parse("personal").unwrap();
    profile.name = "Personal".to_owned();
    profile
}

/// The golden error shape: a message the user sees, or only the status for zod and Node
/// errors, whose text is not behaviour.
fn error_matches(expected: &Value, actual: &AppError) -> bool {
    let status = expected["status"].as_u64() == Some(u64::from(actual.status));
    match (&expected["message"], &expected["thrown"]) {
        (Value::String(message), Value::Null) => status && *message == actual.to_string(),
        _ => status,
    }
}

fn short_hash(text: &str) -> String {
    let mut hex = nsv_domain::sha256_hex(text.as_bytes());
    hex.truncate(24);
    hex
}

#[derive(Clone, Copy)]
struct Gateway {
    port: u16,
    running: bool,
}

struct Scenario<'a> {
    name: &'a str,
    input: &'a Value,
    root: Scratch,
    home: PathBuf,
    data: PathBuf,
    gateway: Arc<Mutex<Gateway>>,
    setup: AgentSetup,
}

impl<'a> Scenario<'a> {
    fn new(name: &'a str, input: &'a Value) -> Self {
        let root = Scratch::new();
        let (home, data) = (root.root.join("home"), root.root.join("data"));
        directory(&home, 0o755);
        directory(&data, 0o700);
        for path in input["directories"].as_array().into_iter().flatten() {
            directory(&home.join(path.as_str().unwrap()), 0o755);
        }
        for (path, content) in input["files"].as_object().into_iter().flatten() {
            file(&home.join(path), content.as_str().unwrap(), 0o644);
        }
        let moved = |agent: &str| input["moved"][agent].as_str().map(|path| home.join(path));
        let gateway = Arc::new(Mutex::new(Gateway {
            port: 4318,
            running: true,
        }));
        let state = Arc::clone(&gateway);
        let setup = AgentSetup::new(
            &DataDir::new(&data),
            Arc::new(move |id| {
                let Gateway { port, running } = *state.lock().unwrap();
                match id.as_str() {
                    WORK if running => Ok(Connection {
                        key: ClientKey::new(key('a')),
                        port,
                    }),
                    PERSONAL if running => Ok(Connection {
                        key: ClientKey::new(key('b')),
                        port,
                    }),
                    _ => Err(AppError::new(
                        "Start this profile before connecting an agent.",
                        409,
                    )),
                }
            }),
            AgentDirectories {
                home: home.clone(),
                codex: moved("codex"),
                opencode: moved("opencode"),
                pi: moved("pi"),
            },
        );
        Self {
            name,
            input,
            root,
            home,
            data,
            gateway,
            setup,
        }
    }

    /// The goldens' placeholders, and each Claude project's manifest hash: the recorder
    /// hashed its own scratch path.
    fn placeholders(&self) -> Placeholders {
        let recorded = format!(
            "/tmp/nsv-golden/agents/{}/home",
            self.name.replace(' ', "-")
        );
        let home = self.home.to_string_lossy().into_owned();
        let mut pairs = vec![
            (
                self.setup.socket_directory().to_string_lossy().into_owned(),
                "<socket-dir>".to_owned(),
            ),
            (
                self.data.to_string_lossy().into_owned(),
                "<data>".to_owned(),
            ),
            (home.clone(), "<home>".to_owned()),
        ];
        for step in self.input["steps"].as_array().unwrap() {
            let project = step["input"]["projectDirectory"]
                .as_str()
                .or(step["project"].as_str());
            if let Some(rest) = project.and_then(|project| project.strip_prefix("<home>")) {
                pairs.push((
                    short_hash(&format!("{home}{rest}")),
                    short_hash(&format!("{recorded}{rest}")),
                ));
            }
        }
        Placeholders::new(pairs)
    }

    fn files(&self) -> BTreeMap<String, Value> {
        let mut files = snapshot(&self.home, "<home>");
        files.extend(snapshot(&self.data, "<data>"));
        files
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let path = if path.contains("<claude-manifest>") {
            let folder = self.data.join("agent-connections/work");
            let manifest = fs::read_dir(folder)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .find(|name| {
                    name.starts_with("claude-")
                        && name.len() == 36
                        && Path::new(name).extension().is_some_and(|e| e == "json")
                })
                .unwrap();
            path.replace("<claude-manifest>", manifest.trim_end_matches(".json"))
        } else {
            path.to_owned()
        };
        match path.strip_prefix("<data>/") {
            Some(rest) => self.data.join(rest),
            None => self.home.join(path),
        }
    }

    fn project(&self, step: &Value) -> Option<String> {
        let project = step["project"].as_str()?;
        Some(
            self.home
                .join(project.trim_start_matches("<home>/"))
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// Runs one step: its result as JSON, or the error.
    async fn step(&self, step: &Value) -> Result<Value, AgentError> {
        let profile = if step["profile"] == "other" {
            personal()
        } else {
            work()
        };
        let agent = || Agent::from_id(step["agent"].as_str().unwrap()).unwrap();
        match step["op"].as_str().unwrap() {
            "install" => {
                let input = step["input"]
                    .to_string()
                    .replace("<home>", &self.home.to_string_lossy());
                // The route's body parser refuses what serde cannot read, with a 400.
                let Ok(input) = serde_json::from_str::<AgentSetupInput>(&input) else {
                    return Err(AgentError::Invalid("body".to_owned()));
                };
                let models = serde_json::from_value(step["models"].clone()).unwrap();
                let installed = self.setup.install(&profile, input, models).await?;
                Ok(serde_json::to_value(installed).unwrap())
            }
            "status" => {
                let status = self
                    .setup
                    .status(&profile, agent(), self.project(step).as_deref())
                    .await?;
                Ok(serde_json::to_value(status).unwrap())
            }
            "remove" => {
                self.setup
                    .remove(&profile, agent(), self.project(step).as_deref())
                    .await?;
                Ok(Value::Null)
            }
            "write" => {
                let path = self.resolve(step["path"].as_str().unwrap());
                if path.ends_with(".mode") {
                    let mode = u32::try_from(step["mode"].as_u64().unwrap()).unwrap();
                    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(mode))
                        .unwrap();
                } else {
                    fs::write(path, step["content"].as_str().unwrap()).unwrap();
                }
                Ok(Value::Null)
            }
            "delete" => {
                fs::remove_file(self.resolve(step["path"].as_str().unwrap())).unwrap();
                Ok(Value::Null)
            }
            "copy" => {
                let content = fs::read(self.resolve(step["from"].as_str().unwrap())).unwrap();
                fs::write(self.resolve(step["to"].as_str().unwrap()), content).unwrap();
                Ok(Value::Null)
            }
            "addKey" => {
                let path = self.resolve(step["path"].as_str().unwrap());
                let mut current: Map<String, Value> =
                    serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
                current.insert(
                    step["key"].as_str().unwrap().to_owned(),
                    step["value"].clone(),
                );
                fs::write(path, format!("{:#}\n", Value::Object(current))).unwrap();
                Ok(Value::Null)
            }
            "port" => {
                self.gateway.lock().unwrap().port =
                    u16::try_from(step["port"].as_u64().unwrap()).unwrap();
                Ok(Value::Null)
            }
            "stop" | "start" => {
                self.gateway.lock().unwrap().running = step["op"] == "start";
                Ok(Value::Null)
            }
            other => panic!("unknown step {other}"),
        }
    }
}

/// Replays one scenario; each mismatch is described.
async fn scenario(case: &Value) -> Vec<String> {
    let name = case["name"]
        .as_str()
        .unwrap()
        .strip_prefix("scenario ")
        .unwrap();
    let scenario = Scenario::new(name, &case["input"]);
    let placeholders = scenario.placeholders();
    let mut before = scenario.files();
    let mut mismatches = Vec::new();
    let steps = case["input"]["steps"].as_array().unwrap();
    for (index, (step, expected)) in steps
        .iter()
        .zip(case["output"].as_array().unwrap())
        .enumerate()
    {
        let outcome = scenario.step(step).await.map_err(AppError::from);
        let after = scenario.files();
        let changed = placeholders.apply(&json!(changes(&before, &after)));
        before = after;
        let label = format!("{name} step {index} {step}");
        match (&outcome, &expected["error"]) {
            (Err(error), Value::Object(_)) if error_matches(&expected["error"], error) => {}
            (Ok(result), Value::Null) if placeholders.apply(result) == expected["result"] => {}
            _ => mismatches.push(format!(
                "{label}: expected {} got {outcome:?}",
                if expected["error"].is_null() {
                    &expected["result"]
                } else {
                    &expected["error"]
                }
            )),
        }
        if changed != expected["changes"] {
            let expected = expected["changes"].as_object().unwrap();
            let actual = changed.as_object().unwrap();
            let paths: std::collections::BTreeSet<&String> =
                expected.keys().chain(actual.keys()).collect();
            for path in paths {
                if expected.get(path) != actual.get(path) {
                    mismatches.push(format!(
                        "{label}: {path}\n--- expected\n{}\n--- actual\n{}",
                        expected
                            .get(path)
                            .map_or("(unchanged)".to_owned(), |file| file["content"].to_string()),
                        actual
                            .get(path)
                            .map_or("(unchanged)".to_owned(), |file| file["content"].to_string()),
                    ));
                }
            }
        }
    }
    scenario.setup.close().await.unwrap();
    drop(scenario.root);
    mismatches
}

/// The credential socket's raw replies, then the generated key helper run against it.
async fn credential_socket(cases: &[&Value]) -> Vec<String> {
    let root = Scratch::new();
    let (home, data) = (root.root.join("home"), root.root.join("data"));
    directory(&home, 0o755);
    directory(&data, 0o700);
    let gateway = Arc::new(Mutex::new(Gateway {
        port: 4318,
        running: true,
    }));
    let state = Arc::clone(&gateway);
    let setup = AgentSetup::new(
        &DataDir::new(&data),
        Arc::new(move |id| {
            let Gateway { port, running } = *state.lock().unwrap();
            if running && id.as_str() == WORK {
                Ok(Connection {
                    key: ClientKey::new(key('a')),
                    port,
                })
            } else {
                Err(AppError::new("stopped", 409))
            }
        }),
        crate::common::home_only(&home),
    );
    let installed = setup
        .install(
            &work(),
            AgentSetupInput {
                agent: Agent::Codex,
                project_directory: None,
                other_providers: None,
            },
            vec!["gpt-5".to_owned()],
        )
        .await
        .unwrap();
    let helper = installed
        .files
        .iter()
        .find(|file| file.contains("codex-key-"))
        .unwrap()
        .clone();
    let mut mismatches = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let actual = if name.starts_with("socket ") {
            let mut stream = UnixStream::connect(setup.socket_path()).await.unwrap();
            stream
                .write_all(case["input"].as_str().unwrap().as_bytes())
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).await.unwrap();
            Value::String(reply)
        } else {
            *gateway.lock().unwrap() = Gateway {
                port: u16::try_from(case["input"]["port"].as_u64().unwrap()).unwrap(),
                running: case["input"]["running"].as_bool().unwrap(),
            };
            let output = run("sh", &[&helper], &root.root).await;
            json!({
                "status": output.status.code().unwrap(),
                "stdout": String::from_utf8(output.stdout).unwrap(),
                "stderr": String::from_utf8(output.stderr).unwrap(),
            })
        };
        if actual != case["output"] {
            mismatches.push(format!("{name}: expected {} got {actual}", case["output"]));
        }
    }
    setup.close().await.unwrap();
    mismatches
}

/// The metadata models.dev gave each model the goldens offered; any other model has none.
fn enrich(cases: &[Value]) -> Vec<CatalogModel> {
    cases
        .iter()
        .filter(|case| {
            let name = case["name"].as_str().unwrap();
            name.starts_with("offeredModels pi") || name.starts_with("offeredModels opencode")
        })
        .filter_map(|case| case["output"].as_array())
        .flatten()
        .map(|model| serde_json::from_value(model.clone()).unwrap())
        .collect()
}

async fn catalog(
    cases: &[Value],
    case: &Value,
    priced: &[CatalogModel],
) -> Result<Value, AgentError> {
    let name = case["name"].as_str().unwrap();
    if let Some(agent) = name.strip_prefix("entries ") {
        // Made from what `offeredModels <agent> otherProviders false` gave.
        let offered = format!("offeredModels {agent} otherProviders false");
        let offered = cases
            .iter()
            .find(|case| case["name"] == offered.as_str())
            .unwrap();
        let models: Vec<CatalogModel> = serde_json::from_value(offered["output"].clone()).unwrap();
        return Ok(json!(catalog_entries(
            Agent::from_id(agent).unwrap(),
            &models
        )?));
    }
    let input = &case["input"];
    let agent = Agent::from_id(input["agent"].as_str().unwrap()).unwrap();
    let models: Vec<Model> = serde_json::from_value(input["models"].clone()).unwrap();
    let other = input["otherProviders"].as_bool().unwrap_or(false);
    let offered = offered_models(agent, models, other, |models| async move {
        models
            .into_iter()
            .map(|model| {
                priced
                    .iter()
                    .find(|known| known.id == model.id)
                    .cloned()
                    .unwrap_or(CatalogModel {
                        id: model.id,
                        owned_by: model.owned_by,
                        metadata: None,
                    })
            })
            .collect()
    })
    .await?;
    Ok(json!(offered))
}

fn schema(case: &Value) -> Result<Value, AgentError> {
    let input: AgentSetupInput = serde_json::from_value(case["input"].clone())
        .map_err(|_| AgentError::Invalid("body".to_owned()))?;
    validate_setup(&input)?;
    Ok(json!(input))
}

/// pi's controls are shipped inside every pi extension: the one a connection writes holds the
/// golden source verbatim.
async fn pi_controls(source: &str) -> bool {
    let root = Scratch::new();
    let (home, data) = (root.root.join("home"), root.root.join("data"));
    directory(&home, 0o755);
    let setup = AgentSetup::new(
        &DataDir::new(&data),
        Arc::new(|_| {
            Ok(Connection {
                key: ClientKey::new(key('a')),
                port: 4318,
            })
        }),
        crate::common::home_only(&home),
    );
    let input = AgentSetupInput {
        agent: Agent::Pi,
        project_directory: None,
        other_providers: None,
    };
    let installed = setup
        .install(&work(), input, vec!["m".to_owned()])
        .await
        .unwrap();
    setup.close().await.unwrap();
    fs::read_to_string(&installed.files[0])
        .unwrap()
        .contains(source)
}

#[tokio::test]
async fn agent_setup_matches_the_typescript_goldens() {
    let cases = cases();
    assert_eq!(cases.len(), 81);
    let priced = enrich(&cases);
    let mut mismatches = Vec::new();
    let mut replayed = 0;
    let mut socket_cases = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let outcome = if name.starts_with("scenario ") {
            mismatches.extend(scenario(case).await);
            replayed += 1;
            continue;
        } else if name.starts_with("socket ") || name.starts_with("key helper ") {
            socket_cases.push(case);
            replayed += 1;
            continue;
        } else if name.starts_with("offeredModels ") || name.starts_with("entries ") {
            catalog(&cases, case, &priced).await
        } else if name.starts_with("agentSetupSchema ") {
            schema(case)
        } else if name == "pi profile controls source" {
            replayed += 1;
            if !pi_controls(case["output"].as_str().unwrap()).await {
                mismatches.push(format!(
                    "{name}: the pi extension does not hold the controls source"
                ));
            }
            continue;
        } else {
            // `unprintable` records a JavaScript regular expression that pi evaluates; the
            // extension carries its text verbatim (checked by the pi scenarios).
            assert!(name.starts_with("unprintable "), "{name}");
            continue;
        };
        let outcome = outcome.map_err(AppError::from);
        replayed += 1;
        let matched = match (&outcome, &case["error"]) {
            (Err(error), Value::Object(_)) => error_matches(&case["error"], error),
            (Ok(output), Value::Null) => *output == case["output"],
            _ => false,
        };
        if !matched {
            mismatches.push(format!("{name}: expected {case} got {outcome:?}"));
        }
    }
    mismatches.extend(credential_socket(&socket_cases).await);
    assert_eq!(
        replayed, 75,
        "every case but the six JavaScript `unprintable` ones"
    );
    assert!(
        mismatches.is_empty(),
        "{} mismatches:\n{}",
        mismatches.len(),
        mismatches.join("\n\n")
    );
}
