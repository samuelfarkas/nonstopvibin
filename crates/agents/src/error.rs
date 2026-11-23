//! What agent setup returns when it refuses or fails.

use std::io;

use nsv_domain::error::AppError;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// A refusal the user sees as is, with its HTTP status.
    #[error(transparent)]
    App(#[from] AppError),
    /// Input that failed validation: the TypeScript server's zod errors, answered with 400.
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// What the API answers for an agent setup failure.
impl From<AgentError> for AppError {
    fn from(error: AgentError) -> Self {
        match error {
            AgentError::App(error) => error,
            AgentError::Invalid(message) => Self::new(message, 400),
            AgentError::Io(error) => Self::internal(error),
        }
    }
}

impl AgentError {
    /// A missing file or directory, which several reads treat as absent.
    pub(crate) fn is_missing(&self) -> bool {
        matches!(self, Self::Io(error) if error.kind() == io::ErrorKind::NotFound)
    }
}

/// `new AppError(message)`: a 400 the user sees.
pub(crate) fn refused(message: impl Into<String>) -> AgentError {
    AppError::new(message, 400).into()
}

/// `new AppError(message, 409)`.
pub(crate) fn conflicting(message: impl Into<String>) -> AgentError {
    AppError::new(message, 409).into()
}

/// A zod failure: `<path>: <message>`, the text the TypeScript routes answered with.
pub(crate) fn invalid(path: &str, message: &str) -> AgentError {
    AgentError::Invalid(format!("{path}: {message}"))
}

/// The refusal every merge raises when the user's settings collide with ours.
pub(crate) fn conflict() -> AgentError {
    conflicting(
        "Agent configuration already exists or was edited outside nonstopvibin. Resolve the conflicting settings before reconnecting or disconnecting.",
    )
}
