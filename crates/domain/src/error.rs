//! The error a user sees: a message and the HTTP status the API answers with, or a 500 whose
//! cause only the error list shows.

use std::fmt;

/// What every 500 answers, as main's `server.ts` answered an error it did not make itself.
pub const OPERATION_FAILED: &str = "The operation failed. Check the profile status and try again.";

/// `AppError` of `src/server/errors.ts`. Any status but 500 shows its message as is. A 500 is
/// hidden: the client gets [`OPERATION_FAILED`], and the message, its cause, goes to the error
/// list.
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

    /// A failure the user must not read: a 500 whose cause goes to the error list.
    #[must_use]
    pub fn internal(cause: impl fmt::Display) -> Self {
        Self::new(cause.to_string(), 500)
    }

    /// A request that arrived after shutdown began.
    #[must_use]
    pub fn shutting_down() -> Self {
        Self::new("nonstopvibin is shutting down.", 503)
    }

    /// A request path that is not a URL or holds a segment `decodeURIComponent` rejects.
    #[must_use]
    pub fn malformed_path() -> Self {
        Self::new("Malformed request path.", 400)
    }

    /// A method the path does not serve.
    #[must_use]
    pub fn method_not_allowed() -> Self {
        Self::new("Method not allowed.", 405)
    }

    /// A profile id that names no profile.
    #[must_use]
    pub fn profile_not_found() -> Self {
        Self::new("Profile not found.", 404)
    }

    /// An account id that names none of the profile's accounts.
    #[must_use]
    pub fn account_not_found() -> Self {
        Self::new("Account not found in this profile.", 404)
    }

    /// An operation that needs the profile's core while it has had none.
    #[must_use]
    pub fn not_started() -> Self {
        Self::new("Start this profile first.", 409)
    }

    /// Whether the user gets [`OPERATION_FAILED`] instead of the message.
    #[must_use]
    pub const fn hidden(&self) -> bool {
        self.status == 500
    }
}
