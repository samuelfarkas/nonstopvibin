//! Starting `claude`: where it is, its arguments and environment, and how it dies with the
//! daemon.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::conversation::Settings;

/// The entrypoint label Claude Code reports to Anthropic for these requests.
pub(crate) const ENTRYPOINT: &str = "nonstopvibin";

/// What `claude` inherits, each only when set and not empty.
const INHERITED: [&str; 5] = ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"];

/// The switches that keep Claude Code to the harness's conversation: no `CLAUDE.md` files,
/// memory, compaction, tool search, advisor, fallbacks or away summaries. MCP tool output is
/// capped where a harness's own tool output would not be.
const SWITCHES: [(&str, &str); 11] = [
    ("CLAUDE_CODE_ENTRYPOINT", ENTRYPOINT),
    ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
    ("DISABLE_AUTO_COMPACT", "1"),
    ("ENABLE_TOOL_SEARCH", "false"),
    ("CLAUDE_CODE_DISABLE_ADVISOR_TOOL", "1"),
    ("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "1"),
    ("CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK", "1"),
    ("CLAUDE_CODE_ENABLE_AWAY_SUMMARY", "0"),
    ("CLAUDE_CODE_POLISHED_DEWDROP", "drop"),
    ("MAX_MCP_OUTPUT_TOKENS", "200000"),
];

/// `claude` as found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) path: PathBuf,
    /// Found on `PATH`, so a shell runs it as `claude`.
    pub(crate) on_path: bool,
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|status| status.is_file() && status.permissions().mode() & 0o111 != 0)
}

/// `claude` on `path`, else in `~/.local/bin` or `~/.claude/local`, where its installers put
/// it. Blocks on the file system.
pub(crate) fn find(path: Option<&OsStr>, home: Option<&Path>) -> Option<Found> {
    let listed = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join("claude"))
        .find(|candidate| executable(candidate));
    if let Some(path) = listed {
        return Some(Found {
            path,
            on_path: true,
        });
    }
    let home = home?;
    [".local/bin/claude", ".claude/local/claude"]
        .iter()
        .map(|relative| home.join(relative))
        .find(|candidate| executable(candidate))
        .map(|path| Found {
            path,
            on_path: false,
        })
}

/// A session's place on disk: a new one, one to resume, or none at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Persistence {
    New(String),
    Resume(String),
    None,
}

/// What a process is started for.
#[derive(Debug)]
pub(crate) enum Mode<'a> {
    /// Only its `initialize` answer is read.
    Probe,
    /// It runs conversation turns.
    Turns {
        settings: &'a Settings,
        /// The owner-only file holding the harness's system prompt.
        system: &'a Path,
        /// The owner-only `--mcp-config` file; `None` without tools.
        mcp: Option<&'a Path>,
        persistence: &'a Persistence,
    },
}

/// `claude`'s arguments: stream-json both ways, no built-in tools, no settings files, only
/// the harness's MCP server, and nothing that prompts.
pub(crate) fn arguments(mode: &Mode<'_>) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--tools",
        "",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--permission-mode",
        "dontAsk",
        "--permission-prompts",
        "none",
        "--settings",
        r#"{"disableAllHooks":true}"#,
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    let mut add = |items: &[&OsStr]| arguments.extend(items.iter().map(OsString::from));
    match mode {
        Mode::Probe => add(&["--no-session-persistence".as_ref()]),
        Mode::Turns {
            settings,
            system,
            mcp,
            persistence,
        } => {
            add(&[
                "--include-partial-messages".as_ref(),
                "--allowedTools".as_ref(),
                "mcp__client__*".as_ref(),
                "--model".as_ref(),
                settings.model.as_ref(),
                "--thinking-display".as_ref(),
                "summarized".as_ref(),
                "--system-prompt-snapshot".as_ref(),
                "off".as_ref(),
                "--system-prompt-file".as_ref(),
                system.as_os_str(),
            ]);
            if let Some(effort) = settings.effort {
                add(&["--effort".as_ref(), effort.as_str().as_ref()]);
            }
            if let Some(mcp) = mcp {
                add(&["--mcp-config".as_ref(), mcp.as_os_str()]);
            }
            match persistence {
                Persistence::New(id) => add(&["--session-id".as_ref(), id.as_ref()]),
                Persistence::Resume(id) => add(&["--resume".as_ref(), id.as_ref()]),
                Persistence::None => add(&["--no-session-persistence".as_ref()]),
            }
        }
    }
    arguments
}

/// `claude`'s environment: [`INHERITED`] as `inherited` answers it, the profile's Claude
/// configuration directory, the [`SWITCHES`] and the request's output token limit.
pub(crate) fn environment(
    config: &Path,
    max_tokens: Option<u64>,
    inherited: impl Fn(&str) -> Option<OsString>,
) -> Vec<(OsString, OsString)> {
    let mut variables: Vec<(OsString, OsString)> = INHERITED
        .iter()
        .filter_map(|name| {
            inherited(name)
                .filter(|value| !value.is_empty())
                .map(|value| ((*name).into(), value))
        })
        .collect();
    variables.push(("CLAUDE_CONFIG_DIR".into(), config.into()));
    variables.extend(
        SWITCHES
            .iter()
            .map(|(name, value)| ((*name).into(), (*value).into())),
    );
    if let Some(tokens) = max_tokens {
        variables.push((
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(),
            tokens.to_string().into(),
        ));
    }
    variables
}

/// The command for `program` in `work`: only `environment`, pipes for stdin and stdout, and
/// killed with the daemon. On Linux the kernel sends SIGKILL when the thread that started it
/// ends, which is the daemon's runtime thread; elsewhere it exits once its stdin closes.
pub(crate) fn command(
    program: &Path,
    work: &Path,
    arguments: &[OsString],
    environment: Vec<(OsString, OsString)>,
) -> Command {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(work)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(target_os = "linux")]
    die_with_parent(&mut command);
    command
}

#[cfg(target_os = "linux")]
fn die_with_parent(command: &mut Command) {
    use rustix::process::{Signal, getppid, set_parent_process_death_signal};
    let parent = rustix::process::getpid();
    #[expect(
        unsafe_code,
        reason = "pre_exec is unsafe: the closure runs in the forked child before exec"
    )]
    // SAFETY: the closure makes two system calls, prctl and getppid, which are
    // async-signal-safe; it allocates nothing and touches no lock.
    unsafe {
        command.pre_exec(move || {
            set_parent_process_death_signal(Some(Signal::KILL))?;
            // The daemon may have died between fork and prctl, and the signal with it.
            if getppid() != Some(parent) {
                return Err(std::io::Error::other("the daemon exited"));
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::conversation::Effort;

    fn strings(arguments: &[OsString]) -> Vec<&str> {
        arguments.iter().map(|a| a.to_str().unwrap()).collect()
    }

    const ISOLATION: [&str; 17] = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--tools",
        "",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--permission-mode",
        "dontAsk",
        "--permission-prompts",
        "none",
        "--settings",
        r#"{"disableAllHooks":true}"#,
    ];

    #[test]
    fn a_session_runs_the_harnesses_model_prompt_and_tools_and_nothing_of_its_own() {
        let settings = Settings {
            model: "claude-opus-5-5".to_owned(),
            effort: Some(Effort::Xhigh),
            max_tokens: Some(32000),
            system: String::new(),
            tools: Arc::new([]),
        };
        let arguments = arguments(&Mode::Turns {
            settings: &settings,
            system: Path::new("/run/s.system"),
            mcp: Some(Path::new("/run/s.mcp.json")),
            persistence: &Persistence::New("0b5a4f3e-1c2d-4e5f-8a9b-0c1d2e3f4a5b".to_owned()),
        });
        let mut expected = ISOLATION.to_vec();
        expected.extend([
            "--include-partial-messages",
            "--allowedTools",
            "mcp__client__*",
            "--model",
            "claude-opus-5-5",
            "--thinking-display",
            "summarized",
            "--system-prompt-snapshot",
            "off",
            "--system-prompt-file",
            "/run/s.system",
            "--effort",
            "xhigh",
            "--mcp-config",
            "/run/s.mcp.json",
            "--session-id",
            "0b5a4f3e-1c2d-4e5f-8a9b-0c1d2e3f4a5b",
        ]);
        assert_eq!(strings(&arguments), expected);
        let resumed = super::arguments(&Mode::Turns {
            settings: &Settings {
                effort: None,
                ..settings.clone()
            },
            system: Path::new("/run/s.system"),
            mcp: None,
            persistence: &Persistence::Resume("id".to_owned()),
        });
        assert_eq!(strings(&resumed[resumed.len() - 2..]), ["--resume", "id"]);
        assert!(!strings(&resumed).contains(&"--effort"));
        assert!(!strings(&resumed).contains(&"--mcp-config"));
        let probe = super::arguments(&Mode::Probe);
        let mut expected = ISOLATION.to_vec();
        expected.push("--no-session-persistence");
        assert_eq!(strings(&probe), expected);
    }

    #[test]
    fn the_environment_holds_only_the_inherited_basics_and_the_switches() {
        let variables = environment(
            Path::new("/data/claude-agent/p"),
            Some(4096),
            |name| match name {
                "PATH" => Some("/usr/bin".into()),
                "LANG" => Some(OsString::new()),
                "ANTHROPIC_API_KEY" | "CLAUDE_CODE_OAUTH_TOKEN" => Some("secret".into()),
                _ => None,
            },
        );
        let variables: Vec<(&str, &str)> = variables
            .iter()
            .map(|(name, value)| (name.to_str().unwrap(), value.to_str().unwrap()))
            .collect();
        assert_eq!(
            variables,
            [
                ("PATH", "/usr/bin"),
                ("CLAUDE_CONFIG_DIR", "/data/claude-agent/p"),
                ("CLAUDE_CODE_ENTRYPOINT", "nonstopvibin"),
                ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
                ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
                ("DISABLE_AUTO_COMPACT", "1"),
                ("ENABLE_TOOL_SEARCH", "false"),
                ("CLAUDE_CODE_DISABLE_ADVISOR_TOOL", "1"),
                ("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "1"),
                ("CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK", "1"),
                ("CLAUDE_CODE_ENABLE_AWAY_SUMMARY", "0"),
                ("CLAUDE_CODE_POLISHED_DEWDROP", "drop"),
                ("MAX_MCP_OUTPUT_TOKENS", "200000"),
                ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "4096"),
            ]
        );
    }

    #[test]
    fn claude_is_found_on_the_path_first_then_where_its_installers_put_it() {
        let root = nsv_testkit::TempDir::new("find-claude");
        let (bin, home) = (root.path().join("bin"), root.path().join("home"));
        let local = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&local).unwrap();
        nsv_testkit::write_executable(&local.join("claude"), b"#!/bin/sh\n");
        let path = std::env::join_paths([&bin]).unwrap();
        assert_eq!(
            find(Some(&path), Some(&home)),
            Some(Found {
                path: local.join("claude"),
                on_path: false
            })
        );
        std::fs::write(bin.join("claude"), b"not executable").unwrap();
        assert_eq!(
            find(Some(&path), Some(&home)).unwrap().path,
            local.join("claude")
        );
        nsv_testkit::write_executable(&bin.join("claude"), b"#!/bin/sh\n");
        assert_eq!(
            find(Some(&path), Some(&home)),
            Some(Found {
                path: bin.join("claude"),
                on_path: true
            })
        );
        assert_eq!(find(None, None), None);
    }
}
