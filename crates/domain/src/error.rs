//! The error a user sees: a message and the HTTP status the API answers with.

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
}
