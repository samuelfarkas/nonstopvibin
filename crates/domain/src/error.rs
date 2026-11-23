//! The error a user sees: a message and the HTTP status the API answers with.

use std::fmt;

/// `AppError` of `src/server/errors.ts`. Its message is shown to the user as is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct AppError {
    pub message: String,
    pub status: u16,
}

impl AppError {
    /// TypeScript's `new AppError(message, status)`; its default status was 400.
    #[must_use]
    pub fn new(message: impl Into<String>, status: u16) -> Self {
        Self {
            message: message.into(),
            status,
        }
    }

    /// A failure that is not the user's: a 500 with its cause.
    #[must_use]
    pub fn internal(cause: impl fmt::Display) -> Self {
        Self::new(cause.to_string(), 500)
    }

    /// A request that arrived after shutdown began.
    #[must_use]
    pub fn shutting_down() -> Self {
        Self::new("nonstopvibin is shutting down.", 503)
    }
}
