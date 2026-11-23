//! The private credential protocol between `AgentSetup`'s Unix socket and the code it generates:
//! `GET /profiles/<profile id>` with `Host: localhost`, answered with the profile's client key
//! and the gateway port on two lines. Never a shell program or a management token. The server
//! (`socket.rs`) and both clients below are built from these names. Installed agents execute
//! the generated text, so it stays byte-for-byte what the TypeScript server wrote.

use std::path::Path;

use nsv_domain::secret::{CLIENT_KEY_LENGTH, CLIENT_KEY_PREFIX};
use nsv_domain::wire::Profile;

use crate::connection::{shell_quote, text};
use crate::json;

pub(crate) const HOST: &str = "localhost";
pub(crate) const PROFILES_PATH: &str = "/profiles/";
pub(crate) const SEPARATOR: &str = "\n";
/// The client-key alphabet in the order installed credential helpers already contain.
const KEY_CHARACTERS: &str = "a-zA-Z0-9_-";

/// The key helper the harness's native credential hook runs, never an agent launcher. The
/// installed URL stays fixed; a port change must fail before returning a key.
pub(crate) fn credential_helper(profile: &Profile, socket: &Path, port: u16) -> String {
    let socket = shell_quote(&text(socket));
    let url = shell_quote(&format!("http://{HOST}{PROFILES_PATH}{}", profile.id));
    let start = shell_quote(&format!("Open nonstopvibin and start {}.", profile.slug));
    let separator = shell_quote(SEPARATOR);
    let port = shell_quote(&port.to_string());
    let length = CLIENT_KEY_PREFIX.len() + CLIENT_KEY_LENGTH;
    format!(
        r#"#!/bin/sh
set -eu
nv_connection=$(curl -q --fail --silent --show-error --noproxy '*' --proxy '' --max-time 4 --unix-socket {socket} {url}) || {{
  printf '%s\n' {start} >&2
  exit 1
}}
nv_key=${{nv_connection%%{separator}*}}
nv_port=${{nv_connection#*{separator}}}
case "$nv_key" in {CLIENT_KEY_PREFIX}*) ;; *) exit 1 ;; esac
case "$nv_key" in *[!{KEY_CHARACTERS}]*) exit 1 ;; esac
[ "${{#nv_key}}" -eq {length} ] || exit 1
[ "$nv_port" = {port} ] || {{
  printf '%s\n' 'The gateway port changed. Reconnect this agent in nonstopvibin.' >&2
  exit 1
}}
printf '%s\n' "$nv_key"
"#
    )
}

/// The client key as a JavaScript regular expression source, without anchors.
pub(crate) fn client_key_source() -> String {
    format!("{CLIENT_KEY_PREFIX}[{KEY_CHARACTERS}]{{{CLIENT_KEY_LENGTH}}}")
}

/// JavaScript source of `function credential()`, which resolves to the profile's key or rejects
/// with `failure` (a JavaScript expression) when the reply is refused, malformed or for another
/// port.
pub(crate) fn credential_client(
    profile: &Profile,
    socket: &Path,
    port: u16,
    failure: &str,
) -> String {
    // The regular expression literal JavaScript prints for `^<key>\n([0-9]{1,5})$`.
    let reply = format!("/^{}\\n([0-9]{{1,5}})$/", client_key_source());
    let socket = json::string(&text(socket));
    let path = json::string(&format!("{PROFILES_PATH}{}", profile.id));
    let host = json::string(HOST);
    let port = json::string(&port.to_string());
    let separator = json::string(SEPARATOR);
    format!(
        r#"function credential() {{
  return new Promise((resolve, reject) => {{
    const failure = () => {failure};
    const request = http.get({{
      socketPath: {socket},
      path: {path},
      headers: {{ Host: {host} }},
    }}, (response) => {{
      let body = "";
      response.setEncoding("utf8");
      response.on("data", (chunk) => {{
        body += chunk;
        if (body.length > 128) request.destroy(failure());
      }});
      response.on("error", () => reject(failure()));
      response.on("end", () => {{
        const match = {reply}.exec(body);
        if (response.statusCode !== 200 || !match || match[1] !== {port}) reject(failure());
        else resolve(body.split({separator})[0]);
      }});
    }});
    const timer = setTimeout(() => request.destroy(failure()), 5000);
    request.on("close", () => clearTimeout(timer));
    request.on("error", () => reject(failure()));
  }});
}}"#
    )
}
