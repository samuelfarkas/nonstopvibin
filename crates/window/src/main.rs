//! `nonstopvibin-window`: the only process that links WebKit. The daemon starts it whenever a
//! window or dialog must show and talks to it over stdin and stdout (`nsv_desktop::pipe`); it
//! exits when its last window closes, so no WebKit process outlives the window. A daemon that
//! failed to start or stop starts it for an error dialog alone (`Alert`).
#![forbid(unsafe_code)]

mod app;
mod bridge;
mod layout;
mod native;

use std::io::{self, BufRead};
use std::process::ExitCode;

use nsv_desktop::pipe::{self, Failure, Opening, Reason, Start};
use nsv_domain::random;

use crate::native::{Native, Os};

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
    let (start, token) = match pipe::read_handshake(input)? {
        Opening::Alert { failure, reason } => return Ok(Launch::Alert(failure, reason)),
        Opening::Window { start, token } => (start, token),
    };
    let Start {
        origin,
        view,
        bounds,
        webview,
        updates,
    } = start;
    let nonce = random::bytes::<16>()?;
    Ok(Launch::Window(app::Boot {
        origin,
        token,
        view,
        bounds,
        web_data: Os::web_data(webview)?,
        nonce: format!("{:032x}", u128::from_le_bytes(nonce)),
        updates,
    }))
}
