//! Reading and replacing files in the user's agent configuration, with the refusals that keep a
//! symlinked, shared or foreign-owned file from being read or replaced. Every function blocks.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use nsv_store::files::{EntryKind, own_entry, read_own_file, write_atomic};
use serde_json::{Map, Value};

use crate::error::{AgentError, conflicting, refused};
use crate::json;

/// Largest agent configuration file read or written.
pub(crate) const FILE_LIMIT: usize = 1_000_000;

/// Creates `path` (with parents, 0700) when `create`, then checks it is an owner-only directory
/// without symbolic links. `false` when it is missing and `create` is off.
pub(crate) fn private_directory(path: &Path, create: bool) -> Result<bool, AgentError> {
    if create {
        DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    }
    match fs::symlink_metadata(path) {
        Ok(status) if own_entry(&status, EntryKind::Directory, 0o077) => Ok(true),
        Ok(_) => Err(refused(
            "Agent setup needs an owner-only directory without symbolic links.",
        )),
        Err(error) if !create && error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// `root/names...` where the user chose `root`: generated descendants never traverse symbolic
/// links. Intermediate directories are created (0700) when `create`; without it a missing one
/// gives the path under the canonical root.
pub(crate) fn config_path(
    root: &Path,
    names: &[String],
    create: bool,
) -> Result<PathBuf, AgentError> {
    let canonical = fs::canonicalize(root)?;
    let validate = |folder: &Path| -> Result<(), AgentError> {
        if own_entry(&fs::symlink_metadata(folder)?, EntryKind::Directory, 0o022) {
            Ok(())
        } else {
            Err(refused(
                "Agent configuration needs a directory you own, without symbolic links or shared write access.",
            ))
        }
    };
    validate(&canonical)?;
    let (last, parents) = names
        .split_last()
        .map_or((None, &[][..]), |(last, parents)| (Some(last), parents));
    let mut folder = canonical.clone();
    for name in parents {
        folder.push(name);
        if create {
            match DirBuilder::new().mode(0o700).create(&folder) {
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
                    return Err(error.into());
                }
                _ => {}
            }
        }
        match validate(&folder) {
            Err(error) if !create && error.is_missing() => {
                return Ok(names.iter().fold(canonical, |path, name| path.join(name)));
            }
            other => other?,
        }
    }
    Ok(last.map_or(folder.clone(), |last| folder.join(last)))
}

/// An agent configuration file, `None` when it is missing.
pub(crate) fn read_config(path: &Path) -> Result<Option<String>, AgentError> {
    read_own_file(path, 0o022, FILE_LIMIT as u64, || {
        refused(
            "Agent configuration must be an ordinary file you own, under 1 MB, without shared write access.",
        )
    })
    .map_err(|error| match error {
        AgentError::Io(cause) if cause.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) => {
            refused("Agent configuration cannot be a symbolic link.")
        }
        other => other,
    })
}

/// A JSON object, or the refusal the user sees for anything else.
pub(crate) fn json_object(content: &str) -> Result<Map<String, Value>, AgentError> {
    match json::parse(content) {
        Some(Value::Object(object)) => Ok(object),
        _ => Err(refused(
            "This agent configuration is not a valid JSON object. Fix it before connecting.",
        )),
    }
}

/// Replaces `path` with `after` (removes it for `None`) if it still holds `before`.
pub(crate) fn replace_config(
    path: &Path,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<(), AgentError> {
    if read_config(path)?.as_deref() != before {
        return Err(conflicting(
            "Agent configuration changed during setup. Try again.",
        ));
    }
    match after {
        None => remove_file(path)?,
        Some(content) => write_atomic(path, content.as_bytes(), 0o600)?,
    }
    Ok(())
}

/// `rm(path, { force: true })`: a missing file is already removed.
pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}
