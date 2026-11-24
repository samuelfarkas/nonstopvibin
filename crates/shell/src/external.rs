//! What leaves the app: provider pages in the system browser, and the data folder in the file
//! manager.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;

use nsv_desktop::bridge::{Method, fits_external};
use nsv_domain::registry::registry;
use tokio::process::Command;
use url::Url;

use crate::platform::{Opener, Os};

const NOT_APPROVED: &str = "This external URL is not an approved provider sign-in page.";

/// main.ts's `safeExternal`: `https:` with no user name or password on a host some provider
/// lists in `hosts`, within `openExternal`'s length limit. Returns the normalized URL to open.
pub(crate) fn safe_external(raw: &str) -> Result<String, String> {
    if !fits_external(raw) {
        return Err(Method::OpenExternal.invalid_argument());
    }
    // Node's message for a string `new URL` cannot parse.
    let url = Url::parse(raw).map_err(|_| "Invalid URL".to_owned())?;
    let providers = registry();
    let approved = url.host_str().is_some_and(|host| {
        providers
            .providers
            .iter()
            .any(|(_, entry)| entry.hosts.iter().any(|allowed| allowed == host))
    });
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || !approved
    {
        return Err(NOT_APPROVED.to_owned());
    }
    Ok(url.into())
}

/// Opens `url` when [`safe_external`] approves it: `openExternal`, and a page's new window.
pub(crate) fn open_approved(url: &str) -> Result<(), String> {
    open(&safe_external(url)?)
}

/// Opens `target`: a URL [`safe_external`] approved, or the page holding the session link.
pub(crate) fn open(target: impl AsRef<OsStr>) -> Result<(), String> {
    Os::open(target.as_ref())
}

#[cfg(target_os = "macos")]
impl Opener for Os {
    /// Through Launch Services, as Electron's `shell.openExternal` did, so no process's
    /// arguments carry the URL: other users can read those, and a sign-in URL holds its OAuth
    /// `state`.
    fn open(target: &OsStr) -> Result<(), String> {
        use objc2_app_kit::NSWorkspace;
        use objc2_foundation::{NSString, NSURL};

        let text = target.to_str().ok_or_else(|| "Invalid URL".to_owned())?;
        let string = NSString::from_str(text);
        let url = if text.starts_with('/') {
            NSURL::fileURLWithPath(&string)
        } else {
            NSURL::URLWithString(&string).ok_or_else(|| "Invalid URL".to_owned())?
        };
        if NSWorkspace::sharedWorkspace().openURL(&url) {
            Ok(())
        } else {
            Err("macOS could not open it.".to_owned())
        }
    }

    fn reveal(file: &Path) -> impl Future<Output = Result<(), String>> {
        std::future::ready(launch(Command::new("open").arg("-R").arg(file)))
    }
}

#[cfg(target_os = "linux")]
impl Opener for Os {
    fn open(target: &OsStr) -> Result<(), String> {
        launch(Command::new("xdg-open").arg(target))
    }

    /// The `FileManager1` D-Bus interface, else the folder in the default file manager.
    async fn reveal(file: &Path) -> Result<(), String> {
        let Ok(uri) = Url::from_file_path(file) else {
            return Err("The data folder's path is not absolute.".to_owned());
        };
        match show_items(uri.as_str()).await {
            Ok(()) => Ok(()),
            Err(error) => {
                tracing::warn!("no FileManager1 ({error}); opening the folder");
                Self::open(file.parent().unwrap_or(file).as_os_str())
            }
        }
    }
}

#[cfg(target_os = "linux")]
async fn show_items(uri: &str) -> zbus::Result<()> {
    let bus = zbus::Connection::session().await?;
    bus.call_method(
        Some("org.freedesktop.FileManager1"),
        "/org/freedesktop/FileManager1",
        Some("org.freedesktop.FileManager1"),
        "ShowItems",
        &(vec![uri], ""),
    )
    .await?;
    Ok(())
}

/// Starts a program that hands its argument to the desktop. Tokio reaps it once it exits.
fn launch(command: &mut Command) -> Result<(), String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|error| format!("Could not open it: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Every `external url` case main.ts answered in `tests/golden/cli.json`.
    #[test]
    fn approves_exactly_what_main_ts_approved() {
        let golden: Vec<Value> =
            serde_json::from_str(include_str!("../../../tests/golden/cli.json")).unwrap();
        let mut checked = 0;
        for case in &golden {
            if !case["name"].as_str().unwrap().starts_with("external url ") {
                continue;
            }
            let input = case["input"].as_str().unwrap();
            let actual = safe_external(input);
            if let Some(error) = case.get("error") {
                assert_eq!(actual, Err(error["message"].as_str().unwrap().to_owned()));
            } else if case["output"]["allowed"] == Value::Bool(true) {
                assert_eq!(
                    actual.as_deref(),
                    Ok(case["output"]["href"].as_str().unwrap())
                );
            } else {
                assert_eq!(actual, Err(NOT_APPROVED.to_owned()), "{input}");
            }
            checked += 1;
        }
        assert_eq!(checked, 12);
    }

    #[test]
    fn every_provider_host_is_approved() {
        let golden: Vec<Value> =
            serde_json::from_str(include_str!("../../../tests/golden/cli.json")).unwrap();
        let hosts = golden
            .iter()
            .find(|case| case["name"] == "external hosts")
            .unwrap()["output"]
            .as_array()
            .unwrap();
        assert_eq!(hosts.len(), 14);
        for host in hosts {
            let url = format!("https://{}/", host.as_str().unwrap());
            assert_eq!(safe_external(&url), Ok(url.clone()));
        }
        assert_eq!(
            safe_external("https://claude.ai:pass@claude.ai/"),
            Err(NOT_APPROVED.to_owned())
        );
    }

    /// The daemon applies `openExternal`'s 20,000-unit limit again, to bridge calls and to new
    /// windows alike.
    #[test]
    fn an_approved_url_over_the_bridge_limit_is_refused() {
        let at_limit = format!("https://claude.ai/{}", "a".repeat(20_000 - 18));
        assert_eq!(safe_external(&at_limit), Ok(at_limit.clone()));
        assert_eq!(
            safe_external(&format!("{at_limit}a")),
            Err("Invalid argument for openExternal.".to_owned())
        );
    }
}
