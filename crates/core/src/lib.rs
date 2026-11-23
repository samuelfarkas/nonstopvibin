//! `CLIProxyAPI` cores: binary pins, config, spawn, watchdog, ledger, pool, management client and release installs.
#![forbid(unsafe_code)]

pub mod binary;
pub mod config;
pub mod install;
pub mod pool;
pub mod processes;
pub mod upstream;
mod yaml;

use std::io;

use nsv_domain::error::AppError;
use nsv_store::StoreError;

pub use binary::{check_core_directory, verify_core};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// An error the user sees, with its HTTP status.
    #[error(transparent)]
    App(#[from] AppError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// What the API answers for a core failure: its own error, or a hidden 500.
impl From<CoreError> for AppError {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::App(error) => error,
            CoreError::Store(error) => error.into(),
            other => Self::internal(other),
        }
    }
}
