//! Request bodies, queries and path segments, checked at the boundary as the TypeScript routes'
//! zod schemas checked them, with the same messages: each issue is `path: message`, and the 400
//! joins them with `; `. Past this module, handlers receive typed values only.
//!
//! zod's order holds: an object's fields in schema order, then its unknown keys. A wrong type,
//! option or key stops the object's own refinements; a failed length or pattern check does not.

use std::fmt::Display;

use nsv_agents::{Issue, PROJECT_LENGTH, project_issues, selection_issues, setup_issues};
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, is_uuid};
use nsv_domain::js;
use nsv_domain::registry::{
    Agent, OAuthProvider, Protocol, Provider, RetentionDays, Strategy, registry,
};
use nsv_domain::secret::ApiKey;
use nsv_domain::wire::{
    AccountPatch, AgentSelection, AgentSetupInput, ApiAccountInput, ApiModelInput, ProfilePatch,
};
use serde_json::{Map, Value};

/// A refused input; its message is the 400's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Invalid(pub(crate) String);

impl From<Invalid> for AppError {
    fn from(error: Invalid) -> Self {
        Self::new(error.0, 400)
    }
}

#[derive(Debug, Default)]
struct Issues {
    list: Vec<String>,
    /// A wrong type, option or key: the enclosing object's refinements are skipped.
    aborted: bool,
}

impl Issues {
    fn fatal(&mut self, path: &str, message: impl Display) {
        self.list.push(format!("{path}: {message}"));
        self.aborted = true;
    }

    fn check(&mut self, path: &str, message: impl Display) {
        self.list.push(format!("{path}: {message}"));
    }

    fn finish<T>(self, value: Option<T>) -> Result<T, Invalid> {
        match value {
            Some(value) if self.list.is_empty() => Ok(value),
            _ if self.list.is_empty() => Err(Invalid(": Invalid input".to_owned())),
            _ => Err(Invalid(self.list.join("; "))),
        }
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// zod's name for the type of a value it did not expect.
fn received(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

fn wrong_type(issues: &mut Issues, path: &str, expected: &str, value: Option<&Value>) {
    issues.fatal(
        path,
        format_args!(
            "Invalid input: expected {expected}, received {}",
            received(value)
        ),
    );
}

fn object<'a>(
    issues: &mut Issues,
    value: Option<&'a Value>,
    path: &str,
) -> Option<&'a Map<String, Value>> {
    match value {
        Some(Value::Object(map)) => Some(map),
        other => {
            wrong_type(issues, path, "object", other);
            None
        }
    }
}

/// `.strict()`: keys outside `known` are one issue, after the fields'.
fn strict(issues: &mut Issues, map: &Map<String, Value>, path: &str, known: &[&str]) {
    let unknown: Vec<String> = map
        .keys()
        .filter(|key| !known.contains(&key.as_str()))
        .map(|key| Value::String(key.clone()).to_string())
        .collect();
    match unknown.as_slice() {
        [] => {}
        [one] => issues.fatal(path, format_args!("Unrecognized key: {one}")),
        many => issues.fatal(path, format_args!("Unrecognized keys: {}", many.join(", "))),
    }
}

fn string<'a>(issues: &mut Issues, value: Option<&'a Value>, path: &str) -> Option<&'a str> {
    match value {
        Some(Value::String(text)) => Some(text),
        other => {
            wrong_type(issues, path, "string", other);
            None
        }
    }
}

fn boolean(issues: &mut Issues, value: Option<&Value>, path: &str) -> Option<bool> {
    match value {
        Some(Value::Bool(flag)) => Some(*flag),
        other => {
            wrong_type(issues, path, "boolean", other);
            None
        }
    }
}

/// `.min(min).max(max)` on a string, in UTF-16 code units as JavaScript counts them.
fn length(issues: &mut Issues, text: &str, path: &str, min: usize, max: usize) {
    let units = js::utf16_len(text);
    if units < min {
        issues.check(
            path,
            format_args!("Too small: expected string to have >={min} characters"),
        );
    }
    if units > max {
        issues.check(
            path,
            format_args!("Too big: expected string to have <={max} characters"),
        );
    }
}

/// `z.string().trim().min(min).max(max)`: the trimmed string.
fn trimmed(
    issues: &mut Issues,
    value: Option<&Value>,
    path: &str,
    min: usize,
    max: usize,
) -> Option<String> {
    let text = js::trim(string(issues, value, path)?);
    length(issues, text, path, min, max);
    Some(text.to_owned())
}

/// `z.enum(options)`.
fn option<T: Copy>(
    issues: &mut Issues,
    value: Option<&Value>,
    path: &str,
    options: &[T],
    name: fn(T) -> &'static str,
) -> Option<T> {
    let found = value
        .and_then(Value::as_str)
        .and_then(|text| options.iter().copied().find(|option| name(*option) == text));
    if found.is_none() {
        let listed: Vec<String> = options
            .iter()
            .map(|option| Value::String(name(*option).to_owned()).to_string())
            .collect();
        issues.fatal(
            path,
            format_args!("Invalid option: expected one of {}", listed.join("|")),
        );
    }
    found
}

/// A field that zod reads as `.optional()`: absent is `Some(None)`; `null` is a wrong type,
/// which `read` reports as `None`.
#[expect(
    clippy::option_option,
    reason = "an absent field and a field that failed its check are different outcomes"
)]
fn optional<'a, T>(
    map: &'a Map<String, Value>,
    key: &str,
    read: impl FnOnce(&'a Value) -> Option<T>,
) -> Option<Option<T>> {
    map.get(key)
        .map_or(Some(None), |value| read(value).map(Some))
}

/// `z.string().regex(pattern)`, where `matches` is the outcome and `source` the pattern.
fn pattern(issues: &mut Issues, path: &str, matches: bool, source: &str) {
    if !matches {
        issues.check(
            path,
            format_args!("Invalid string: must match pattern {source}"),
        );
    }
}

fn integer_range(issues: &mut Issues, number: f64, path: &str, min: f64, max: f64) {
    if number.fract() != 0.0 {
        issues.check(path, "Invalid input: expected int, received number");
    }
    if number < min {
        issues.check(
            path,
            format_args!("Too small: expected number to be >={min}"),
        );
    }
    if number > max {
        issues.check(path, format_args!("Too big: expected number to be <={max}"));
    }
}

/// The body of a route that takes `{}`.
pub(crate) fn empty(value: &Value) -> Result<(), Invalid> {
    let mut issues = Issues::default();
    let parsed = object(&mut issues, Some(value), "").map(|map| strict(&mut issues, map, "", &[]));
    issues.finish(parsed)
}

/// `PUT /usage-retention`: `{ days: 0 | 30 | 90 | 365 }`.
pub(crate) fn retention(value: &Value) -> Result<RetentionDays, Invalid> {
    let mut issues = Issues::default();
    let days = object(&mut issues, Some(value), "").and_then(|map| {
        let days = map.get("days").and_then(Value::as_f64).and_then(|days| {
            RetentionDays::ALL.iter().copied().find(|option| {
                #[expect(clippy::float_cmp, reason = "JavaScript's === on literal numbers")]
                let equal = f64::from(option.days()) == days;
                equal
            })
        });
        if days.is_none() {
            issues.fatal("days", "Invalid input");
        }
        strict(&mut issues, map, "", &["days"]);
        days
    });
    issues.finish(days)
}

/// `POST /profiles`: the trimmed name.
pub(crate) fn profile_name(value: &Value) -> Result<String, Invalid> {
    let mut issues = Issues::default();
    let name = object(&mut issues, Some(value), "").and_then(|map| {
        let name = trimmed(
            &mut issues,
            map.get("name"),
            "name",
            1,
            registry().limits.name_length,
        );
        strict(&mut issues, map, "", &["name"]);
        name
    });
    issues.finish(name)
}

/// `PATCH /profiles/:profileId`.
pub(crate) fn profile_patch(value: &Value) -> Result<ProfilePatch, Invalid> {
    let mut issues = Issues::default();
    let patch = object(&mut issues, Some(value), "").and_then(|map| {
        let limit = registry().limits.name_length;
        let name = optional(map, "name", |value| {
            trimmed(&mut issues, Some(value), "name", 1, limit)
        });
        let strategy = optional(map, "strategy", |value| {
            option(
                &mut issues,
                Some(value),
                "strategy",
                Strategy::ALL,
                Strategy::as_str,
            )
        });
        let affinity = optional(map, "sessionAffinity", |value| {
            boolean(&mut issues, Some(value), "sessionAffinity")
        });
        strict(
            &mut issues,
            map,
            "",
            &["name", "strategy", "sessionAffinity"],
        );
        Some(ProfilePatch {
            name: name?,
            strategy: strategy?,
            session_affinity: affinity?,
        })
    });
    issues.finish(patch)
}

/// `projectSchema`: the agents' project rules, its length in zod's own words.
fn project(issues: &mut Issues, value: &Value) -> Option<String> {
    let text = string(issues, Some(value), "projectDirectory")?;
    for issue in project_issues(text) {
        if issue == Issue::ProjectLength {
            let (min, max) = PROJECT_LENGTH.into_inner();
            length(issues, text, issue.path(), min, max);
        } else {
            issues.check(issue.path(), issue.message());
        }
    }
    Some(text.to_owned())
}

/// `agentSetupSchema`: `POST /profiles/:profileId/agent-setup`.
pub(crate) fn agent_setup(value: &Value) -> Result<AgentSetupInput, Invalid> {
    let mut issues = Issues::default();
    let input = object(&mut issues, Some(value), "").and_then(|map| {
        let agent = option(
            &mut issues,
            map.get("agent"),
            "agent",
            Agent::ALL,
            Agent::as_str,
        );
        let project = optional(map, "projectDirectory", |value| project(&mut issues, value));
        let other = optional(map, "otherProviders", |value| {
            boolean(&mut issues, Some(value), "otherProviders")
        });
        strict(
            &mut issues,
            map,
            "",
            &["agent", "projectDirectory", "otherProviders"],
        );
        let input = AgentSetupInput {
            agent: agent?,
            project_directory: project?,
            other_providers: other?,
        };
        if !issues.aborted {
            for issue in setup_issues(&input) {
                issues.check(issue.path(), issue.message());
            }
        }
        Some(input)
    });
    issues.finish(input)
}

/// `agentSelectionSchema`: the agent-setup query and `DELETE` body.
pub(crate) fn agent_selection(value: &Value) -> Result<AgentSelection, Invalid> {
    let mut issues = Issues::default();
    let selection = object(&mut issues, Some(value), "").and_then(|map| {
        let agent = option(
            &mut issues,
            map.get("agent"),
            "agent",
            Agent::ALL,
            Agent::as_str,
        );
        let project = optional(map, "projectDirectory", |value| project(&mut issues, value));
        strict(&mut issues, map, "", &["agent", "projectDirectory"]);
        let selection = AgentSelection {
            agent: agent?,
            project_directory: project?,
        };
        if !issues.aborted {
            for issue in selection_issues(&selection) {
                issues.check(issue.path(), issue.message());
            }
        }
        Some(selection)
    });
    issues.finish(selection)
}

/// The OAuth routes' `state` query: `z.string().min(1)`.
pub(crate) fn state_query(state: Option<&str>) -> Result<String, Invalid> {
    let value = state.map_or(Value::Null, |state| Value::String(state.to_owned()));
    let mut issues = Issues::default();
    let text = string(&mut issues, Some(&value), "").map(|text| {
        length(&mut issues, text, "", 1, usize::MAX);
        text.to_owned()
    });
    issues.finish(text)
}

/// A body of one bounded string field, such as `{ state }` or `{ creditId }`.
pub(crate) fn single(value: &Value, key: &str, max: usize) -> Result<String, Invalid> {
    let mut issues = Issues::default();
    let field = object(&mut issues, Some(value), "").and_then(|map| {
        let text = string(&mut issues, map.get(key), key).map(|text| {
            length(&mut issues, text, key, 1, max);
            text.to_owned()
        });
        strict(&mut issues, map, "", &[key]);
        text
    });
    issues.finish(field)
}

fn url(issues: &mut Issues, text: &str, path: &str) {
    if url::Url::parse(text).is_err() {
        issues.check(path, "Invalid URL");
    }
}

/// `POST /profiles/:profileId/callback`: `{ url }`.
pub(crate) fn callback(value: &Value) -> Result<String, Invalid> {
    let mut issues = Issues::default();
    let field = object(&mut issues, Some(value), "").and_then(|map| {
        let text = string(&mut issues, map.get("url"), "url").map(|text| {
            url(&mut issues, text, "url");
            length(&mut issues, text, "url", 0, 10_000);
            text.to_owned()
        });
        strict(&mut issues, map, "", &["url"]);
        text
    });
    issues.finish(field)
}

/// `POST /profiles/:profileId/oauth`.
pub(crate) fn oauth_start(value: &Value) -> Result<(OAuthProvider, Option<AccountId>), Invalid> {
    let offered: Vec<OAuthProvider> = OAuthProvider::ALL
        .iter()
        .copied()
        .filter(|provider| {
            registry()
                .providers
                .get(Provider::from(*provider))
                .sign_in
                .is_some()
        })
        .collect();
    let mut issues = Issues::default();
    let body = object(&mut issues, Some(value), "").and_then(|map| {
        let provider = option(
            &mut issues,
            map.get("provider"),
            "provider",
            &offered,
            OAuthProvider::as_str,
        );
        let reconnect = optional(map, "reconnectAccountId", |value| {
            let text = string(&mut issues, Some(value), "reconnectAccountId")?;
            length(&mut issues, text, "reconnectAccountId", 1, 255);
            AccountId::parse(text).ok()
        });
        strict(&mut issues, map, "", &["provider", "reconnectAccountId"]);
        Some((provider?, reconnect?))
    });
    issues.finish(body)
}

/// `POST /profiles/:profileId/import`: credential files by name.
pub(crate) fn import(value: &Value) -> Result<Map<String, Value>, Invalid> {
    let mut issues = Issues::default();
    let contents = object(&mut issues, Some(value), "").and_then(|map| {
        let contents = match map.get("contents") {
            Some(Value::Object(contents)) => Some(contents.clone()),
            other => {
                wrong_type(&mut issues, "contents", "record", other);
                None
            }
        };
        strict(&mut issues, map, "", &["contents"]);
        contents
    });
    issues.finish(contents)
}

/// `POST /profiles/:profileId/import-source`: an id a scan handed out.
pub(crate) fn import_source(value: &Value) -> Result<String, Invalid> {
    let mut issues = Issues::default();
    let id = object(&mut issues, Some(value), "").and_then(|map| {
        let id = string(&mut issues, map.get("sourceId"), "sourceId").map(|id| {
            let hex = id.len() == 64 && id.bytes().all(|b| matches!(b, b'a'..=b'f' | b'0'..=b'9'));
            pattern(&mut issues, "sourceId", hex, "/^[a-f0-9]{64}$/");
            id.to_owned()
        });
        strict(&mut issues, map, "", &["sourceId"]);
        id
    });
    issues.finish(id)
}

/// `POST /core/install`: a reviewed core version.
pub(crate) fn core_version(value: &Value) -> Result<String, Invalid> {
    let mut issues = Issues::default();
    let version = object(&mut issues, Some(value), "").and_then(|map| {
        let version = string(&mut issues, map.get("version"), "version").map(|version| {
            let parts: Vec<&str> = version.split('.').collect();
            let semver = parts.len() == 3
                && parts
                    .iter()
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
            pattern(&mut issues, "version", semver, "/^\\d+\\.\\d+\\.\\d+$/");
            version.to_owned()
        });
        strict(&mut issues, map, "", &["version"]);
        version
    });
    issues.finish(version)
}

/// `PATCH /profiles/:profileId/accounts/:accountId`.
pub(crate) fn account_patch(value: &Value) -> Result<AccountPatch, Invalid> {
    let mut issues = Issues::default();
    let patch = object(&mut issues, Some(value), "").and_then(|map| {
        let disabled = optional(map, "disabled", |value| {
            boolean(&mut issues, Some(value), "disabled")
        });
        let limit = registry().limits.priority_limit;
        let priority = optional(map, "priority", |value| {
            let Some(number) = value.as_f64() else {
                wrong_type(&mut issues, "priority", "number", Some(value));
                return None;
            };
            #[expect(
                clippy::cast_precision_loss,
                reason = "the priority limit is a small integer"
            )]
            let bound = limit as f64;
            integer_range(&mut issues, number, "priority", -bound, bound);
            #[expect(
                clippy::cast_possible_truncation,
                reason = "only a whole number within the limit is used"
            )]
            Some(number as i64)
        });
        let name_limit = registry().limits.name_length;
        let name = optional(map, "name", |value| {
            trimmed(&mut issues, Some(value), "name", 1, name_limit)
        });
        strict(&mut issues, map, "", &["disabled", "priority", "name"]);
        Some(AccountPatch {
            disabled: disabled?,
            priority: priority?,
            name: name?,
        })
    });
    issues.finish(patch)
}

/// `POST /profiles/:profileId/api-account`.
pub(crate) fn api_account(value: &Value) -> Result<ApiAccountInput, Invalid> {
    let providers: Vec<Provider> = Provider::ALL
        .iter()
        .copied()
        .filter(|provider| registry().providers.get(*provider).api_key)
        .collect();
    let mut issues = Issues::default();
    let input = object(&mut issues, Some(value), "").and_then(|map| {
        let name = trimmed(
            &mut issues,
            map.get("name"),
            "name",
            1,
            registry().limits.name_length,
        );
        let provider = option(
            &mut issues,
            map.get("provider"),
            "provider",
            &providers,
            Provider::as_str,
        );
        let base_url = string(&mut issues, map.get("baseUrl"), "baseUrl").map(|text| {
            url(&mut issues, text, "baseUrl");
            length(&mut issues, text, "baseUrl", 0, 500);
            text.to_owned()
        });
        let api_key = trimmed(&mut issues, map.get("apiKey"), "apiKey", 1, 20_000);
        let protocol = match map.get("protocol") {
            None => Some(Protocol::Openai),
            value => option(
                &mut issues,
                value,
                "protocol",
                Protocol::ALL,
                Protocol::as_str,
            ),
        };
        let prefix = string(&mut issues, map.get("prefix"), "prefix").map(|text| {
            let valid = text
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            pattern(&mut issues, "prefix", valid, "/^[a-z0-9-]*$/");
            length(
                &mut issues,
                text,
                "prefix",
                0,
                registry().limits.prefix_length,
            );
            text.to_owned()
        });
        let models = models(&mut issues, map.get("models"));
        strict(
            &mut issues,
            map,
            "",
            &[
                "name", "provider", "baseUrl", "apiKey", "protocol", "prefix", "models",
            ],
        );
        Some(ApiAccountInput {
            name: name?,
            provider: provider?,
            base_url: base_url?,
            api_key: ApiKey::new(api_key?),
            protocol: protocol?,
            prefix: prefix?,
            models: models?,
        })
    });
    issues.finish(input)
}

fn models(issues: &mut Issues, value: Option<&Value>) -> Option<Vec<ApiModelInput>> {
    let Some(Value::Array(items)) = value else {
        wrong_type(issues, "models", "array", value);
        return None;
    };
    let mut models = Vec::with_capacity(items.len());
    let mut complete = true;
    for (index, item) in items.iter().enumerate() {
        let path = join("models", &index.to_string());
        let model = object(issues, Some(item), &path).and_then(|entry| {
            let id = trimmed(issues, entry.get("id"), &join(&path, "id"), 1, 200);
            let protocol = option(
                issues,
                entry.get("protocol"),
                &join(&path, "protocol"),
                Protocol::ALL,
                Protocol::as_str,
            );
            Some(ApiModelInput {
                id: id?,
                protocol: protocol?,
            })
        });
        match model {
            Some(model) => models.push(model),
            None => complete = false,
        }
    }
    if items.len() > 500 {
        issues.check("models", "Too big: expected array to have <=500 items");
    }
    complete.then_some(models)
}

/// The `days` query of `GET /profiles/:profileId/usage`:
/// `z.coerce.number().int().min(1).max(all).optional()`; absent is `None`.
pub(crate) fn days(value: Option<&str>, all: u32) -> Result<Option<u32>, Invalid> {
    let Some(value) = value else {
        return Ok(None);
    };
    let number = js::string_to_number(value);
    let mut issues = Issues::default();
    if number.is_nan() {
        issues.fatal("", "Invalid input: expected number, received NaN");
    } else if number.is_infinite() {
        issues.fatal("", "Invalid input: expected number, received number");
    } else {
        integer_range(&mut issues, number, "", 1.0, f64::from(all));
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "only a whole number from 1 to the history limit is used"
    )]
    let days = number as u32;
    issues.finish(Some(Some(days)))
}

/// `decodeURIComponent`: `None` for a malformed escape or bytes that are not UTF-8.
pub(crate) fn decode_component(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// `z.string().uuid()` on a profile path segment.
pub(crate) fn uuid(segment: &str) -> Result<(), Invalid> {
    if is_uuid(segment) {
        Ok(())
    } else {
        Err(Invalid(": Invalid UUID".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message<T: std::fmt::Debug>(result: Result<T, Invalid>) -> String {
        result.unwrap_err().0
    }

    #[test]
    fn names_are_trimmed_and_counted_in_utf16() {
        assert_eq!(
            profile_name(&json!({ "name": "  Work  " })).unwrap(),
            "Work"
        );
        assert_eq!(
            message(profile_name(&json!({ "name": " \u{feff} " }))),
            "name: Too small: expected string to have >=1 characters"
        );
        // 30 astral characters are 60 UTF-16 units; one more is too long.
        assert!(profile_name(&json!({ "name": "😀".repeat(30) })).is_ok());
        assert_eq!(
            message(profile_name(&json!({ "name": "😀".repeat(31) }))),
            "name: Too big: expected string to have <=60 characters"
        );
        // U+0085 is not JavaScript whitespace.
        assert_eq!(
            profile_name(&json!({ "name": "\u{85}" })).unwrap(),
            "\u{85}"
        );
    }

    #[test]
    fn shape_issues_come_in_zod_order_and_stop_refinements() {
        assert_eq!(
            message(profile_name(&json!({ "name": 1, "b": 1, "a": 2 }))),
            "name: Invalid input: expected string, received number; : Unrecognized keys: \"b\", \"a\""
        );
        assert_eq!(
            message(profile_name(&json!([]))),
            ": Invalid input: expected object, received array"
        );
        // An unknown key skips the project refinement; a bad project directory does not.
        assert_eq!(
            message(agent_setup(
                &json!({ "agent": "pi", "projectDirectory": "/x", "extra": 1 })
            )),
            ": Unrecognized key: \"extra\""
        );
        assert_eq!(
            message(agent_setup(
                &json!({ "agent": "pi", "projectDirectory": "", "otherProviders": true })
            )),
            "projectDirectory: Too small: expected string to have >=1 characters; projectDirectory: Choose an absolute project directory.; : Choose a project for Claude Code only.; : Only Claude Code and Codex choose whether to offer other providers."
        );
        assert_eq!(
            message(agent_setup(&json!({ "agent": "claude" }))),
            ": Choose a project for Claude Code only."
        );
        assert_eq!(
            message(agent_selection(&json!({ "agent": null }))),
            "agent: Invalid option: expected one of \"claude\"|\"codex\"|\"pi\"|\"opencode\""
        );
    }

    #[test]
    fn days_coerce_like_javascript_number() {
        assert_eq!(days(None, 3650).unwrap(), None);
        assert_eq!(days(Some(" 0x1e "), 3650).unwrap(), Some(30));
        assert_eq!(days(Some("1e1"), 3650).unwrap(), Some(10));
        assert_eq!(
            message(days(Some(""), 3650)),
            ": Too small: expected number to be >=1"
        );
        assert_eq!(
            message(days(Some("inf"), 3650)),
            ": Invalid input: expected number, received NaN"
        );
        assert_eq!(
            message(days(Some("0.5"), 3650)),
            ": Invalid input: expected int, received number; : Too small: expected number to be >=1"
        );
        assert_eq!(
            message(days(Some("3651"), 3650)),
            ": Too big: expected number to be <=3650"
        );
    }

    #[test]
    fn components_decode_like_decode_uri_component() {
        assert_eq!(decode_component("a%2Fb%20c").unwrap(), "a/b c");
        assert_eq!(decode_component("%C3%A9").unwrap(), "é");
        assert!(decode_component("%E0%A4%A").is_none());
        assert!(decode_component("%zz").is_none());
        assert!(decode_component("%C3").is_none());
    }

    #[test]
    fn api_accounts_report_nested_paths_and_keep_the_trimmed_key() {
        let input = api_account(
            &json!({
                "name": " Local ", "provider": "custom", "baseUrl": "http://127.0.0.1:1/v1",
                "apiKey": " sk ", "prefix": "", "models": [{ "id": " m ", "protocol": "openai", "x": 1 }],
            }),
        )
        .unwrap();
        assert_eq!(input.name, "Local");
        assert_eq!(input.api_key.expose_secret(), "sk");
        assert_eq!(input.protocol, Protocol::Openai);
        assert_eq!(input.models[0].id, "m");
        assert_eq!(
            message(api_account(&json!({
                "name": "x", "provider": "claude", "baseUrl": "nope", "apiKey": "k",
                "prefix": "A", "models": [{ "id": "", "protocol": "x" }],
            }))),
            "provider: Invalid option: expected one of \"opencode-go\"|\"openai\"|\"anthropic\"|\"gemini\"|\"custom\"; baseUrl: Invalid URL; prefix: Invalid string: must match pattern /^[a-z0-9-]*$/; models.0.id: Too small: expected string to have >=1 characters; models.0.protocol: Invalid option: expected one of \"openai\"|\"anthropic\"|\"responses\""
        );
    }
}
