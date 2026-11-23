//! The tray as plain data: its look follows the daemon's status, its menu is main.ts's, and a
//! click toggles the quota popup. The platform trays draw this and report what the user chose.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use std::time::{Duration, Instant};

use nsv_desktop::pipe::Anchor;
use nsv_desktop::{Command, TrayStatus};

#[cfg(target_os = "linux")]
pub(crate) use linux::{Link, Tray};
#[cfg(target_os = "macos")]
pub(crate) use macos::{Link, StatusItem, Tray};

/// What the main thread hands the daemon thread ([`crate::run_desktop`]).
#[derive(Debug)]
pub struct TrayLink {
    /// Opened by the login item: the daemon starts without a window, as with `--background`.
    pub(crate) login_launch: bool,
    pub(crate) tray: Link,
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

/// What the user chose in the tray: a left click toggles the quota popup, or a menu item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Action {
    pub(crate) command: Command,
    /// Where the icon is, when the tray knows; `None` keeps the last one.
    pub(crate) anchor: Option<Anchor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Entry {
    Item(Command),
    Separator,
}

/// The tray menu, as main.ts built it.
pub(crate) const MENU: [Entry; 5] = [
    Entry::Item(Command::OpenMain),
    Entry::Item(Command::ToggleQuota),
    Entry::Item(Command::CheckUpdates),
    Entry::Separator,
    Entry::Item(Command::Quit),
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
