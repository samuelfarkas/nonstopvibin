//! The pipe between the daemon and the window helper (`nonstopvibin-window`): one JSON object
//! per line on the helper's stdin and stdout. The daemon writes `Start`, then the session token
//! alone on the next line, so the token never appears in argv, the environment or a URL.
//! A daemon that failed to start or stop writes `Alert` instead, and nothing after it.

use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::id::ProfileId;

/// The longest line either side reads. An `openExternal` URL of 20,000 UTF-16 units escapes to
/// at most 120,000 bytes of JSON.
pub const MAX_LINE: usize = 256 * 1024;

/// A rectangle in logical pixels, as Electron's `Rectangle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// The daemon's origin, `http://<loopback host>:<port>`: the only origin a window may show and
/// the only origin whose pages reach the bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Origin(String);

/// An origin that is not `http://` on a loopback host with a port.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the daemon origin must be http://127.0.0.1, localhost or [::1] with a port")]
pub struct InvalidOrigin;

impl Origin {
    /// # Errors
    /// When `value` is not exactly `http://<127.0.0.1|localhost|[::1]>:<port>`.
    pub fn parse(value: &str) -> Result<Self, InvalidOrigin> {
        let port = ["127.0.0.1:", "localhost:", "[::1]:"]
            .iter()
            .find_map(|host| value.strip_prefix("http://")?.strip_prefix(host))
            .ok_or(InvalidOrigin)?;
        let valid = !port.starts_with('0')
            && port.bytes().all(|b| b.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|port| port > 0);
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidOrigin)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `url` is on this origin. The webview hands over normalized URLs, so a prefix match
    /// that must end the authority is exact: `http://127.0.0.1:1234@evil.example/` and
    /// `http://127.0.0.1:12345/` both fail.
    #[must_use]
    pub fn contains(&self, url: &str) -> bool {
        url.strip_prefix(self.0.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?', '#']))
    }
}

impl TryFrom<String> for Origin {
    type Error = InvalidOrigin;
    fn try_from(value: String) -> Result<Self, InvalidOrigin> {
        Self::parse(&value)
    }
}

impl From<Origin> for String {
    fn from(value: Origin) -> String {
        value.0
    }
}

/// The native dialogs the daemon may ask for. Their text is the helper's, never the daemon's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DialogKind {
    /// "Restart to update" or "Later"; Restart answers `RestartToUpdate`.
    UpdateReady,
    NoUpdates,
    UpdateFailed,
    MoveToApplications,
    CouldNotStart,
    CouldNotShutDown,
    /// The daemon could not stop for "Restart to update".
    CouldNotRestart,
    /// The update could not be swapped in or started.
    CouldNotInstall,
}

/// What failed when the daemon writes `Alert`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Failure {
    Start,
    Shutdown,
}

impl Failure {
    /// The title main.ts gave the error box.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Start => "NonstopVibin could not start",
            Self::Shutdown => "Could not shut down cleanly",
        }
    }

    /// The dialog the helper shows, with the reason in place of its text.
    #[must_use]
    pub const fn dialog(self) -> DialogKind {
        match self {
            Self::Start => DialogKind::CouldNotStart,
            Self::Shutdown => DialogKind::CouldNotShutDown,
        }
    }
}

/// The one free text that crosses the pipe: why the daemon failed, shown as a dialog's plain
/// text. Every construction, deserializing included, drops control characters except line
/// breaks, drops bidirectional controls and keeps at most [`Reason::MAX_CHARS`] characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub struct Reason(String);

impl Reason {
    pub const MAX_CHARS: usize = 1000;

    #[must_use]
    pub fn new(text: &str) -> Self {
        let mut kept = text.chars().filter(|&c| {
            c == '\n'
                || !(c.is_control()
                    || matches!(c, '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        });
        let mut reason: String = kept.by_ref().take(Self::MAX_CHARS).collect();
        if kept.next().is_some() {
            reason.pop();
            reason.push('…');
        }
        Self(reason)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Reason {
    fn from(value: String) -> Self {
        Self::new(&value)
    }
}

impl From<Reason> for String {
    fn from(value: Reason) -> Self {
        value.0
    }
}

/// What the daemon asks the helper to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "window", rename_all = "camelCase")]
pub enum View {
    /// The main window; a profile opens `/?profile=<id>`, and reloads an open window to it.
    Main {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<ProfileId>,
    },
    /// The quota popup, placed beside `anchor` inside the work area of the display that holds
    /// it: the tray icon's bounds, or on Linux the point the tray host reported for the click.
    /// The helper looks the work area up, since the daemon links no GUI library.
    Quota {
        anchor: Rect,
    },
    Dialog {
        kind: DialogKind,
    },
}

/// A window the helper reports closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "window", rename_all = "camelCase")]
pub enum ViewKind {
    Main,
    Quota,
    Dialog { kind: DialogKind },
}

/// The bridge methods the daemon serves. The daemon validates each argument again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "arg", rename_all = "camelCase")]
pub enum DaemonCall {
    OpenExternal(String),
    GetLoginItem,
    SetLoginItem(bool),
    RevealData,
}

/// A bridge call's answer: the method's JSON result, or the message the page's promise
/// rejects with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    Result(serde_json::Value),
    Error(String),
}

/// Daemon → helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum ToHelper {
    /// The first line. `bounds` is where the main window was when it last closed, sent with
    /// every start so a helper started for the popup can still restore it.
    Start {
        origin: Origin,
        view: View,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bounds: Option<Rect>,
        /// The data directory; Linux keeps the webview's storage in `<it>/webview`.
        data_directory: PathBuf,
        /// The app can update itself: the macOS app menu enables "Check for updates…".
        updates: bool,
    },
    /// The only line, in place of `Start`: an error dialog of its own, with no window, origin
    /// or token. The helper exits once it is dismissed.
    Alert { failure: Failure, reason: Reason },
    /// Opens a view, or focuses it when it is open.
    Show { view: View },
    Reply {
        id: u64,
        #[serde(flatten)]
        outcome: Outcome,
    },
    /// Closes the quota popup when it is open: a tray click toggles it.
    CloseQuota,
    /// Closes every window without asking the page, then exits.
    Quit,
}

/// Helper → daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum FromHelper {
    /// The first view is on screen.
    Ready,
    Closed {
        view: ViewKind,
        /// The main window's normal bounds, when it was neither maximized nor fullscreen.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bounds: Option<Rect>,
    },
    /// A page called a daemon method; answer with `Reply` and the same id.
    Bridge {
        id: u64,
        #[serde(flatten)]
        call: DaemonCall,
    },
    /// A page tried to open a new window; the daemon applies `openExternal`'s allowlist.
    OpenExternal {
        url: String,
    },
    /// `showWindow(profileId?)` or the "Open main window" menu item. The daemon checks the
    /// profile and answers with `Show`.
    OpenMain {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<ProfileId>,
    },
    ToggleQuota,
    CheckUpdates,
    QuitApp,
    /// "Restart to update" in the update-ready dialog.
    RestartToUpdate,
}

/// Reads one message line. `None` at the end of the stream.
///
/// # Errors
/// On I/O errors, a line longer than [`MAX_LINE`], or a line that is not a `T`.
pub fn read<T: DeserializeOwned>(reader: &mut impl BufRead) -> io::Result<Option<T>> {
    read_line(reader)?
        .map(|line| serde_json::from_slice(&line).map_err(io::Error::from))
        .transpose()
}

/// Reads one raw line without its newline, such as the session token. `None` at the end of
/// the stream.
///
/// # Errors
/// On I/O errors, or a line longer than [`MAX_LINE`] or cut off by the end of the stream.
pub fn read_line(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let limit = u64::try_from(MAX_LINE + 1).unwrap_or(u64::MAX);
    reader.take(limit).read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Ok(None);
    }
    if line.pop() != Some(b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "helper message too long or unterminated",
        ));
    }
    Ok(Some(line))
}

/// Writes one message line and flushes it.
///
/// # Errors
/// On I/O errors.
pub fn write<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wire<T: Serialize>(message: &T) -> serde_json::Value {
        serde_json::to_value(message).unwrap()
    }

    #[test]
    fn origin_accepts_only_loopback_http_with_a_port() {
        for good in [
            "http://127.0.0.1:4100",
            "http://localhost:1",
            "http://[::1]:65535",
        ] {
            assert!(Origin::parse(good).is_ok(), "{good}");
        }
        for bad in [
            "https://127.0.0.1:4100",
            "http://127.0.0.1",
            "http://127.0.0.1:",
            "http://127.0.0.1:0",
            "http://127.0.0.1:04100",
            "http://127.0.0.1:65536",
            "http://127.0.0.1:4100/",
            "http://example.com:4100",
            "http://127.0.0.1.example.com:4100",
        ] {
            assert!(Origin::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn origin_contains_its_own_urls_only() {
        let origin = Origin::parse("http://127.0.0.1:1234").unwrap();
        for inside in [
            "http://127.0.0.1:1234",
            "http://127.0.0.1:1234/",
            "http://127.0.0.1:1234/?view=tray",
            "http://127.0.0.1:1234#session=x",
        ] {
            assert!(origin.contains(inside), "{inside}");
        }
        for outside in [
            "http://127.0.0.1:12345/",
            "http://127.0.0.1:1234@example.com/",
            "http://127.0.0.1:1234.example.com/",
            "https://127.0.0.1:1234/",
            "http://localhost:1234/",
            "about:blank",
        ] {
            assert!(!origin.contains(outside), "{outside}");
        }
    }

    #[test]
    fn messages_have_the_documented_wire_shape() {
        let profile = ProfileId::parse("0b6f4f5e-6a7b-4c8d-9e0f-1a2b3c4d5e6f").unwrap();
        let anchor = Rect {
            x: 1800,
            y: 0,
            width: 24,
            height: 24,
        };
        let cases = [
            (
                wire(&ToHelper::Start {
                    origin: Origin::parse("http://127.0.0.1:4100").unwrap(),
                    view: View::Main {
                        profile: Some(profile.clone()),
                    },
                    bounds: None,
                    data_directory: PathBuf::from("/data"),
                    updates: true,
                }),
                json!({"type": "Start", "origin": "http://127.0.0.1:4100",
                       "view": {"window": "main", "profile": profile.as_str()},
                       "dataDirectory": "/data", "updates": true}),
            ),
            (
                wire(&ToHelper::Show {
                    view: View::Quota { anchor },
                }),
                json!({"type": "Show", "view": {"window": "quota",
                       "anchor": {"x": 1800, "y": 0, "width": 24, "height": 24}}}),
            ),
            (wire(&ToHelper::CloseQuota), json!({"type": "CloseQuota"})),
            (
                wire(&ToHelper::Reply {
                    id: 7,
                    outcome: Outcome::Result(json!(true)),
                }),
                json!({"type": "Reply", "id": 7, "result": true}),
            ),
            (
                wire(&ToHelper::Reply {
                    id: 8,
                    outcome: Outcome::Error("No.".into()),
                }),
                json!({"type": "Reply", "id": 8, "error": "No."}),
            ),
            (
                wire(&FromHelper::Bridge {
                    id: 3,
                    call: DaemonCall::SetLoginItem(true),
                }),
                json!({"type": "Bridge", "id": 3, "method": "setLoginItem", "arg": true}),
            ),
            (
                wire(&FromHelper::Bridge {
                    id: 4,
                    call: DaemonCall::GetLoginItem,
                }),
                json!({"type": "Bridge", "id": 4, "method": "getLoginItem"}),
            ),
            (
                wire(&FromHelper::Closed {
                    view: ViewKind::Dialog {
                        kind: DialogKind::UpdateReady,
                    },
                    bounds: None,
                }),
                json!({"type": "Closed", "view": {"window": "dialog", "kind": "updateReady"}}),
            ),
            (wire(&FromHelper::QuitApp), json!({"type": "QuitApp"})),
            (
                wire(&ToHelper::Alert {
                    failure: Failure::Start,
                    reason: Reason::new("vault.key is missing."),
                }),
                json!({"type": "Alert", "failure": "start", "reason": "vault.key is missing."}),
            ),
        ];
        for (actual, expected) in cases {
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn an_alert_reason_is_plain_bounded_text_even_from_the_pipe() {
        let line = json!({
            "type": "Alert",
            "failure": "shutdown",
            "reason": "a\tb\r\nc\u{1b}[31md\u{0}e\u{202e}f\u{2067}g",
        })
        .to_string();
        let message: Option<ToHelper> = read(&mut format!("{line}\n").as_bytes()).unwrap();
        assert_eq!(
            message,
            Some(ToHelper::Alert {
                failure: Failure::Shutdown,
                reason: Reason::new("ab\nc[31mdefg"),
            })
        );
        assert_eq!(Reason::new("ab\nc[31mdefg").as_str(), "ab\nc[31mdefg");
        let long = Reason::new(&"é".repeat(1500));
        assert_eq!(long.as_str(), format!("{}…", "é".repeat(999)));
        let exact = "x".repeat(1000);
        assert_eq!(Reason::new(&exact).as_str(), exact);
    }

    #[test]
    fn every_message_reads_back_from_its_line() {
        let messages = [
            FromHelper::Ready,
            FromHelper::Bridge {
                id: 1,
                call: DaemonCall::OpenExternal("https://a.example/".into()),
            },
            FromHelper::Bridge {
                id: 2,
                call: DaemonCall::RevealData,
            },
            FromHelper::OpenMain { profile: None },
            FromHelper::Closed {
                view: ViewKind::Main,
                bounds: Some(Rect {
                    x: -5,
                    y: 10,
                    width: 1240,
                    height: 880,
                }),
            },
        ];
        let mut buffer = Vec::new();
        for message in &messages {
            write(&mut buffer, message).unwrap();
        }
        let mut reader = buffer.as_slice();
        for message in &messages {
            assert_eq!(
                read::<FromHelper>(&mut reader).unwrap().as_ref(),
                Some(message)
            );
        }
        assert!(matches!(read::<FromHelper>(&mut reader), Ok(None)));
    }

    #[test]
    fn rejects_unknown_kinds_free_text_and_overlong_lines() {
        for line in [
            r#"{"type":"Show","view":{"window":"dialog","kind":"custom","text":"hi"}}"#,
            r#"{"type":"Show","view":{"window":"main","profile":"not-a-uuid"}}"#,
            r#"{"type":"Start","origin":"https://example.com","view":{"window":"main"},"dataDirectory":"/d"}"#,
            r#"{"type":"Eval","script":"alert(1)"}"#,
            r#"{"type":"Alert","failure":"custom","reason":"hi"}"#,
        ] {
            let input = format!("{line}\n");
            assert!(read::<ToHelper>(&mut input.as_bytes()).is_err(), "{line}");
        }
        let long = format!("\"{}\"\n", "a".repeat(MAX_LINE));
        assert!(read_line(&mut long.as_bytes()).is_err());
        assert!(read_line(&mut b"token-without-newline".as_slice()).is_err());
        let fits = format!("{}\n", "a".repeat(MAX_LINE));
        assert_eq!(
            read_line(&mut fits.as_bytes())
                .unwrap()
                .map(|line| line.len()),
            Some(MAX_LINE)
        );
    }
}
