//! The commands a user gives the daemon from a menu: the tray's, which the shell draws, and the
//! macOS app menu's, which the window helper draws and forwards.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Command {
    OpenMain,
    ToggleQuota,
    CheckUpdates,
    Quit,
}

impl Command {
    /// The tray's label, which the app menu shares except for [`Self::app_menu_label`].
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::OpenMain => "Open NonstopVibin",
            Self::ToggleQuota => "Subscription quotas",
            Self::CheckUpdates => "Check for updates…",
            Self::Quit => "Quit NonstopVibin",
        }
    }

    /// The macOS app menu's label: beside the window's own items it names the main window.
    #[must_use]
    pub const fn app_menu_label(self) -> &'static str {
        match self {
            Self::OpenMain => "Open main window",
            other => other.label(),
        }
    }

    /// Whether its item is enabled: "Check for updates…" only where the app can update itself.
    #[must_use]
    pub const fn enabled(self, updates: bool) -> bool {
        !matches!(self, Self::CheckUpdates) || updates
    }
}
