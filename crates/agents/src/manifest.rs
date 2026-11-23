//! A connection's input, as the routes and the manifests take it, and the manifest each
//! connection records under `<data>/agent-connections/<slug>/`.

use nsv_domain::registry::{Agent, Registry};
use nsv_domain::wire::{AgentSelection, AgentSetupInput, utf16_len};
use serde_json::{Map, Value};

use crate::error::{AgentError, invalid};
use crate::files::FILE_LIMIT;
use crate::json;

/// Manifest versions releases have written; version 1 has its own schema.
const VERSIONS: std::ops::RangeInclusive<u8> = 1..=7;
const PROJECT_MESSAGE: &str = "Choose a project for Claude Code only.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub version: u8,
    pub input: AgentSetupInput,
    pub models: Vec<String>,
    pub port: u16,
    /// The agent configuration text the connection wrote.
    pub content: String,
    /// Our settings that were in the user's file before connecting.
    pub retained: Option<String>,
    pub helper_content: Option<String>,
}

impl Manifest {
    /// `JSON.stringify(manifest, null, 2) + "\n"`, in the order the TypeScript server set fields.
    pub(crate) fn text(&self) -> String {
        let mut object = json::object([
            ("version", self.version.into()),
            ("input", Value::Object(input_json(&self.input))),
            ("models", self.models.clone().into()),
            ("port", self.port.into()),
            ("content", self.content.clone().into()),
        ]);
        if let Some(helper) = &self.helper_content {
            object.insert("helperContent".to_owned(), helper.clone().into());
        }
        if let Some(retained) = &self.retained {
            object.insert("retained".to_owned(), retained.clone().into());
        }
        format!("{}\n", json::pretty(&Value::Object(object)))
    }

    /// Parses a manifest a release wrote; a version 1 manifest becomes the current shape.
    pub(crate) fn parse(
        registry: &Registry,
        mut object: Map<String, Value>,
    ) -> Result<Self, AgentError> {
        let version = match object.get("version").and_then(Value::as_u64) {
            Some(version) => u8::try_from(version).ok().filter(|v| VERSIONS.contains(v)),
            None => None,
        }
        .ok_or_else(|| invalid("version", "Invalid input"))?;
        let legacy = version == 1;
        let allowed: &[&str] = if legacy {
            &[
                "version",
                "input",
                "port",
                "content",
                "retained",
                "helperContent",
            ]
        } else {
            &[
                "version",
                "input",
                "models",
                "port",
                "content",
                "retained",
                "helperContent",
            ]
        };
        strict(&object, "", allowed)?;
        let input = object.remove("input").unwrap_or(Value::Null);
        let (input, models) = if legacy {
            legacy_input(registry, input)?
        } else {
            let input = setup_input(input)?;
            validate_setup(registry, &input)?;
            (input, manifest_models(object.remove("models"))?)
        };
        let port = object
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
            .filter(|&port| port >= 1)
            .ok_or_else(|| invalid("port", "Invalid input"))?;
        Ok(Self {
            version,
            input,
            models,
            port,
            content: limited_text(&mut object, "content")?
                .ok_or_else(|| invalid("content", "Invalid input"))?,
            retained: limited_text(&mut object, "retained")?,
            helper_content: limited_text(&mut object, "helperContent")?,
        })
    }
}

fn input_json(input: &AgentSetupInput) -> Map<String, Value> {
    let mut object = json::object([("agent", input.agent.as_str().into())]);
    if let Some(project) = &input.project_directory {
        object.insert("projectDirectory".to_owned(), project.clone().into());
    }
    if let Some(other) = input.other_providers {
        object.insert("otherProviders".to_owned(), other.into());
    }
    object
}

/// zod's `.strict()`: no key outside `allowed`.
fn strict(object: &Map<String, Value>, path: &str, allowed: &[&str]) -> Result<(), AgentError> {
    match object.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(invalid(path, &format!("Unrecognized key: \"{key}\""))),
        None => Ok(()),
    }
}

/// An optional string of at most 1 MB (as JavaScript counts length); `null` is not absent.
fn limited_text(object: &mut Map<String, Value>, key: &str) -> Result<Option<String>, AgentError> {
    match object.remove(key) {
        None => Ok(None),
        Some(Value::String(text)) if utf16_len(&text) <= FILE_LIMIT => Ok(Some(text)),
        Some(_) => Err(invalid(key, "Invalid input")),
    }
}

fn agent(value: Option<&Value>) -> Result<Agent, AgentError> {
    value
        .and_then(Value::as_str)
        .and_then(Agent::from_id)
        .ok_or_else(|| invalid("input.agent", "Invalid option"))
}

fn optional_project(value: Option<&Value>) -> Result<Option<String>, AgentError> {
    match value {
        None => Ok(None),
        Some(Value::String(project)) => Ok(Some(project.clone())),
        Some(_) => Err(invalid("input.projectDirectory", "Invalid input")),
    }
}

/// A manifest's `input`, as `agentSetupSchema` takes it (its refinements are `validate_setup`).
fn setup_input(value: Value) -> Result<AgentSetupInput, AgentError> {
    let Value::Object(object) = value else {
        return Err(invalid("input", "Invalid input"));
    };
    strict(
        &object,
        "input",
        &["agent", "projectDirectory", "otherProviders"],
    )?;
    Ok(AgentSetupInput {
        agent: agent(object.get("agent"))?,
        project_directory: optional_project(object.get("projectDirectory"))?,
        other_providers: match object.get("otherProviders") {
            None => None,
            Some(Value::Bool(other)) => Some(*other),
            Some(_) => return Err(invalid("input.otherProviders", "Invalid input")),
        },
    })
}

/// A version 1 manifest's single-model input, with its one model as the catalog.
fn legacy_input(
    registry: &Registry,
    value: Value,
) -> Result<(AgentSetupInput, Vec<String>), AgentError> {
    let Value::Object(object) = value else {
        return Err(invalid("input", "Invalid input"));
    };
    strict(
        &object,
        "input",
        &[
            "agent",
            "model",
            "projectDirectory",
            "contextWindow",
            "maxTokens",
        ],
    )?;
    let number = |key: &str, range: std::ops::RangeInclusive<u64>| {
        object
            .get(key)
            .and_then(Value::as_u64)
            .filter(|value| range.contains(value))
            .ok_or_else(|| invalid(&format!("input.{key}"), "Invalid input"))
    };
    let context = number("contextWindow", 1024..=2_000_000)?;
    let output = number("maxTokens", 1..=200_000)?;
    let model = match object.get("model") {
        Some(Value::String(model)) => model_id(model, "input.model")?.to_owned(),
        _ => return Err(invalid("input.model", "Invalid input")),
    };
    if output >= context {
        return Err(invalid(
            "",
            "Output budget must be smaller than the context budget.",
        ));
    }
    let input = AgentSetupInput {
        agent: agent(object.get("agent"))?,
        project_directory: optional_project(object.get("projectDirectory"))?,
        other_providers: None,
    };
    validate_project(input.project_directory.as_deref())?;
    if !takes_project(
        registry,
        input.agent,
        input.project_directory.as_deref(),
        true,
    ) {
        return Err(invalid("", PROJECT_MESSAGE));
    }
    Ok((input, vec![model]))
}

fn manifest_models(value: Option<Value>) -> Result<Vec<String>, AgentError> {
    let Some(Value::Array(items)) = value else {
        return Err(invalid("models", "Invalid input"));
    };
    if items.len() > 10_000 {
        return Err(invalid(
            "models",
            "Too big: expected array to have <=10000 items",
        ));
    }
    items
        .into_iter()
        .map(|item| match item {
            Value::String(model) if (1..=200).contains(&utf16_len(&model)) => Ok(model),
            _ => Err(invalid("models", "Invalid input")),
        })
        .collect()
}

/// Every character is printable ASCII or beyond: no C0 control and no DEL.
fn without_controls(value: &str) -> bool {
    value.chars().all(|c| c >= ' ' && c != '\u{7f}')
}

fn model_id<'a>(model: &'a str, path: &str) -> Result<&'a str, AgentError> {
    if !(1..=200).contains(&utf16_len(model)) {
        return Err(invalid(path, "Model IDs have 1 to 200 characters."));
    }
    if !without_controls(model) {
        return Err(invalid(
            path,
            "Model IDs cannot contain control characters.",
        ));
    }
    Ok(model)
}

/// The catalog a connection offers: 1 to 10000 model IDs without control characters.
pub(crate) fn validate_models(models: &[String]) -> Result<(), AgentError> {
    if models.is_empty() || models.len() > 10_000 {
        return Err(invalid("", "Offer 1 to 10000 models."));
    }
    for model in models {
        model_id(model, "")?;
    }
    Ok(())
}

fn validate_project(project: Option<&str>) -> Result<(), AgentError> {
    let Some(project) = project else {
        return Ok(());
    };
    if !(1..=4096).contains(&utf16_len(project)) {
        return Err(invalid(
            "projectDirectory",
            "Project directories have 1 to 4096 characters.",
        ));
    }
    if !project.starts_with('/') || !without_controls(project) {
        return Err(invalid(
            "projectDirectory",
            "Choose an absolute project directory.",
        ));
    }
    Ok(())
}

/// Claude Code connects one project at a time; the other agents take none. Connecting
/// `requires` the project; looking a connection up may leave it out.
fn takes_project(registry: &Registry, agent: Agent, project: Option<&str>, requires: bool) -> bool {
    if registry.agents.get(agent).project {
        !requires || project.is_some_and(|project| !project.is_empty())
    } else {
        project.is_none()
    }
}

/// `agentSetupSchema`'s rules for a connection the app makes, beyond the shape serde checks.
///
/// # Errors
/// [`AgentError::Invalid`] (400) when the input breaks one.
pub fn validate_setup(registry: &Registry, input: &AgentSetupInput) -> Result<(), AgentError> {
    validate_project(input.project_directory.as_deref())?;
    if !takes_project(
        registry,
        input.agent,
        input.project_directory.as_deref(),
        true,
    ) {
        return Err(invalid("", PROJECT_MESSAGE));
    }
    if registry.agents.get(input.agent).native.is_none() && input.other_providers.is_some() {
        return Err(invalid(
            "",
            "Only Claude Code and Codex choose whether to offer other providers.",
        ));
    }
    Ok(())
}

/// `agentSelectionSchema`'s rules for looking a connection up or disconnecting it.
///
/// # Errors
/// [`AgentError::Invalid`] (400) when the selection breaks one.
pub fn validate_selection(
    registry: &Registry,
    selection: &AgentSelection,
) -> Result<(), AgentError> {
    validate_project(selection.project_directory.as_deref())?;
    if takes_project(
        registry,
        selection.agent,
        selection.project_directory.as_deref(),
        false,
    ) {
        Ok(())
    } else {
        Err(invalid("", PROJECT_MESSAGE))
    }
}
