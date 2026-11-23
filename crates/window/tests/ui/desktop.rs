//! What the user does to the helper's main window: keys, a click, the close button, and
//! whether the window is on screen. X11 (`XTest`) on Linux; System Events on macOS, which
//! needs the Accessibility permission the CI runner grants.

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
        pub(crate) fn click(&self, x: f64, y: f64) {
            let position = system_events(&format!(
                "get position of window \"NonstopVibin\" of process \"{PROCESS}\""
            ));
            let (left, top) = position.split_once(", ").unwrap();
            let (left, top): (f64, f64) = (left.parse().unwrap(), top.parse().unwrap());
            system_events(&format!("click at {{{}, {}}}", left + x, top + y));
        }

        pub(crate) fn close_main(&self) {
            system_events(&format!(
                "click (first button of window \"NonstopVibin\" of process \"{PROCESS}\" whose subrole is \"AXCloseButton\")"
            ));
        }
    }
}

pub(crate) use imp::Desktop;
