//! The pipe between the daemon and the window helper (`nonstopvibin-window`): one JSON object
//! per line on the helper's stdin and stdout. The handshake opens it: [`handshake`] writes
//! `Start` and then the session token alone on the next line, so the token never appears in
//! argv, the environment or a URL, and [`read_handshake`] reads them back. A daemon that failed
//! to start or stop writes `Alert` instead, and nothing after it.

use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;

use nsv_domain::endpoint::Origin;
use nsv_domain::id::ProfileId;
use nsv_domain::secret::SessionToken;
use serde::de::{self, DeserializeOwned};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::Command;
use crate::bridge::{Method, OPEN_EXTERNAL_LIMIT};

/// The longest line either side reads.
pub const MAX_LINE: usize = 256 * 1024;

// An `openExternal` URL whose UTF-16 units each escape to six bytes of JSON still fits a line.
const _: () = assert!(6 * OPEN_EXTERNAL_LIMIT + 1024 <= MAX_LINE);

/// A rectangle in logical pixels, as Electron's `Rectangle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Where the tray icon is, for the quota popup: logical pixels (points on macOS) on the
/// desktop, with the origin at the top left of the first screen and y growing down. Each
/// process converts its toolkit's coordinates at its own boundary. A Linux tray host reports
/// only the point of the click: an anchor without a size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Anchor {
    #[must_use]
    pub const fn point(x: i32, y: i32) -> Self {
        Self {
            x,
            y,
            width: 0,
            height: 0,
        }
    }
}

/// The native dialogs the daemon may ask for. Their text and buttons are the helper's, never
/// the daemon's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DialogKind {
    /// "Restart to update" ([`Choice::Accept`]) or "Later".
    UpdateReady,
    NoUpdates,
    UpdateFailed,
    MoveToApplications,
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
    /// The title main.ts gave the error box: the helper's dialog, or the desktop notification
    /// the daemon posts when no helper can show it.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Start => "NonstopVibin could not start",
            Self::Shutdown => "Could not shut down cleanly",
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
    /// it. The helper looks the work area up, since the daemon links no GUI library.
    Quota {
        anchor: Anchor,
    },
    Dialog {
        kind: DialogKind,
    },
}

/// What the user chose in a dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Choice {
    /// The dialog's action: "Restart to update".
    Accept,
    /// Its other button, or the dialog closed without one.
    Dismiss,
}

/// A window the helper reports closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "window", rename_all = "camelCase")]
pub enum ViewKind {
    Main,
    Quota,
    Dialog { kind: DialogKind, choice: Choice },
}

/// The bridge methods the daemon serves, named by [`Method`]. The daemon validates each
/// argument again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonCall {
    OpenExternal(String),
    GetLoginItem,
    SetLoginItem(bool),
    RevealData,
}

impl DaemonCall {
    /// The call `method` makes with `arg`, when the daemon serves `method` and `arg` has its
    /// type.
    #[must_use]
    pub fn new(method: Method, arg: Option<Value>) -> Option<Self> {
        match (method, arg) {
            (Method::OpenExternal, Some(Value::String(url))) => Some(Self::OpenExternal(url)),
            (Method::GetLoginItem, None) => Some(Self::GetLoginItem),
            (Method::SetLoginItem, Some(Value::Bool(enabled))) => Some(Self::SetLoginItem(enabled)),
            (Method::RevealData, None) => Some(Self::RevealData),
            _ => None,
        }
    }

    #[must_use]
    pub const fn method(&self) -> Method {
        match self {
            Self::OpenExternal(_) => Method::OpenExternal,
            Self::GetLoginItem => Method::GetLoginItem,
            Self::SetLoginItem(_) => Method::SetLoginItem,
            Self::RevealData => Method::RevealData,
        }
    }
}

/// `{"method": <name>, "arg": <argument>}`, without `arg` for a method that takes none.
impl Serialize for DaemonCall {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("method", self.method().name())?;
        match self {
            Self::OpenExternal(url) => map.serialize_entry("arg", url)?,
            Self::SetLoginItem(enabled) => map.serialize_entry("arg", enabled)?,
            Self::GetLoginItem | Self::RevealData => {}
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for DaemonCall {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            method: String,
            #[serde(default)]
            arg: Option<Value>,
        }
        let Raw { method, arg } = Raw::deserialize(deserializer)?;
        Method::named(&method)
            .and_then(|method| Self::new(method, arg))
            .ok_or_else(|| de::Error::custom(format!("not a daemon call: {method}")))
    }
}

/// A bridge call's answer: the method's JSON result, or the message the page's promise
/// rejects with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    Result(Value),
    Error(String),
}

/// What the helper starts with. `bounds` is where the main window was when it last closed,
/// sent with every start so a helper started for the popup can still restore it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    pub origin: Origin,
    pub view: View,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Rect>,
    /// The data directory's webview storage, which Linux keeps for the owner only.
    pub webview: PathBuf,
    /// The app can update itself: the app menu enables "Check for updates…".
    pub updates: bool,
}

/// Daemon → helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum ToHelper {
    /// The first line, which [`handshake`] writes.
    Start(Start),
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
        /// It was the helper's last window: the helper exits, and the next view needs a new
        /// one. A helper that failed reports that by its exit status instead.
        last: bool,
    },
    /// A page called a daemon method; answer with `Reply` and the same id.
    Bridge {
        id: u64,
        #[serde(flatten)]
        call: DaemonCall,
    },
    /// A page tried to open a new window; the daemon applies `openExternal`'s allowlist.
    OpenExternal { url: String },
    /// `showWindow(profileId?)`. The daemon checks the profile and answers with `Show`.
    OpenMain {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<ProfileId>,
    },
    /// A command from the app menu, which the daemon carries out as from the tray.
    Command { command: Command },
}

/// One message as its line.
///
/// # Errors
/// When `message` does not serialize.
pub fn encode(message: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    Ok(line)
}

/// How much a reader takes for one line: [`MAX_LINE`] bytes and the newline.
#[must_use]
pub fn line_limit() -> u64 {
    u64::try_from(MAX_LINE + 1).unwrap_or(u64::MAX)
}

/// The line in what a reader read up to a newline, taking at most [`line_limit`] bytes: `None`
/// for nothing, at the end of the stream.
///
/// # Errors
/// A line longer than [`MAX_LINE`] or cut off by the end of the stream.
pub fn frame(mut read: Vec<u8>) -> io::Result<Option<Vec<u8>>> {
    if read.is_empty() {
        return Ok(None);
    }
    if read.pop() != Some(b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "helper message too long or unterminated",
        ));
    }
    Ok(Some(read))
}

/// # Errors
/// A line that is not a `T`.
pub fn decode<T: DeserializeOwned>(line: &[u8]) -> io::Result<T> {
    Ok(serde_json::from_slice(line)?)
}

/// Reads one message line. `None` at the end of the stream.
///
/// # Errors
/// On I/O errors, a line longer than [`MAX_LINE`], or a line that is not a `T`.
pub fn read<T: DeserializeOwned>(reader: &mut impl BufRead) -> io::Result<Option<T>> {
    read_line(reader)?.map(|line| decode(&line)).transpose()
}

/// Reads one raw line without its newline. `None` at the end of the stream.
///
/// # Errors
/// On I/O errors, or a line longer than [`MAX_LINE`] or cut off by the end of the stream.
pub fn read_line(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    reader.take(line_limit()).read_until(b'\n', &mut line)?;
    frame(line)
}

/// Writes one message line and flushes it.
///
/// # Errors
/// On I/O errors.
pub fn write<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    writer.write_all(&encode(message)?)?;
    writer.flush()
}

/// The daemon's half of the handshake: `Start`, then the session token alone on the next line.
///
/// # Errors
/// When `start` does not serialize.
pub fn handshake(start: Start, token: &SessionToken) -> io::Result<Vec<u8>> {
    let mut lines = encode(&ToHelper::Start(start))?;
    lines.extend_from_slice(token.expose_secret().as_bytes());
    lines.push(b'\n');
    Ok(lines)
}

/// What the helper opens with.
#[derive(Debug)]
pub enum Opening {
    Window { start: Start, token: SessionToken },
    Alert { failure: Failure, reason: Reason },
}

/// The helper's half of the handshake: `Start` and the token line after it, or `Alert` alone.
///
/// # Errors
/// On I/O errors, a first message other than `Start` or `Alert`, or a missing or empty token.
pub fn read_handshake(input: &mut impl BufRead) -> io::Result<Opening> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message.to_owned());
    match read(input)? {
        Some(ToHelper::Alert { failure, reason }) => Ok(Opening::Alert { failure, reason }),
        Some(ToHelper::Start(start)) => {
            let token = read_line(input)?
                .and_then(|line| String::from_utf8(line).ok())
                .filter(|token| !token.is_empty())
                .ok_or_else(|| invalid("the session token must follow Start on its own line"))?;
            Ok(Opening::Window {
                start,
                token: SessionToken::new(token),
            })
        }
        _ => Err(invalid("the first message must be Start or Alert")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wire<T: Serialize>(message: &T) -> Value {
        serde_json::to_value(message).unwrap()
    }

    #[test]
    fn messages_have_the_documented_wire_shape() {
        let profile = ProfileId::parse("0b6f4f5e-6a7b-4c8d-9e0f-1a2b3c4d5e6f").unwrap();
        let anchor = Anchor {
            x: 1800,
            y: 0,
            width: 24,
            height: 24,
        };
        let cases = [
            (
                wire(&ToHelper::Start(Start {
                    origin: Origin::parse("http://127.0.0.1:4100").unwrap(),
                    view: View::Main {
                        profile: Some(profile.clone()),
                    },
                    bounds: None,
                    webview: PathBuf::from("/data/webview"),
                    updates: true,
                })),
                json!({"type": "Start", "origin": "http://127.0.0.1:4100",
                       "view": {"window": "main", "profile": profile.as_str()},
                       "webview": "/data/webview", "updates": true}),
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
                        choice: Choice::Accept,
                    },
                    bounds: None,
                    last: true,
                }),
                json!({"type": "Closed", "view": {"window": "dialog", "kind": "updateReady",
                       "choice": "accept"}, "last": true}),
            ),
            (
                wire(&FromHelper::Command {
                    command: Command::Quit,
                }),
                json!({"type": "Command", "command": "quit"}),
            ),
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
                last: false,
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
            r#"{"type":"Start","origin":"https://example.com","view":{"window":"main"},"webview":"/d/webview","updates":false}"#,
            r#"{"type":"Eval","script":"alert(1)"}"#,
            r#"{"type":"Alert","failure":"custom","reason":"hi"}"#,
        ] {
            let input = format!("{line}\n");
            assert!(read::<ToHelper>(&mut input.as_bytes()).is_err(), "{line}");
        }
        // The daemon acts on `last`, so a helper must say it.
        let unsaid = "{\"type\":\"Closed\",\"view\":{\"window\":\"quota\"}}\n";
        assert!(read::<FromHelper>(&mut unsaid.as_bytes()).is_err());
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

    fn start() -> Start {
        Start {
            origin: Origin::parse("http://127.0.0.1:4100").unwrap(),
            view: View::Main { profile: None },
            bounds: None,
            webview: PathBuf::from("/data/webview"),
            updates: false,
        }
    }

    #[test]
    fn the_handshake_puts_the_token_alone_on_the_line_after_start() {
        let token = SessionToken::new("session-token".to_owned());
        let lines = handshake(start(), &token).unwrap();
        let text = String::from_utf8(lines.clone()).unwrap();
        let (first, rest) = text.split_once('\n').unwrap();
        assert!(!first.contains("session-token"), "{first}");
        assert_eq!(rest, "session-token\n");
        let Opening::Window { start: read, token } = read_handshake(&mut lines.as_slice()).unwrap()
        else {
            panic!("expected a window");
        };
        assert_eq!(read, start());
        assert_eq!(token.expose_secret(), "session-token");
    }

    #[test]
    fn the_helper_opens_only_with_start_and_its_token_or_an_alert() {
        let alert = encode(&ToHelper::Alert {
            failure: Failure::Start,
            reason: Reason::new("port in use"),
        })
        .unwrap();
        assert!(matches!(
            read_handshake(&mut alert.as_slice()).unwrap(),
            Opening::Alert {
                failure: Failure::Start,
                ..
            }
        ));
        let without_token = encode(&ToHelper::Start(start())).unwrap();
        let empty_token = [without_token.as_slice(), b"\n"].concat();
        let show = encode(&ToHelper::Show {
            view: View::Main { profile: None },
        })
        .unwrap();
        for input in [without_token, empty_token, show, Vec::new()] {
            assert!(read_handshake(&mut input.as_slice()).is_err());
        }
    }
}
