//! The supervisor over a real pipe, with a shell script playing the window helper: it says
//! what the test wrote for it and records what the shell sends. No X server needed, except for
//! the browser fallback, which needs the fake `xdg-open` that tests/gui.sh puts on PATH.
// Marks the whole binary as test code for the workspace's test-only lint exceptions.
#![cfg(test)]
#![cfg(target_os = "linux")]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use common::{Host, PROFILE, TOKEN, directory, exited, options, spawn_shell, wait_until};
use nsv_shell::HostEvent;
use serde_json::{Value, json};

/// Helper number n says `say.n`, records its stdin in `heard.n` until the pipe closes, and
/// exits; `crash.n` makes it exit 1 right after speaking.
const FAKE_HELPER: &str = r#"#!/bin/sh
dir=$(dirname "$0")
n=$(( $(cat "$dir/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$dir/count"
echo $$ > "$dir/pid.$n"
[ -f "$dir/say.$n" ] && cat "$dir/say.$n"
[ -f "$dir/crash.$n" ] && exit 1
cat > "$dir/heard.$n"
"#;

struct Fake {
    dir: PathBuf,
}

impl Fake {
    fn new(name: &str) -> Self {
        let dir = directory(name);
        let helper = dir.join("helper.sh");
        fs::write(&helper, FAKE_HELPER).unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn helper(&self) -> PathBuf {
        self.dir.join("helper.sh")
    }

    fn say(&self, n: u32, messages: &[Value]) {
        let lines: String = messages
            .iter()
            .map(|message| {
                message.to_string()
                    + "
"
            })
            .collect();
        fs::write(self.dir.join(format!("say.{n}")), lines).unwrap();
    }

    fn crash(&self, n: u32) {
        fs::write(self.dir.join(format!("crash.{n}")), "").unwrap();
    }

    fn count(&self) -> u32 {
        fs::read_to_string(self.dir.join("count")).map_or(0, |count| count.trim().parse().unwrap())
    }

    fn pid(&self, n: u32) -> u32 {
        let file = self.dir.join(format!("pid.{n}"));
        wait_until(&format!("helper {n} starts"), || file.exists());
        fs::read_to_string(file).unwrap().trim().parse().unwrap()
    }

    /// What helper n heard, once its pipe closed: the token line as a string, the rest as JSON.
    fn heard(&self, n: u32) -> Vec<Value> {
        let pid = self.pid(n);
        wait_until(&format!("helper {n} exits"), || exited(pid));
        fs::read_to_string(self.dir.join(format!("heard.{n}")))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap_or_else(|_| json!(line)))
            .collect()
    }

    fn config(&self) -> PathBuf {
        self.dir.join("config")
    }
}

fn start(data: &Path, bounds: Option<Value>) -> Value {
    let mut start = json!({"type": "Start", "origin": "http://127.0.0.1:4100",
        "view": {"window": "main"}, "dataDirectory": data});
    if let Some(bounds) = bounds {
        start["bounds"] = bounds;
    }
    start
}

#[test]
fn forwards_bridge_calls_remembers_bounds_and_quits_from_the_helper() {
    let fake = Fake::new("bridge");
    let bounds = json!({"x": 40, "y": 30, "width": 1000, "height": 700});
    fake.say(
        1,
        &[
            json!({"type": "Ready"}),
            json!({"type": "OpenMain", "profile": "11111111-2222-4333-8444-555555555555"}),
            json!({"type": "Bridge", "id": 1, "method": "getLoginItem"}),
            json!({"type": "Bridge", "id": 2, "method": "setLoginItem", "arg": true}),
            json!({"type": "Bridge", "id": 3, "method": "openExternal", "arg": "http://claude.ai/"}),
            json!({"type": "OpenExternal", "url": "https://evil.example/"}),
            json!({"type": "Closed", "view": {"window": "main"}, "bounds": bounds}),
        ],
    );
    fake.say(2, &[json!({"type": "Ready"}), json!({"type": "QuitApp"})]);
    let host = Host::new(4100, &fake.dir);
    let shell = spawn_shell(&host, options(fake.helper(), false, false, &fake.config()));
    // No main window exists, so a sign-in shows none; the second launch's request does.
    host.send(HostEvent::SignInAccepted);
    host.send(HostEvent::ShowMain);
    assert_eq!(
        fake.heard(1),
        [
            start(&fake.dir, None),
            json!(TOKEN),
            json!({"type": "Reply", "id": 1, "result": false}),
            json!({"type": "Reply", "id": 2,
                   "error": "Use the packaged app to enable automatic startup."}),
            json!({"type": "Reply", "id": 3,
                   "error": "This external URL is not an approved provider sign-in page."}),
        ]
    );
    // The helper exits with its last window; the next view starts a new one where it was.
    host.send(HostEvent::ShowMain);
    assert_eq!(
        fake.heard(2),
        [
            start(&fake.dir, Some(bounds)),
            json!(TOKEN),
            json!({"type": "Quit"})
        ]
    );
    shell.join().unwrap();
    assert!(host.shut_down.load(Ordering::SeqCst));
    assert_eq!(fake.count(), 2);
    assert_eq!(
        host.reports(),
        [
            "Profile not found.",
            "This external URL is not an approved provider sign-in page."
        ]
    );
    fs::remove_dir_all(&fake.dir).unwrap();
}

#[test]
fn a_known_profile_opens_and_a_crashed_helper_is_replaced() {
    let fake = Fake::new("crash");
    fake.say(1, &[json!({"type": "Ready"})]);
    fake.crash(1);
    fake.say(
        2,
        &[
            json!({"type": "Ready"}),
            json!({"type": "OpenMain", "profile": PROFILE}),
            json!({"type": "QuitApp"}),
        ],
    );
    let host = Host::new(4100, &fake.dir);
    let shell = spawn_shell(&host, options(fake.helper(), true, false, &fake.config()));
    let first = fake.pid(1);
    wait_until("helper 1 crashes", || exited(first));
    host.send(HostEvent::ShowMain);
    assert_eq!(
        fake.heard(2),
        [
            start(&fake.dir, None),
            json!(TOKEN),
            json!({"type": "Show", "view": {"window": "main", "profile": PROFILE}}),
            json!({"type": "Quit"}),
        ]
    );
    shell.join().unwrap();
    assert_eq!(fake.count(), 2);
    assert_eq!(host.reports(), Vec::<String>::new());
    fs::remove_dir_all(&fake.dir).unwrap();
}

/// Needs tests/gui.sh's fake `xdg-open`, which appends its arguments to
/// `$NSV_OPENED`.
#[test]
#[ignore = "needs the fake xdg-open from tests/gui.sh"]
fn a_helper_that_cannot_start_falls_back_to_the_browser() {
    let opened = PathBuf::from(std::env::var_os("NSV_OPENED").unwrap());
    drop(fs::remove_file(&opened));
    let fake = Fake::new("fallback");
    // Exits before Ready, as without a display or WebKitGTK.
    fake.crash(1);
    let link = format!("http://127.0.0.1:4100/#session={TOKEN}\n");
    for helper in [fake.helper(), fake.dir.join("no-such-helper")] {
        let host = Host::new(4100, &fake.dir);
        let shell = spawn_shell(&host, options(helper, true, false, &fake.config()));
        wait_until("the browser opens the session link", || {
            fs::read_to_string(&opened).is_ok_and(|text| text == link)
        });
        fs::remove_file(&opened).unwrap();
        host.close();
        shell.join().unwrap();
    }
    assert_eq!(fake.count(), 1);
    fs::remove_dir_all(&fake.dir).unwrap();
}
