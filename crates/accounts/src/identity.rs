//! The Claude seat a credential file belongs to (`claudeIdentity` and `sameSeat` of
//! `src/server/identity.ts`). Files may predate organization metadata, and a missing identity
//! never matches another seat.

use nsv_domain::id::is_uuid;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::js;

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
    let organization_name = js::text(js::field(raw, "organization_name"))
        .map(js::trim)
        .filter(|name| {
            (1..=200).contains(&js::utf16_len(name)) && !name.chars().any(char::is_control)
        })
        .map(str::to_owned);
    SeatIdentity {
        account_uuid: uuid("account_uuid"),
        organization_uuid: uuid("organization_uuid"),
        organization_name,
    }
}

/// Whether both name the same account in the same organization; unknown fields never match.
#[must_use]
pub fn same_seat(a: &SeatIdentity, b: &SeatIdentity) -> bool {
    matches!((&a.account_uuid, &a.organization_uuid), (Some(account), Some(organization))
        if !account.is_empty() && !organization.is_empty()
            && b.account_uuid.as_ref() == Some(account)
            && b.organization_uuid.as_ref() == Some(organization))
}
