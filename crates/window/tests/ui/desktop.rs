//! What the user does to the helper's main window: keys, a click, the close button, and
//! whether the window is on screen; on macOS also typing, ⌘ shortcuts, the pasteboard and the
//! menu bar. X11 (`XTest`) on Linux; System Events on macOS, which needs the Accessibility
//! permission the CI runner grants.

#[cfg(target_os = "linux")]
mod imp {
    use x11_dl::keysym::{XK_Escape, XK_Return, XK_Tab, XK_space};

    use crate::common::x11::{Window, X};

    const MAIN: &str = "NonstopVibin";

    pub(crate) struct Desktop(X);

    impl Desktop {
        pub(crate) fn open() -> Self {
            Self(X::open())
        }

        fn main(&self) -> Option<Window> {
            self.0.find(MAIN)
        }

        pub(crate) fn main_visible(&self) -> bool {
            self.main().is_some_and(|window| self.0.mapped(window))
        }

        pub(crate) fn key(&self, name: &str) {
            let keysym = match name {
                "Tab" => XK_Tab,
                "Enter" => XK_Return,
                " " => XK_space,
                "Escape" => XK_Escape,
                other => panic!("no key {other:?}"),
            };
            self.0.key(keysym);
        }

        /// A click at `(x, y)` in the main window's page.
        pub(crate) fn click(&self, x: f64, y: f64) {
            let window = self.main().unwrap();
            let (left, top, _, _) = self.0.geometry(window);
            #[expect(
                clippy::cast_possible_truncation,
                reason = "page coordinates are small"
            )]
            self.0
                .click(left + x.round() as i32, top + y.round() as i32);
        }

        pub(crate) fn close_main(&self) {
            self.0.close(self.main().unwrap());
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::process::Command;

    use serde_json::{Value, json};

    const PROCESS: &str = "nonstopvibin-window";

    pub(crate) struct Desktop;

    fn system_events(script: &str) -> String {
        let output = Command::new("osascript")
            .args([
                "-e",
                &format!("tell application \"System Events\" to {script}"),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "osascript: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[expect(
        clippy::unused_self,
        reason = "the Linux twin acts through its X connection; System Events needs none"
    )]
    impl Desktop {
        pub(crate) fn open() -> Self {
            Self
        }

        pub(crate) fn main_visible(&self) -> bool {
            system_events(&format!(
                "count (windows of process \"{PROCESS}\" whose name is \"NonstopVibin\")"
            )) != "0"
        }

        pub(crate) fn key(&self, name: &str) {
            let code = match name {
                "Tab" => 48,
                "Enter" => 36,
                " " => 49,
                "Escape" => 53,
                other => panic!("no key {other:?}"),
            };
            system_events(&format!("key code {code}"));
        }

        /// A click at `(x, y)` in the main window's page; its content view fills the window.
        /// The traffic lights sit on top of the page's top-left corner, so a click on one of
        /// them fails instead of minimizing or zooming the window.
        pub(crate) fn click(&self, x: f64, y: f64) {
            // System Events' `click at` presses the accessibility element there, which a
            // WebKit page never sees as a click: post the mouse events themselves.
            let script = format!(
                "ObjC.import('CoreGraphics'); \
                 const window = Application('System Events').processes.byName('{PROCESS}').windows.byName('NonstopVibin'); \
                 const [left, top] = window.position(); const [x, y] = [left + {x}, top + {y}]; \
                 for (const button of window.buttons()) {{ \
                   const [bx, by] = button.position(); const [width, height] = button.size(); \
                   if (x >= bx && x < bx + width && y >= by && y < by + height) \
                     throw new Error('({x}, {y}) is on the window\\'s ' + button.subrole()); \
                 }} \
                 const point = $.CGPointMake(x, y); \
                 for (const type of [$.kCGEventMouseMoved, $.kCGEventLeftMouseDown, $.kCGEventLeftMouseUp]) \
                 $.CGEventPost($.kCGHIDEventTap, $.CGEventCreateMouseEvent($(), type, point, $.kCGMouseButtonLeft));"
            );
            let output = Command::new("osascript")
                .args(["-l", "JavaScript", "-e", &script])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "osascript: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        pub(crate) fn close_main(&self) {
            system_events(&format!(
                "click (first button of window \"NonstopVibin\" of process \"{PROCESS}\" whose subrole is \"AXCloseButton\")"
            ));
        }

        /// What only macOS has: typing and ⌘ shortcuts, which the application menu handles,
        /// the pasteboard, and the menu bar's titles. `None` for any other action.
        pub(crate) fn mac_action(&self, action: &Value) -> Option<Value> {
            if let Some(text) = action["type"].as_str() {
                system_events(&format!("keystroke {}", Value::from(text)));
            } else if let Some(key) = action["shortcut"].as_str() {
                let modifiers = if action["shift"] == true {
                    "{command down, shift down}"
                } else {
                    "{command down}"
                };
                system_events(&format!("keystroke {} using {modifiers}", Value::from(key)));
            } else if action["clipboard"] == true {
                let output = Command::new("pbpaste").output().unwrap();
                return Some(json!({ "clipboard": String::from_utf8_lossy(&output.stdout) }));
            } else if action["menus"] == true {
                let bar = format!("menu bar 1 of process \"{PROCESS}\"");
                let names = |of: &str| -> Vec<String> {
                    system_events(&format!("get name of {of}"))
                        .split(", ")
                        .map(str::to_owned)
                        .collect()
                };
                let items = |menu: usize| {
                    names(&format!(
                        "menu items of menu 1 of menu bar item {menu} of {bar}"
                    ))
                };
                return Some(json!({
                    "bar": names(&format!("menu bar items of {bar}")),
                    "app": items(2),
                    "edit": items(3),
                    "view": items(4),
                }));
            } else {
                return None;
            }
            Some(json!({}))
        }
    }
}

pub(crate) use imp::Desktop;
