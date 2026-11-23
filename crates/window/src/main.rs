//! `nonstopvibin-window`: the only process that links WebKit. The daemon starts it whenever a
//! window or dialog must show and talks to it over stdin and stdout (`nsv_domain::helper`); it
//! exits when its last window closes, so no WebKit process outlives the window. A daemon that
//! failed to start or stop starts it for an error dialog alone (`Alert`).
#![forbid(unsafe_code)]

mod app;
mod bridge;
mod layout;
mod native;

use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nsv_domain::helper::{self, Failure, Reason, ToHelper};
use nsv_domain::secret::SessionToken;

fn main() -> ExitCode {
    // The lock ends here; whatever it buffered stays in stdin for the pipe thread.
    let launch = launch(&mut io::stdin().lock());
    match launch {
        Ok(Launch::Window(boot)) => app::run(boot),
        Ok(Launch::Alert(failure, reason)) => app::alert(failure, reason),
        Err(error) => {
            app::log(format_args!("could not start: {error}"));
            ExitCode::FAILURE
        }
    }
}

enum Launch {
    Window(app::Boot),
    Alert(Failure, Reason),
}

fn launch(input: &mut impl BufRead) -> io::Result<Launch> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message.to_owned());
    let (origin, view, bounds, data_directory, updates) = match helper::read(input)? {
        Some(ToHelper::Alert { failure, reason }) => return Ok(Launch::Alert(failure, reason)),
        Some(ToHelper::Start {
            origin,
            view,
            bounds,
            data_directory,
            updates,
        }) => (origin, view, bounds, data_directory, updates),
        _ => return Err(invalid("the first message must be Start or Alert")),
    };
    let token = helper::read_line(input)?
        .and_then(|line| String::from_utf8(line).ok())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| invalid("the session token must follow Start on its own line"))?;
    // Linux has no app menu.
    #[cfg(not(target_os = "macos"))]
    let _: bool = updates;
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    Ok(Launch::Window(app::Boot {
        origin,
        token: SessionToken::new(token),
        view,
        bounds,
        web_data: web_data(&data_directory)?,
        nonce: format!("{:032x}", u128::from_le_bytes(nonce)),
        #[cfg(target_os = "macos")]
        updates,
    }))
}

/// Linux keeps the webview's storage in `<data dir>/webview`, for the owner only.
#[cfg(target_os = "linux")]
fn web_data(data_directory: &Path) -> io::Result<Option<PathBuf>> {
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let path = data_directory.join("webview");
    match fs::DirBuilder::new().mode(0o700).create(&path) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(Some(path))
}

/// macOS keeps WebKit's default store.
#[cfg(not(target_os = "linux"))]
#[expect(clippy::unnecessary_wraps, reason = "the Linux twin can fail")]
fn web_data(_data_directory: &Path) -> io::Result<Option<PathBuf>> {
    Ok(None)
}
