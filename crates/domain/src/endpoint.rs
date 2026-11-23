//! Where the daemon is reached and what its addresses look like: the loopback origin, a
//! profile's gateway routes, the catalog request an agent makes, the session and window links,
//! and the content security policy of every page it serves.

use serde::{Deserialize, Serialize};

use crate::id::ProfileId;
use crate::secret::SessionToken;

/// The host every local link names and the daemon listens on.
pub const LOOPBACK_HOST: &str = "127.0.0.1";

/// `http://127.0.0.1:<port>`, the daemon's origin on its loopback port.
#[must_use]
pub fn loopback_origin(port: u16) -> String {
    format!("http://{LOOPBACK_HOST}:{port}")
}

/// `<origin>/#session=<token>`: the fragment never reaches the server.
#[must_use]
pub fn session_link(origin: &str, token: &SessionToken) -> String {
    format!("{origin}/#session={}", token.expose_secret())
}

/// The prefix of one profile's routes on the gateway, `/p/<slug>`.
pub const PROFILE_ROUTES: &str = "/p/";

/// `/p/<slug>`: one profile's routes on the gateway.
#[must_use]
pub fn profile_path(slug: &str) -> String {
    format!("{PROFILE_ROUTES}{slug}")
}

/// The gateway's route for one profile on `port`; Anthropic clients add `/v1` themselves.
#[must_use]
pub fn profile_base(port: u16, slug: &str) -> String {
    format!("{}{}", loopback_origin(port), profile_path(slug))
}

/// The OpenAI-style base URL of one profile on the gateway.
#[must_use]
pub fn profile_endpoint(port: u16, slug: &str) -> String {
    format!("{}/v1", profile_base(port, slug))
}

/// `catalogRequest.query`: how generated pi and `OpenCode` code asks `GET /v1/models` for its
/// curated catalog.
pub const CATALOG_QUERY: &str = "nonstopvibin";
/// `catalogRequest.header`: how Codex asks for its curated catalog.
pub const CATALOG_HEADER: &str = "x-nonstopvibin-agent";
/// The response header that names the profile a catalog answer belongs to.
pub const PROFILE_HEADER: &str = "x-nonstopvibin-profile";

/// The content security policy of every page the daemon serves, byte-identical to the
/// TypeScript server's.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'";

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

    /// The main window's page, opened on `profile` when one is given.
    #[must_use]
    pub fn main_url(&self, profile: Option<&ProfileId>) -> String {
        match profile {
            Some(profile) => format!("{}/?profile={profile}", self.0),
            None => self.0.clone(),
        }
    }

    /// The quota popup's page.
    #[must_use]
    pub fn tray_url(&self) -> String {
        format!("{}/?view=tray", self.0)
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn links_name_the_loopback_origin() {
        let origin = Origin::parse(&loopback_origin(4320)).unwrap();
        assert_eq!(origin.as_str(), "http://127.0.0.1:4320");
        assert_eq!(
            session_link(origin.as_str(), &SessionToken::new("t")),
            "http://127.0.0.1:4320/#session=t"
        );
        assert_eq!(origin.tray_url(), "http://127.0.0.1:4320/?view=tray");
        let profile = ProfileId::parse("6f1c1a52-6d1f-4b5e-9c55-0a4b0c7e1d2f").unwrap();
        assert_eq!(
            origin.main_url(Some(&profile)),
            "http://127.0.0.1:4320/?profile=6f1c1a52-6d1f-4b5e-9c55-0a4b0c7e1d2f"
        );
        assert_eq!(
            profile_endpoint(4320, "work"),
            "http://127.0.0.1:4320/p/work/v1"
        );
    }
}
