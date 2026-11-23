//! Claude Code: our fields in the project's `.claude/settings.local.json`, which the user shares
//! with us, plus a `--settings` file carrying the model picker.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use nsv_domain::endpoint::profile_base;
use nsv_domain::js;
use nsv_domain::wire::Profile;
use serde_json::{Map, Value};

use crate::connection::{Definition, shell_quote, text};
use crate::error::{AgentError, conflict, invalid, refused};
use crate::files::{json_object, read_config};
use crate::json;
use crate::manifest::Manifest;

pub(crate) const DEFINITION: Definition = Definition {
    version: 3,
    older_needs_reconnect: true,
    directory: None,
    project_root: Some(claude_project),
    file: |_, _| vec![".claude".to_owned(), "settings.local.json".to_owned()],
    helper: true,
    configure: |context| settings(context.profile, context.helper, context.port),
    merge,
    // The file is shared with the user, so key order and formatting are theirs.
    unchanged: |current, expected| {
        Ok(json_object(current)? == json_object(expected.unwrap_or("{}"))?)
    },
    retain: Some(matching),
    launch_settings: Some(launch_settings),
    command: |_, launch| format!("claude --settings {}", shell_quote(launch)),
    instructions: |profile| {
        format!(
            "Run it inside the connected project; plain claude skips the picker. Approve the credential helper, pick a model with /model, and check /status shows {}.",
            profile.name
        )
    },
};

fn settings(profile: &Profile, helper: &Path, port: u16) -> String {
    let env = [
        ("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1".to_owned()),
        ("CLAUDE_CODE_USE_BEDROCK", "0".to_owned()),
        ("CLAUDE_CODE_USE_VERTEX", "0".to_owned()),
        ("CLAUDE_CODE_USE_FOUNDRY", "0".to_owned()),
        ("CLAUDE_CODE_USE_MANTLE", "0".to_owned()),
        ("CLAUDE_CODE_USE_ANTHROPIC_AWS", "0".to_owned()),
        ("ANTHROPIC_CUSTOM_HEADERS", String::new()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_owned()),
        (
            "ANTHROPIC_BASE_URL",
            profile_base(port, profile.slug.as_str()),
        ),
        ("ANTHROPIC_AUTH_TOKEN", String::new()),
        ("ANTHROPIC_API_KEY", String::new()),
        ("CLAUDE_CODE_OAUTH_TOKEN", String::new()),
        ("CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR", String::new()),
    ];
    let env: Map<String, Value> = env
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.into()))
        .collect();
    Value::Object(json::object([
        ("apiKeyHelper", shell_quote(&text(helper)).into()),
        ("env", Value::Object(env)),
    ]))
    .to_string()
}

/// `object.parse(value ?? {})`: an object, empty when absent or `null`.
fn record(value: Option<&Value>, path: &str) -> Result<Map<String, Value>, AgentError> {
    match value {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(object)) => Ok(object.clone()),
        Some(_) => Err(invalid(path, "Invalid input: expected record")),
    }
}

/// Retains only values equal to our generated, nonsecret settings. Never backs up arbitrary
/// user configuration: it may contain credentials.
fn matching(current: Option<&str>, generated: &str) -> Result<Option<String>, AgentError> {
    let Some(current) = current else {
        return Ok(None);
    };
    let saved = json_object(current)?;
    let fresh = json_object(generated)?;
    let same = |existing: &Map<String, Value>, fields: &Map<String, Value>| -> Map<String, Value> {
        fields
            .iter()
            .filter(|(key, value)| *key != "env" && existing.get(*key) == Some(*value))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    };
    let env = same(
        &record(saved.get("env"), "env")?,
        &record(fresh.get("env"), "env")?,
    );
    let mut retained = same(&saved, &fresh);
    if !env.is_empty() {
        retained.insert("env".to_owned(), Value::Object(env));
    }
    Ok(Some(Value::Object(retained).to_string()))
}

fn merge(
    current: Option<&str>,
    previous: Option<&str>,
    next: Option<&str>,
) -> Result<Option<String>, AgentError> {
    let parse = |content: Option<&str>| content.map_or_else(|| Ok(Map::new()), json_object);
    let mut root = parse(current)?;
    let old = parse(previous)?;
    let fresh = parse(next)?;
    let mut env = match root.get("env") {
        None => Map::new(),
        Some(Value::Object(env)) => env.clone(),
        Some(_) => return Err(invalid("env", "Invalid input: expected record")),
    };
    let (old_env, fresh_env) = (
        record(old.get("env"), "env")?,
        record(fresh.get("env"), "env")?,
    );
    replace(&mut root, &old, &fresh)?;
    replace(&mut env, &old_env, &fresh_env)?;
    if env.is_empty() {
        root.shift_remove("env");
    } else {
        root.insert("env".to_owned(), Value::Object(env));
    }
    Ok((!root.is_empty()).then(|| format!("{}\n", js::pretty(&Value::Object(root)))))
}

/// Replaces our `old` fields in `target` with `fresh`; a field the user set differently is a
/// conflict. `env` is merged on its own.
fn replace(
    target: &mut Map<String, Value>,
    old: &Map<String, Value>,
    fresh: &Map<String, Value>,
) -> Result<(), AgentError> {
    let keys = old
        .keys()
        .chain(fresh.keys().filter(|key| !old.contains_key(*key)));
    for key in keys.filter(|key| *key != "env") {
        if let Some(value) = target.get(key)
            && old.get(key) != Some(value)
            && fresh.get(key) != Some(value)
        {
            return Err(conflict());
        }
        match fresh.get(key) {
            None => {
                target.shift_remove(key);
            }
            Some(value) => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(())
}

fn launch_settings(manifest: &Manifest) -> Result<Option<String>, AgentError> {
    // Connections before version 3 predate the picker.
    if manifest.version < 3 {
        return Ok(None);
    }
    // Project and local settings ignore modelPicker. --settings carries the same immutable
    // profile route plus the real model IDs, without choosing a model.
    let mut settings = json_object(&manifest.content)?;
    let options: Vec<Value> = manifest
        .models
        .iter()
        .map(|model| Value::Object(json::object([("model", model.clone().into())])))
        .collect();
    settings.insert(
        "modelPicker".to_owned(),
        Value::Object(json::object([
            ("options", Value::Array(options)),
            ("replaceBuiltInOptions", true.into()),
        ])),
    );
    Ok(Some(format!("{}\n", js::pretty(&Value::Object(settings)))))
}

/// The project a connection belongs to: the canonical directory, which must be owned without
/// shared write access and be its repository's root. Claude worktrees inherit the main
/// checkout's local settings, so the main checkout is chosen explicitly rather than promising
/// isolation the harness does not provide.
pub(crate) fn claude_project(path: &Path) -> Result<PathBuf, AgentError> {
    let project = fs::canonicalize(path)?;
    let status = fs::symlink_metadata(&project)?;
    if !status.is_dir()
        || status.uid() != rustix::process::getuid().as_raw()
        || status.mode() & 0o022 != 0
    {
        return Err(refused(
            "Choose a project directory you own, without shared write access.",
        ));
    }
    let mut cursor = project.as_path();
    while let Some(parent) = cursor.parent() {
        match git_marker(cursor) {
            Ok(()) => break,
            Err(error) if error.is_missing() => {}
            Err(error) => return Err(error),
        }
        cursor = parent;
    }
    if cursor != project && cursor.parent().is_some() {
        return Err(refused(
            "Choose the repository root so Claude loads the same settings throughout the project.",
        ));
    }
    Ok(project)
}

/// Checks the Git marker in `directory`, missing when it has none; a linked worktree's is
/// refused.
fn git_marker(directory: &Path) -> Result<(), AgentError> {
    let marker = directory.join(".git");
    let git = fs::symlink_metadata(&marker)?;
    if git.is_symlink() || (!git.is_file() && !git.is_dir()) {
        return Err(refused(
            "The project's Git marker must be an ordinary file or directory without symbolic links.",
        ));
    }
    if git.is_file() {
        let content = read_config(&marker)?;
        let git_dir = content
            .as_deref()
            .map(js::trim)
            .and_then(|content| content.strip_prefix("gitdir: "))
            .filter(|rest| {
                !rest.is_empty() && !rest.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
            });
        if let Some(git_dir) = git_dir
            && read_config(&resolve(directory, git_dir).join("commondir"))?
                .is_some_and(|common| !common.is_empty())
        {
            return Err(refused(
                "This project is a Git worktree. Claude shares local settings with its main checkout; choose the main checkout instead.",
            ));
        }
    }
    Ok(())
}

/// Node's `path.resolve(base, path)`: lexical, without touching the file system.
fn resolve(base: &Path, path: &str) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in base.join(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}
