//! The Claude seat a credential file belongs to (`src/server/identity.ts`): read from the file,
//! or asked of Claude for a signed-in token. Files may predate organization metadata, and a
//! missing identity never matches another seat.

use std::time::Duration;

use nsv_domain::error::AppError;
use nsv_domain::id::is_uuid;
use nsv_domain::js;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::fetch::{Outside, Request};
use crate::quota::CLAUDE_OAUTH_BETA;

const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const PROFILE_TIMEOUT: Duration = Duration::from_secs(15);
const PROFILE_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeatIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_name: Option<String>,
}

/// The seat fields of a credential file: trimmed UUIDs in lowercase, and an organization name
/// of 1–200 UTF-16 units without control characters. Anything else is left out.
#[must_use]
pub fn claude_identity(value: &Value) -> SeatIdentity {
    let raw = Some(value);
    let uuid = |key: &str| {
        js::text(js::field(raw, key))
            .map(js::trim)
            .filter(|text| is_uuid(text))
            .map(str::to_ascii_lowercase)
    };
    SeatIdentity {
        account_uuid: uuid("account_uuid"),
        organization_uuid: uuid("organization_uuid"),
        organization_name: label(js::field(raw, "organization_name")),
    }
}

/// A display label: trimmed, 1–200 UTF-16 units, no control characters.
fn label(value: Option<&Value>) -> Option<String> {
    js::text(value)
        .map(js::trim)
        .filter(|text| {
            (1..=200).contains(&js::utf16_len(text)) && !text.chars().any(char::is_control)
        })
        .map(str::to_owned)
}

/// Asks Claude whose token `raw` holds (`verifyClaudeIdentity`): a copy of the credential with
/// that seat, and that email when Claude names a valid one. The seat comes from the token,
/// never from imported labels or the browser's active organization.
///
/// # Errors
/// 502 for a credential without an access token, a failed or refused request, an unreadable
/// profile, or one without both UUIDs.
pub async fn verify_claude_identity(
    fetch: &dyn Outside,
    raw: &Map<String, Value>,
) -> Result<Map<String, Value>, AppError> {
    let token = js::text(raw.get("access_token"))
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            AppError::new(
                "Claude did not return a usable credential. Sign in again.",
                502,
            )
        })?;
    let response = fetch
        .https(Request {
            url: PROFILE_URL.to_owned(),
            headers: vec![
                ("Authorization", format!("Bearer {token}")),
                ("anthropic-beta", CLAUDE_OAUTH_BETA.to_owned()),
            ],
            timeout: PROFILE_TIMEOUT,
            limit: PROFILE_LIMIT,
        })
        .await
        .map_err(|_| {
            AppError::new(
                "Could not verify your Claude organization. Check your connection and try again.",
                502,
            )
        })?;
    if !response.ok() {
        return Err(AppError::new(
            format!(
                "Claude could not verify this organization (HTTP {}). Sign in again.",
                response.status.as_u16()
            ),
            502,
        ));
    }
    let profile: Value = serde_json::from_slice(&response.body).map_err(|_| {
        AppError::new(
            "Claude returned an unreadable account profile. Try again.",
            502,
        )
    })?;
    let account = js::field(Some(&profile), "account");
    let organization = js::field(Some(&profile), "organization");
    let identity = claude_identity(&json!({
        "account_uuid": js::field(account, "uuid"),
        "organization_uuid": js::field(organization, "uuid"),
        "organization_name": js::field(organization, "name"),
    }));
    let (Some(account_uuid), Some(organization_uuid)) =
        (identity.account_uuid, identity.organization_uuid)
    else {
        return Err(AppError::new(
            "Claude did not identify the account and organization. Sign in again and select your subscription.",
            502,
        ));
    };
    let mut verified = raw.clone();
    verified.insert("account_uuid".to_owned(), account_uuid.into());
    verified.insert("organization_uuid".to_owned(), organization_uuid.into());
    match identity.organization_name {
        Some(name) => verified.insert("organization_name".to_owned(), name.into()),
        None => verified.shift_remove("organization_name"),
    };
    if let Some(email) = label(js::field(account, "email")) {
        verified.insert("email".to_owned(), email.into());
    }
    Ok(verified)
}

/// Whether both name the same account in the same organization; unknown fields never match.
#[must_use]
pub fn same_seat(a: &SeatIdentity, b: &SeatIdentity) -> bool {
    matches!((&a.account_uuid, &a.organization_uuid), (Some(account), Some(organization))
        if !account.is_empty() && !organization.is_empty()
            && b.account_uuid.as_ref() == Some(account)
            && b.organization_uuid.as_ref() == Some(organization))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seat_matching_requires_both_uuids() {
        let ids = [
            "abcdef01-2345-4678-9abc-def012345678",
            "00000000-0000-4000-8000-000000000002",
            "00000000-0000-4000-8000-000000000003",
        ];
        let seat = |account: &str, organization: &str| {
            claude_identity(&json!({ "account_uuid": account, "organization_uuid": organization }))
        };
        for subject in ids {
            for organization in ids {
                let identity = seat(subject, organization);
                for other_subject in ids {
                    for other_organization in ids {
                        assert_eq!(
                            same_seat(&identity, &seat(other_subject, other_organization)),
                            subject == other_subject && organization == other_organization,
                            "{subject}/{organization} against {other_subject}/{other_organization}"
                        );
                    }
                }
                let account_only = SeatIdentity {
                    account_uuid: Some(subject.to_owned()),
                    ..SeatIdentity::default()
                };
                let organization_only = SeatIdentity {
                    organization_uuid: Some(organization.to_owned()),
                    ..SeatIdentity::default()
                };
                assert!(!same_seat(&identity, &account_only));
                assert!(!same_seat(&identity, &organization_only));
            }
        }
        assert!(!same_seat(
            &SeatIdentity::default(),
            &SeatIdentity::default()
        ));
    }
}
