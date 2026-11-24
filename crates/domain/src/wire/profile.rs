use serde::{Deserialize, Serialize};

use super::{Account, Extra};
use crate::id::{ProfileId, ProfileSlug};
use crate::registry::{RetentionDays, Strategy};

/// A profile as the `profiles.body` column stores it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Profile {
    pub id: ProfileId,
    pub slug: ProfileSlug,
    pub name: String,
    pub strategy: Strategy,
    pub session_affinity: bool,
    pub enabled: bool,
    pub created_at: String,
    #[serde(flatten)]
    #[cfg_attr(test, ts(skip))]
    pub extra: Extra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub enum Runtime {
    Stopped,
    Starting,
    Running,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct ProfileState {
    #[serde(flatten)]
    pub profile: Profile,
    pub runtime: Runtime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub accounts: Vec<Account>,
    pub endpoint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub enum CorePhase {
    Checking,
    Downloading,
    Testing,
    Restarting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct CoreState {
    /// The core this build bundles.
    pub bundled: String,
    /// Whether this run can install reviewed core updates (installed builds only).
    pub updates: bool,
    /// A newer reviewed core the last check found that this app may run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available: Option<String>,
    /// The update or revert in progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<CorePhase>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct AppState {
    pub profiles: Vec<ProfileState>,
    /// The core profiles start from: `core.bundled`, or an update installed over it.
    pub core_version: String,
    pub core: CoreState,
    pub gateway: String,
    pub usage_retention_days: RetentionDays,
    pub version: String,
    pub errors: Vec<String>,
}
