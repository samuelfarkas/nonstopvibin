//! `window.nonstopvibin`: the user script that builds it, and the parser every IPC message
//! from a page passes before the helper acts on it.

use nsv_domain::endpoint::Origin;
use nsv_domain::helper::{DaemonCall, Outcome};
use nsv_domain::id::ProfileId;
use nsv_domain::js::utf16_len;
use nsv_domain::secret::same_secret;
use serde_json::Value;

/// `main.ts`'s zod limits, in UTF-16 code units as JavaScript counts them.
const OPEN_EXTERNAL_LIMIT: usize = 20_000;
const COPY_LIMIT: usize = 1_000_000;
/// The largest message a valid call makes: `copy` of 1,000,000 units that each escape to
/// six bytes of JSON (`\u001f`), plus the envelope.
const MESSAGE_LIMIT: usize = 6 * COPY_LIMIT + 1024;

/// The page-side half. It runs at document start in the main frame only, before any page
/// script, and closes over the IPC handler, the nonce and the pending calls; the page reaches
/// none of them. Replies arrive through `__nonstopvibinReply`, which only settles a call the
/// page itself made.
const SCRIPT: &str = r#"(() => {
  "use strict";
  if (location.origin !== ORIGIN) return;
  const handler = window.webkit.messageHandlers.ipc;
  const send = (message) => handler.postMessage(JSON.stringify(message));
  const nonce = NONCE;
  const pending = new Map();
  let next = 0;
  const call = (method, args) =>
    new Promise((resolve, reject) => {
      const id = ++next;
      const message = { nonce, id, method };
      if (args.length > 0) message.arg = args[0];
      pending.set(id, { resolve, reject });
      try {
        send(message);
      } catch (error) {
        pending.delete(id);
        reject(error);
      }
    });
  Object.defineProperty(window, "__nonstopvibinReply", {
    value: (id, error, result) => {
      const waiter = pending.get(id);
      if (!waiter) return;
      pending.delete(id);
      if (error === null) waiter.resolve(result);
      else waiter.reject(new Error(error));
    },
  });
  const methods = ["bootstrap", "openExternal", "copy", "showWindow", "releaseWindow",
    "setLoginItem", "getLoginItem", "revealData", "chooseProject"];
  Object.defineProperty(window, "nonstopvibin", {
    enumerable: true,
    value: Object.freeze(Object.fromEntries(
      methods.map((method) => [method, (...args) => call(method, args)]))),
  });
  if (ESCAPE_CLOSES)
    window.addEventListener("keydown", (event) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      event.stopImmediatePropagation();
      send({ nonce, id: 0, method: "escape" });
    }, true);
  // The macOS main window has no title bar: a press in a drag region (styles.css,
  // `--app-region: drag`, inherited) moves the window, as Electron's `-webkit-app-region` did.
  if (DRAGS)
    window.addEventListener("mousedown", (event) => {
      if (event.button !== 0) return;
      if (getComputedStyle(event.target).getPropertyValue("--app-region").trim() !== "drag") return;
      send({ nonce, id: 0, method: "dragWindow",
        arg: [Math.round(event.clientX), Math.round(event.clientY)] });
    }, true);
  // Electron showed no context menu; WebKit's offers Reload, Back and Inspect.
  window.addEventListener("contextmenu", (event) => event.preventDefault(), true);
})();"#;

/// The draft guard: a cancelable `beforeunload` that `useKeepDraft` (client/controls.tsx)
/// vetoes while a form holds a draft. Evaluates to whether it was vetoed.
pub(crate) const BEFORE_UNLOAD: &str = r#"(() => {
  const event = new Event("beforeunload", { cancelable: true });
  window.dispatchEvent(event);
  return event.defaultPrevented;
})()"#;

/// The bridge script for one window. Escape closes the quota popup, as Electron's
/// `before-input-event` handler did; a press in a drag region moves a window that `drags`.
pub(crate) fn script(origin: &Origin, nonce: &str, escape_closes: bool, drags: bool) -> String {
    let flag = |on: bool| if on { "true" } else { "false" };
    SCRIPT
        .replacen("ORIGIN", &Value::from(origin.as_str()).to_string(), 1)
        .replacen("NONCE", &Value::from(nonce).to_string(), 1)
        .replacen("ESCAPE_CLOSES", flag(escape_closes), 1)
        .replacen("DRAGS", flag(drags), 1)
}

/// The script that settles page call `id`.
pub(crate) fn reply(id: u64, outcome: &Outcome) -> String {
    match outcome {
        Outcome::Result(value) => format!("window.__nonstopvibinReply({id},null,{value})"),
        Outcome::Error(message) => {
            format!(
                "window.__nonstopvibinReply({id},{},undefined)",
                Value::from(message.as_str())
            )
        }
    }
}

/// A call the bridge script made, with a valid argument.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Call {
    Bootstrap,
    Copy(String),
    ShowWindow(Option<ProfileId>),
    ReleaseWindow,
    ChooseProject,
    Daemon(DaemonCall),
    /// Escape in the quota popup; not a bridge method.
    Escape,
    /// A press at page point (`x`, `y`) in a drag region of the macOS main window; not a
    /// bridge method.
    DragWindow {
        x: i32,
        y: i32,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rejected {
    /// Not from the bridge script on the daemon origin. Nothing answers it.
    Untrusted(&'static str),
    /// From the bridge script, with an unknown method or a bad argument: the call rejects.
    Invalid { id: u64, message: String },
}

const METHODS: [&str; 9] = [
    "bootstrap",
    "openExternal",
    "copy",
    "showWindow",
    "releaseWindow",
    "setLoginItem",
    "getLoginItem",
    "revealData",
    "chooseProject",
];

/// Parses an IPC message that a page at `page_url` posted.
pub(crate) fn parse(
    origin: &Origin,
    nonce: &str,
    page_url: &str,
    body: &str,
) -> Result<(u64, Call), Rejected> {
    if !origin.contains(page_url) {
        return Err(Rejected::Untrusted("the page is not on the daemon origin"));
    }
    if body.len() > MESSAGE_LIMIT {
        return Err(Rejected::Untrusted("the message is too long"));
    }
    let Ok(Value::Object(mut message)) = serde_json::from_str(body) else {
        return Err(Rejected::Untrusted("the message is not a JSON object"));
    };
    let signed = message
        .get("nonce")
        .and_then(Value::as_str)
        .is_some_and(|presented| same_secret(presented, nonce));
    if !signed {
        return Err(Rejected::Untrusted(
            "the message did not come from the bridge script",
        ));
    }
    let Some(id) = message.get("id").and_then(Value::as_u64) else {
        return Err(Rejected::Untrusted("the message has no id"));
    };
    let method = match message.remove("method") {
        Some(Value::String(method)) => method,
        _ => String::new(),
    };
    let call = match (method.as_str(), message.remove("arg")) {
        ("bootstrap", None) => Call::Bootstrap,
        ("copy", Some(Value::String(text))) if utf16_len(&text) <= COPY_LIMIT => Call::Copy(text),
        ("showWindow", None) => Call::ShowWindow(None),
        ("showWindow", Some(Value::String(profile))) => match ProfileId::parse(&profile) {
            Ok(profile) => Call::ShowWindow(Some(profile)),
            Err(_) => return Err(invalid(id, &method)),
        },
        ("releaseWindow", None) => Call::ReleaseWindow,
        ("chooseProject", None) => Call::ChooseProject,
        ("openExternal", Some(Value::String(url))) if utf16_len(&url) <= OPEN_EXTERNAL_LIMIT => {
            Call::Daemon(DaemonCall::OpenExternal(url))
        }
        ("getLoginItem", None) => Call::Daemon(DaemonCall::GetLoginItem),
        ("setLoginItem", Some(Value::Bool(enabled))) => {
            Call::Daemon(DaemonCall::SetLoginItem(enabled))
        }
        ("revealData", None) => Call::Daemon(DaemonCall::RevealData),
        ("escape", None) => Call::Escape,
        ("dragWindow", Some(Value::Array(point))) => match point.as_slice() {
            [x, y] => match (page_coordinate(x), page_coordinate(y)) {
                (Some(x), Some(y)) => Call::DragWindow { x, y },
                _ => return Err(invalid(id, &method)),
            },
            _ => return Err(invalid(id, &method)),
        },
        _ if METHODS.contains(&method.as_str()) => return Err(invalid(id, &method)),
        _ => {
            return Err(Rejected::Invalid {
                id,
                message: "Unknown desktop method.".to_owned(),
            });
        }
    };
    Ok((id, call))
}

/// A whole page coordinate; a window is far smaller than `i32`.
fn page_coordinate(value: &Value) -> Option<i32> {
    value.as_i64().and_then(|value| i32::try_from(value).ok())
}

fn invalid(id: u64, method: &str) -> Rejected {
    Rejected::Invalid {
        id,
        message: format!("Invalid argument for {method}."),
    }
}

/// Whether a new window's URL may go to the daemon, which applies `openExternal`'s limit.
pub(crate) fn fits_external(url: &str) -> bool {
    utf16_len(url) <= OPEN_EXTERNAL_LIMIT
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PAGE: &str = "http://127.0.0.1:4100/?view=tray";
    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    fn origin() -> Origin {
        Origin::parse("http://127.0.0.1:4100").unwrap()
    }

    fn call(method: &str, arg: Option<Value>) -> Result<(u64, Call), Rejected> {
        let mut message = json!({ "nonce": NONCE, "id": 9, "method": method });
        if let Some(arg) = arg {
            message["arg"] = arg;
        }
        parse(&origin(), NONCE, PAGE, &message.to_string())
    }

    fn rejected(method: &str) -> Result<(u64, Call), Rejected> {
        Err(Rejected::Invalid {
            id: 9,
            message: format!("Invalid argument for {method}."),
        })
    }

    #[test]
    fn accepts_each_method_with_the_argument_main_ts_accepted() {
        let profile = "0b6f4f5e-6a7b-4c8d-9e0f-1a2b3c4d5e6f";
        let cases = [
            ("bootstrap", None, Call::Bootstrap),
            ("copy", Some(json!("")), Call::Copy(String::new())),
            ("showWindow", None, Call::ShowWindow(None)),
            (
                "showWindow",
                Some(json!(profile)),
                Call::ShowWindow(Some(ProfileId::parse(profile).unwrap())),
            ),
            ("releaseWindow", None, Call::ReleaseWindow),
            ("chooseProject", None, Call::ChooseProject),
            (
                "openExternal",
                Some(json!("https://claude.ai/")),
                Call::Daemon(DaemonCall::OpenExternal("https://claude.ai/".to_owned())),
            ),
            ("getLoginItem", None, Call::Daemon(DaemonCall::GetLoginItem)),
            (
                "setLoginItem",
                Some(json!(false)),
                Call::Daemon(DaemonCall::SetLoginItem(false)),
            ),
            ("revealData", None, Call::Daemon(DaemonCall::RevealData)),
            ("escape", None, Call::Escape),
            (
                "dragWindow",
                Some(json!([12, -3])),
                Call::DragWindow { x: 12, y: -3 },
            ),
        ];
        for (method, arg, expected) in cases {
            assert_eq!(call(method, arg), Ok((9, expected)), "{method}");
        }
    }

    #[test]
    fn rejects_arguments_main_ts_rejected() {
        let cases = [
            ("bootstrap", json!(null)),
            ("bootstrap", json!("x")),
            ("copy", json!(42)),
            ("showWindow", json!("not-a-uuid")),
            ("showWindow", json!(null)),
            ("releaseWindow", json!(false)),
            ("openExternal", json!({"href": "https://claude.ai/"})),
            ("setLoginItem", json!("true")),
            ("getLoginItem", json!(true)),
            ("revealData", json!([])),
            ("chooseProject", json!("/tmp")),
            ("dragWindow", json!([1])),
            ("dragWindow", json!([1.5, 2])),
            ("dragWindow", json!(["1", 2])),
        ];
        for (method, arg) in cases {
            assert_eq!(call(method, Some(arg)), rejected(method), "{method}");
        }
        assert_eq!(call("setLoginItem", None), rejected("setLoginItem"));
    }

    #[test]
    fn counts_length_limits_in_utf16_units() {
        assert!(call("openExternal", Some(json!("a".repeat(20_000)))).is_ok());
        assert_eq!(
            call("openExternal", Some(json!("a".repeat(20_001)))),
            rejected("openExternal")
        );
        // U+1F600 is two UTF-16 units, as in JavaScript.
        let at_limit = format!("{}😀", "a".repeat(999_998));
        assert!(call("copy", Some(json!(at_limit))).is_ok());
        let over = format!("{}😀", "a".repeat(999_999));
        assert_eq!(call("copy", Some(json!(over))), rejected("copy"));
    }

    #[test]
    fn rejects_unknown_methods_with_an_answer() {
        for method in ["eval", "", "constructor", "__proto__"] {
            assert_eq!(
                call(method, None),
                Err(Rejected::Invalid {
                    id: 9,
                    message: "Unknown desktop method.".to_owned()
                })
            );
        }
    }

    #[test]
    fn drops_messages_not_from_the_bridge_script_on_the_daemon_origin() {
        let good = json!({ "nonce": NONCE, "id": 1, "method": "bootstrap" }).to_string();
        let wrong_origin = parse(&origin(), NONCE, "http://localhost:4100/", &good);
        assert_eq!(
            wrong_origin,
            Err(Rejected::Untrusted("the page is not on the daemon origin"))
        );
        let unsigned = json!({ "id": 1, "method": "bootstrap" }).to_string();
        let forged = json!({ "nonce": "guess", "id": 1, "method": "bootstrap" }).to_string();
        for body in [unsigned, forged] {
            assert_eq!(
                parse(&origin(), NONCE, PAGE, &body),
                Err(Rejected::Untrusted(
                    "the message did not come from the bridge script"
                ))
            );
        }
        for body in ["bootstrap", "[1]", "{", "null"] {
            assert_eq!(
                parse(&origin(), NONCE, PAGE, body),
                Err(Rejected::Untrusted("the message is not a JSON object"))
            );
        }
        let no_id = json!({ "nonce": NONCE, "id": -1, "method": "bootstrap" }).to_string();
        assert_eq!(
            parse(&origin(), NONCE, PAGE, &no_id),
            Err(Rejected::Untrusted("the message has no id"))
        );
        let huge = "x".repeat(MESSAGE_LIMIT + 1);
        assert_eq!(
            parse(&origin(), NONCE, PAGE, &huge),
            Err(Rejected::Untrusted("the message is too long"))
        );
    }

    #[test]
    fn script_embeds_its_parameters_as_literals() {
        let script = script(&origin(), NONCE, true, false);
        assert!(script.contains(r#"if (location.origin !== "http://127.0.0.1:4100") return;"#));
        assert!(script.contains(&format!("const nonce = \"{NONCE}\";")));
        assert!(script.contains("if (true)\n    window.addEventListener(\"keydown\""));
        assert!(script.contains("if (false)\n    window.addEventListener(\"mousedown\""));
        assert!(
            !["ORIGIN", "ESCAPE_CLOSES", "DRAGS"]
                .iter()
                .any(|name| script.contains(name))
        );
        assert_eq!(
            reply(3, &Outcome::Error("No \"quotes\" </script>".to_owned())),
            r#"window.__nonstopvibinReply(3,"No \"quotes\" </script>",undefined)"#
        );
        assert_eq!(
            reply(4, &Outcome::Result(json!({"token": "t"}))),
            r#"window.__nonstopvibinReply(4,null,{"token":"t"})"#
        );
    }
}
