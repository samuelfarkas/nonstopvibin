//! Credential files other `CLIProxyAPI` apps left, offered for import (`src/server/imports.ts`).
//! Only known local installations are scanned; the renderer never supplies a path, only an id
//! a scan handed out.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use nsv_domain::error::AppError;
use nsv_domain::registry::{Provider, RegistryError, registry};
use nsv_domain::wire::ImportSource;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::identity::{SeatIdentity, claude_identity};
use crate::js;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error(transparent)]
    App(#[from] AppError),
    #[error("could not read the account file: {0}")]
    Io(#[from] io::Error),
    #[error("the account file is not JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

impl ImportError {
    /// The HTTP status the API answers with.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::App(error) => error.status,
            Self::Io(_) | Self::Json(_) | Self::Registry(_) => 500,
        }
    }
}

/// The importable files of the last scan, by id. Scanning and reading block on the file
/// system; run them off the runtime thread.
#[derive(Debug)]
pub struct ImportCatalog {
    directories: Vec<PathBuf>,
    files: HashMap<String, PathBuf>,
}

impl ImportCatalog {
    /// `EasyCLIProxyAPI`'s macOS and Linux folders and `CLIProxyAPI`'s, under `home`.
    #[must_use]
    pub fn new(home: &Path) -> Self {
        Self {
            directories: vec![
                home.join("Library/Application Support/com.cpa.gui/oauth"),
                home.join(".local/share/com.cpa.gui/oauth"),
                home.join(".cli-proxy-api"),
            ],
            files: HashMap::new(),
        }
    }

    /// Lists each directory's regular `*.json` files whose `type` is a provider with credential
    /// files. Missing directories and unrelated, unreadable or malformed files are skipped
    /// without exposing their contents. Ids are the SHA-256 of the path, in hexadecimal.
    ///
    /// # Errors
    /// Only when the embedded registry is broken.
    pub fn scan(&mut self) -> Result<Vec<ImportSource>, ImportError> {
        let registry = registry()?;
        let mut found = Vec::new();
        self.files.clear();
        for directory in &self.directories {
            let Ok(entries) = fs::read_dir(directory) else {
                continue;
            };
            let source = if directory.to_string_lossy().contains("com.cpa.gui") {
                "EasyCLIProxyAPI"
            } else {
                "CLIProxyAPI"
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                // `.json` exactly, as `endsWith(".json")`: `x.JSON` is not scanned.
                #[expect(
                    clippy::case_sensitive_file_extension_comparisons,
                    reason = "the TypeScript rule"
                )]
                if !entry.file_type().is_ok_and(|kind| kind.is_file()) || !name.ends_with(".json") {
                    continue;
                }
                let path = directory.join(entry.file_name());
                let Ok(raw) = read(&path) else {
                    continue;
                };
                let raw = Value::Object(raw);
                let kind = js::string(js::field(Some(&raw), "type")).into_owned();
                let entry =
                    Provider::from_id(&kind).map(|provider| registry.providers.get(provider));
                if !entry.is_some_and(|entry| entry.credential_file) {
                    continue;
                }
                let identity = entry
                    .filter(|entry| entry.organizations)
                    .map_or_else(SeatIdentity::default, |_| claude_identity(&raw));
                let id = sha256_hex(path.to_string_lossy().as_bytes());
                self.files.insert(id.clone(), path);
                found.push(ImportSource {
                    id,
                    file_name: name,
                    provider: kind,
                    email: js::text(js::field(Some(&raw), "email")).map(str::to_owned),
                    source: source.to_owned(),
                    account_uuid: identity.account_uuid,
                    organization_uuid: identity.organization_uuid,
                    organization_name: identity.organization_name,
                });
            }
        }
        Ok(found)
    }

    /// The JSON object of a file the last scan found (any other JSON reads as `{}`).
    ///
    /// # Errors
    /// 404 for an id the last scan did not hand out, 400 for a file that is no longer a regular
    /// file within the size limit, or the read or parse failure.
    pub fn contents(&self, id: &str) -> Result<Map<String, Value>, ImportError> {
        let path = self.files.get(id).ok_or_else(|| {
            AppError::new(
                "Refresh the detected accounts and select the file again.",
                404,
            )
        })?;
        read(path)
    }
}

fn read(path: &Path) -> Result<Map<String, Value>, ImportError> {
    let file = fs::symlink_metadata(path)?;
    if !file.is_file() || file.len() > registry()?.limits.credential_file_bytes {
        return Err(AppError::new(
            "The selected account file is not a supported regular file.",
            400,
        )
        .into());
    }
    let content = fs::read(path)?;
    match serde_json::from_str(&String::from_utf8_lossy(&content))? {
        Value::Object(object) => Ok(object),
        _ => Ok(Map::new()),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}
