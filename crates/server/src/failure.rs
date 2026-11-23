//! The one mapping from an [`AppError`] to what the client gets, for the API and the gateway
//! host alike: a hidden error answers the fixed text, and its cause goes to the error list.

use hyper::StatusCode;
use nsv_domain::error::{AppError, OPERATION_FAILED};
use nsv_gateway::Refusal;

use crate::daemon::Errors;

pub(crate) fn refusal(error: AppError, errors: &Errors) -> Refusal {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if error.hidden() {
        errors.report(error.message);
        return Refusal::new(status, OPERATION_FAILED);
    }
    Refusal::new(status, error.message)
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
