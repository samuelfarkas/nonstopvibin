//! The `https://host[:port]/path?query` URLs the HTTPS client accepts, and redirect targets
//! resolved against them.

/// An HTTPS URL without credentials; the fragment is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Url {
    /// Lowercase; an IPv6 address without brackets.
    pub(crate) host: String,
    pub(crate) port: u16,
    /// Origin form: the path and query, starting with `/`.
    pub(crate) target: String,
}

impl Url {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let rest = strip_prefix_ignore_case(text, "https://")?;
        Self::from_authority(rest)
    }

    /// `host[:port][/path]`, the part after `https://` or `//`.
    fn from_authority(rest: &str) -> Option<Self> {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, target) = rest.split_at(end);
        let (host, port) = split_port(authority)?;
        Some(Self {
            host,
            port,
            target: origin_form(target)?,
        })
    }

    /// Where a `Location` header points from this URL: an absolute HTTPS URL, a
    /// scheme-relative one, or an absolute path. Anything else is not followed.
    pub(crate) fn redirect(&self, location: &str) -> Option<Self> {
        if let Some(rest) = location.strip_prefix("//") {
            Self::from_authority(rest)
        } else if location.starts_with('/') {
            Some(Self {
                host: self.host.clone(),
                port: self.port,
                target: origin_form(location)?,
            })
        } else {
            Self::parse(location)
        }
    }

    /// The `Host` header value.
    pub(crate) fn authority(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == 443 {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| text.get(prefix.len()..))
        .flatten()
}

fn split_port(authority: &str) -> Option<(String, u16)> {
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (address, after) = bracketed.split_once(']')?;
        if address.is_empty() || !address.chars().all(|c| c.is_ascii_hexdigit() || c == ':') {
            return None;
        }
        (address, after.strip_prefix(':').map_or(Some(""), Some)?)
    } else {
        let (host, port) = authority.split_once(':').unwrap_or((authority, ""));
        let valid = !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
        if !valid {
            return None;
        }
        (host, port)
    };
    let port = if port.is_empty() {
        443
    } else if port.chars().all(|c| c.is_ascii_digit()) {
        port.parse().ok().filter(|&p| p != 0)?
    } else {
        return None;
    };
    Some((host.to_ascii_lowercase(), port))
}

/// The path and query without the fragment; `/` for none. Whitespace and control characters
/// are refused rather than escaped.
fn origin_form(target: &str) -> Option<String> {
    let target = target.split_once('#').map_or(target, |(before, _)| before);
    if target.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    Some(if target.is_empty() {
        "/".to_owned()
    } else if target.starts_with('?') {
        format!("/{target}")
    } else {
        target.to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(host: &str, port: u16, target: &str) -> Url {
        Url {
            host: host.to_owned(),
            port,
            target: target.to_owned(),
        }
    }

    #[test]
    fn parses_only_https_urls_without_credentials() {
        assert_eq!(
            Url::parse("https://GitHub.com/a/b?c=1#frag"),
            Some(url("github.com", 443, "/a/b?c=1"))
        );
        assert_eq!(Url::parse("HTTPS://h:8443"), Some(url("h", 8443, "/")));
        assert_eq!(Url::parse("https://[::1]:9/x"), Some(url("::1", 9, "/x")));
        assert_eq!(Url::parse("https://h?q"), Some(url("h", 443, "/?q")));
        for refused in [
            "http://github.com/",
            "https://user:pass@github.com/",
            "https://user@github.com/",
            "https:///path",
            "https://h:0/",
            "https://h:99999/",
            "https://h:x/",
            "https://h/a b",
            "https://h/a\nb",
            "https://[nothex]/",
            "ftp://h/",
            "github.com/x",
        ] {
            assert_eq!(Url::parse(refused), None, "{refused}");
        }
    }

    #[test]
    fn resolves_redirect_locations() {
        let base = Url::parse("https://github.com:444/releases/x").unwrap();
        assert_eq!(
            base.redirect("/other?y"),
            Some(url("github.com", 444, "/other?y"))
        );
        assert_eq!(
            base.redirect("//objects.example/z"),
            Some(url("objects.example", 443, "/z"))
        );
        assert_eq!(
            base.redirect("https://release-assets.example/a"),
            Some(url("release-assets.example", 443, "/a"))
        );
        assert_eq!(base.redirect("http://insecure.example/"), None);
        assert_eq!(base.redirect("relative/path"), None);
    }

    #[test]
    fn host_header_names_the_port_only_when_not_443() {
        assert_eq!(url("h", 443, "/").authority(), "h");
        assert_eq!(url("h", 8443, "/").authority(), "h:8443");
        assert_eq!(url("::1", 9, "/").authority(), "[::1]:9");
    }
}
