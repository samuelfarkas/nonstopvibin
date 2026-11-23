//! The tray as plain data: its look follows the daemon's status, its menu is main.ts's, and a
//! click toggles the quota popup. The platform trays draw this and report what the user chose.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use std::time::{Duration, Instant};

use nsv_domain::helper::Rect;

#[cfg(target_os = "linux")]
pub(crate) use linux::Tray;
#[cfg(target_os = "linux")]
pub use linux::{TrayLink, run_desktop};
#[cfg(target_os = "macos")]
pub(crate) use macos::Tray;
#[cfg(target_os = "macos")]
pub use macos::{TrayLink, run_desktop};

/// What the daemon reports for the tray.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrayStatus {
    /// The running profiles' names, in the order the tooltip lists them.
    pub running: Vec<String>,
    /// Whether any request is streaming.
    pub streaming: bool,
}

/// The colored mirrorball while a profile runs, the grey one when none does
/// (docs/design/icons.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Icon {
    Ball,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Look {
    pub(crate) icon: Icon,
    pub(crate) tooltip: String,
}

impl Look {
    /// `syncTray` in main.ts.
    pub(crate) fn of(status: &TrayStatus) -> Self {
        if status.running.is_empty() {
            return Self {
                icon: Icon::Stopped,
                tooltip: "NonstopVibin · proxy off".to_owned(),
            };
        }
        let activity = if status.streaming {
            "streaming"
        } else {
            "idle"
        };
        Self {
            icon: Icon::Ball,
            tooltip: format!("NonstopVibin — {} · {activity}", status.running.join(", ")),
        }
    }
}

/// What the user chose in the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// A left click or "Subscription quotas". `anchor` is the icon's bounds, or the point a
    /// Linux tray host reported; `None` keeps the last one.
    ToggleQuota(Option<Rect>),
    OpenMain,
    CheckUpdates,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Entry {
    Item { label: &'static str, action: Action },
    Separator,
}

/// "Check for updates…" is enabled only where the app can update itself.
pub(crate) fn enabled(action: Action, updates: bool) -> bool {
    action != Action::CheckUpdates || updates
}

/// The tray menu, as main.ts built it.
pub(crate) const MENU: [Entry; 5] = [
    Entry::Item {
        label: "Open NonstopVibin",
        action: Action::OpenMain,
    },
    Entry::Item {
        label: "Subscription quotas",
        action: Action::ToggleQuota(None),
    },
    Entry::Item {
        label: "Check for updates…",
        action: Action::CheckUpdates,
    },
    Entry::Separator,
    Entry::Item {
        label: "Quit NonstopVibin",
        action: Action::Quit,
    },
];

/// A click this soon after the popup closed on focus loss is the click that took the focus
/// away, so it must not open the popup again.
const REOPEN_GUARD: Duration = Duration::from_millis(250);

/// The quota popup as the daemon knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Popup {
    Open,
    Closed { at: Option<Instant> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Toggle {
    Open,
    Close,
    Ignore,
}

impl Popup {
    pub(crate) fn toggle(self, now: Instant) -> Toggle {
        match self {
            Self::Open => Toggle::Close,
            Self::Closed { at: Some(at) } if now.saturating_duration_since(at) < REOPEN_GUARD => {
                Toggle::Ignore
            }
            Self::Closed { .. } => Toggle::Open,
        }
    }
}

/// Decodes one of the tray PNGs to RGBA rows.
fn rgba(png_bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), png::DecodingError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let mut buffer = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let frame = reader.next_frame(&mut buffer)?;
    buffer.truncate(frame.buffer_size());
    Ok((buffer, frame.width, frame.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(running: &[&str], streaming: bool) -> TrayStatus {
        TrayStatus {
            running: running.iter().map(|name| (*name).to_owned()).collect(),
            streaming,
        }
    }

    #[test]
    fn the_look_follows_main_ts() {
        let cases = [
            (
                status(&[], false),
                Icon::Stopped,
                "NonstopVibin · proxy off",
            ),
            // Streaming without a running profile cannot colour the ball.
            (status(&[], true), Icon::Stopped, "NonstopVibin · proxy off"),
            (
                status(&["Work"], false),
                Icon::Ball,
                "NonstopVibin — Work · idle",
            ),
            (
                status(&["Work", "Side project"], true),
                Icon::Ball,
                "NonstopVibin — Work, Side project · streaming",
            ),
        ];
        for (status, icon, tooltip) in cases {
            assert_eq!(
                Look::of(&status),
                Look {
                    icon,
                    tooltip: tooltip.to_owned()
                }
            );
        }
    }

    #[test]
    fn the_menu_is_main_ts_menu_with_updates_enabled_only_where_possible() {
        for updates in [false, true] {
            let shown: Vec<(&str, bool)> = MENU
                .iter()
                .map(|entry| match entry {
                    Entry::Item { label, action } => (*label, enabled(*action, updates)),
                    Entry::Separator => ("---", false),
                })
                .collect();
            assert_eq!(
                shown,
                [
                    ("Open NonstopVibin", true),
                    ("Subscription quotas", true),
                    ("Check for updates…", updates),
                    ("---", false),
                    ("Quit NonstopVibin", true),
                ]
            );
        }
    }

    #[test]
    fn a_click_right_after_a_focus_loss_close_does_not_reopen() {
        let closed = Instant::now();
        let at = |ms| closed + Duration::from_millis(ms);
        let popup = Popup::Closed { at: Some(closed) };
        assert_eq!(popup.toggle(at(0)), Toggle::Ignore);
        assert_eq!(popup.toggle(at(249)), Toggle::Ignore);
        assert_eq!(popup.toggle(at(250)), Toggle::Open);
        assert_eq!(Popup::Closed { at: None }.toggle(at(0)), Toggle::Open);
        assert_eq!(Popup::Open.toggle(at(0)), Toggle::Close);
    }

    #[test]
    fn the_tray_icons_decode_at_their_documented_sizes() {
        for (bytes, size) in [
            (
                &include_bytes!("../../../public/tray/linux-ball.png")[..],
                22,
            ),
            (include_bytes!("../../../public/tray/linux-stopped.png"), 22),
            (include_bytes!("../../../public/tray/mac-ball@2x.png"), 36),
            (
                include_bytes!("../../../public/tray/mac-stopped@2x.png"),
                36,
            ),
        ] {
            let (pixels, width, height) = rgba(bytes).unwrap();
            assert_eq!((width, height), (size, size));
            assert_eq!(pixels.len(), usize::try_from(size * size * 4).unwrap());
        }
    }
}
