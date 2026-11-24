//! What a client gets when a request fails, and the error list the interface shows. The one
//! mapping from an [`AppError`] serves the API and the gateway host alike: a hidden error answers
//! the fixed text, and its cause goes to the error list.

use std::sync::Mutex;

use hyper::header::{self, HeaderValue};
use hyper::{Response, StatusCode};
use nsv_domain::error::{AppError, OPERATION_FAILED};
use nsv_domain::sync::locked;
use nsv_gateway::{Body, Refusal};

/// The error list the interface shows: the last five distinct messages, oldest first.
#[derive(Debug, Default)]
pub(crate) struct Errors(Mutex<Vec<String>>);

impl Errors {
    pub(crate) fn report(&self, message: String) {
        let mut list = locked(&self.0);
        list.retain(|known| *known != message);
        list.push(message);
        let excess = list.len().saturating_sub(5);
        list.drain(..excess);
    }

    pub(crate) fn list(&self) -> Vec<String> {
        locked(&self.0).clone()
    }
}

pub(crate) fn refusal(error: AppError, errors: &Errors) -> Refusal {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if error.hidden() {
        errors.report(error.message);
        return Refusal::new(status, OPERATION_FAILED);
    }
    Refusal::new(status, error.message)
}

/// A refused upgrade: an empty answer that closes the connection, as the upgrade path wrote to
/// its socket.
pub(crate) fn closed(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_500_hides_its_cause_in_the_error_list_and_others_show_their_text() {
        let errors = Errors::default();
        let hidden = refusal(
            AppError::internal("unable to open /home/me/nonstopvibin.sqlite"),
            &errors,
        );
        assert_eq!(
            (hidden.status(), hidden.message()),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The operation failed. Check the profile status and try again."
            )
        );
        let shown = refusal(AppError::new("Profile not found.", 404), &errors);
        assert_eq!(
            (shown.status(), shown.message()),
            (StatusCode::NOT_FOUND, "Profile not found.")
        );
        assert_eq!(
            errors.list(),
            ["unable to open /home/me/nonstopvibin.sqlite"]
        );
    }
}
