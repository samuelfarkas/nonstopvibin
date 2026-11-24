//! A connection's input, as the routes and the manifests take it, and the manifest each
//! connection records under `<data>/agent-connections/<slug>/`.

use nsv_domain::js;
use nsv_domain::registry::{Agent, registry};
use nsv_domain::wire::{AgentSelection, AgentSetupInput};
use serde_json::{Map, Value};

use crate::connection::definition;
use crate::error::{AgentError, invalid};
use crate::files::FILE_LIMIT;
use crate::json;

/// The newest manifest version a connection records; releases wrote every version from 1, which
/// has its own schema.
fn newest_version() -> u8 {
    Agent::ALL
        .iter()
        .fold(1, |newest, &agent| newest.max(definition(agent).version))
}

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
        format!("{}\n", js::pretty(&Value::Object(object)))
    }

    /// Parses a manifest a release wrote; a version 1 manifest becomes the current shape.
    pub(crate) fn parse(mut object: Map<String, Value>) -> Result<Self, AgentError> {
        let version = match object.get("version").and_then(Value::as_u64) {
            Some(version) => u8::try_from(version)
                .ok()
                .filter(|v| (1..=newest_version()).contains(v)),
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
            legacy_input(input)?
        } else {
            let input = setup_input(input)?;
            validate_setup(&input)?;
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
        Some(Value::String(text)) if js::utf16_len(&text) <= FILE_LIMIT => Ok(Some(text)),
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
fn legacy_input(value: Value) -> Result<(AgentSetupInput, Vec<String>), AgentError> {
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
    validate_setup(&input)?;
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
            Value::String(model) if (1..=200).contains(&js::utf16_len(&model)) => Ok(model),
            _ => Err(invalid("models", "Invalid input")),
        })
        .collect()
}

/// Every character is printable ASCII or beyond: no C0 control and no DEL.
fn without_controls(value: &str) -> bool {
    value.chars().all(|c| c >= ' ' && c != '\u{7f}')
}

fn model_id<'a>(model: &'a str, path: &str) -> Result<&'a str, AgentError> {
    if !(1..=200).contains(&js::utf16_len(model)) {
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

/// A rule of a connection's input that both the routes and the manifests enforce:
/// [`validate_setup`] refuses with the first one an input breaks, and the server's zod-shaped
/// parse reports each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Issue {
    /// The project directory is shorter or longer than [`PROJECT_LENGTH`].
    ProjectLength,
    /// The project directory is relative or holds a control character.
    ProjectNotAbsolute,
    /// A project for an agent that takes none, or, when connecting, none for Claude Code.
    Project,
    /// `otherProviders` for an agent without a curated catalog.
    OtherProviders,
}

/// How long a project directory may be, in UTF-16 code units as JavaScript counts them.
pub const PROJECT_LENGTH: std::ops::RangeInclusive<usize> = 1..=4096;

impl Issue {
    /// The input field it concerns; empty for the input as a whole.
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::ProjectLength | Self::ProjectNotAbsolute => "projectDirectory",
            Self::Project | Self::OtherProviders => "",
        }
    }

    /// What the user is told.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::ProjectLength => "Project directories have 1 to 4096 characters.",
            Self::ProjectNotAbsolute => "Choose an absolute project directory.",
            Self::Project => "Choose a project for Claude Code only.",
            Self::OtherProviders => {
                "Only Claude Code and Codex choose whether to offer other providers."
            }
        }
    }
}

/// What a project directory breaks, in zod's order.
#[must_use]
pub fn project_issues(project: &str) -> Vec<Issue> {
    let mut issues = Vec::new();
    if !PROJECT_LENGTH.contains(&js::utf16_len(project)) {
        issues.push(Issue::ProjectLength);
    }
    if !project.starts_with('/') || !without_controls(project) {
        issues.push(Issue::ProjectNotAbsolute);
    }
    issues
}

/// Claude Code connects one project at a time; the other agents take none. Connecting
/// `requires` the project; looking a connection up may leave it out.
fn takes_project(agent: Agent, project: Option<&str>, requires: bool) -> bool {
    if registry().agents.get(agent).project {
        !requires || project.is_some_and(|project| !project.is_empty())
    } else {
        project.is_none()
    }
}

/// What a connection the app makes breaks beyond its project directory's own rules.
#[must_use]
pub fn setup_issues(input: &AgentSetupInput) -> Vec<Issue> {
    let mut issues = Vec::new();
    if !takes_project(input.agent, input.project_directory.as_deref(), true) {
        issues.push(Issue::Project);
    }
    if !registry().curated(input.agent) && input.other_providers.is_some() {
        issues.push(Issue::OtherProviders);
    }
    issues
}

/// What a lookup or a disconnection breaks beyond its project directory's own rules.
#[must_use]
pub fn selection_issues(selection: &AgentSelection) -> Vec<Issue> {
    if takes_project(
        selection.agent,
        selection.project_directory.as_deref(),
        false,
    ) {
        Vec::new()
    } else {
        vec![Issue::Project]
    }
}

/// `agentSetupSchema`'s rules for a connection the app makes, beyond the shape serde checks.
///
/// # Errors
/// [`AgentError::Invalid`] (400) with the first [`Issue`] the input breaks.
pub fn validate_setup(input: &AgentSetupInput) -> Result<(), AgentError> {
    let project = input.project_directory.as_deref().map(project_issues);
    match project
        .into_iter()
        .flatten()
        .chain(setup_issues(input))
        .next()
    {
        Some(issue) => Err(invalid(issue.path(), issue.message())),
        None => Ok(()),
    }
}
