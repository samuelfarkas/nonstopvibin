//! The built interface, served from its directory with `server.ts`'s headers: the CSP, no
//! sniffing, no referrer, a year's immutable cache for Vite's fingerprinted assets and
//! revalidation by `ETag` for everything else.

use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response, StatusCode};
use nsv_accounts::fetch::Fetch;
use nsv_core::install::Releases;
use nsv_domain::endpoint::CSP;
use nsv_domain::wire::JsNumber;
use nsv_gateway::Body;

use crate::daemon::App;
use crate::http::Failure;
use crate::parse::decode_component;

const UNBUILT: &str = "Build the interface with bun run build before opening this page.";

/// Node's `path.extname`, including its rules for leading dots and trailing slashes.
fn extname(path: &str) -> &str {
    let bytes = path.as_bytes();
    let mut start_dot = None;
    let mut start_part = 0;
    let mut end = None;
    let mut matched_slash = true;
    // 0: no character before the dot; 1: only dots; -1: another character.
    let mut pre_dot = 0_i8;
    for index in (0..bytes.len()).rev() {
        let byte = bytes.get(index).copied().unwrap_or(b'/');
        if byte == b'/' {
            if !matched_slash {
                start_part = index + 1;
                break;
            }
            continue;
        }
        if end.is_none() {
            matched_slash = false;
            end = Some(index + 1);
        }
        if byte == b'.' {
            if start_dot.is_none() {
                start_dot = Some(index);
            } else if pre_dot != 1 {
                pre_dot = 1;
            }
        } else if start_dot.is_some() {
            pre_dot = -1;
        }
    }
    match (start_dot, end) {
        (Some(dot), Some(end))
            if pre_dot != 0 && !(pre_dot == 1 && dot + 1 == end && dot == start_part + 1) =>
        {
            path.get(dot..end).unwrap_or("")
        }
        _ => "",
    }
}

/// `path.resolve(base, relative)` for an absolute base: `.` and `..` resolved lexically.
fn resolve(base: &Path, relative: &str) -> PathBuf {
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in base.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(std::ffi::OsStr::new(part)),
        }
    }
    let mut path = PathBuf::from("/");
    path.extend(parts);
    path
}

/// Vite's fingerprinted assets: `assets/<name>-<8+ word characters>.(js|css|woff2)`.
fn fingerprinted(relative: &str) -> bool {
    let Some(file) = relative.strip_prefix("assets/") else {
        return false;
    };
    if file.contains('/') {
        return false;
    }
    let Some(stem) = [".js", ".css", ".woff2"]
        .iter()
        .find_map(|extension| file.strip_suffix(extension))
    else {
        return false;
    };
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    stem.char_indices().any(|(index, c)| {
        c == '-'
            && index >= 1
            && stem
                .get(index + 1..)
                .is_some_and(|hash| hash.len() >= 8 && hash.bytes().all(word))
    })
}

fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// `If-None-Match` names this `ETag`, weakly compared, or `*`.
fn unchanged(request: &Request<Incoming>, etag: &str) -> bool {
    let opaque = etag.strip_prefix("W/").unwrap_or(etag);
    request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|list| {
            list.split(',').any(|value| {
                let value = value.trim();
                value == "*" || value.strip_prefix("W/").unwrap_or(value) == opaque
            })
        })
}

impl<F: Fetch + Releases + Clone + 'static> App<F> {
    /// `GET` or `HEAD` of a file of the interface; any path without an extension is the
    /// single-page app's `index.html`.
    pub(crate) async fn asset(
        &self,
        request: &Request<Incoming>,
        pathname: &str,
        head: bool,
    ) -> Result<Response<Body>, Failure> {
        let requested =
            decode_component(pathname).ok_or_else(|| Failure::status(404, "Not found."))?;
        let relative = if requested == "/" || extname(&requested).is_empty() {
            "index.html"
        } else {
            requested.trim_start_matches('/')
        };
        let root = resolve(&self.client, "");
        let path = resolve(&root, relative);
        if path == root || !path.starts_with(&root) {
            return Err(Failure::status(404, "Not found."));
        }
        let metadata = tokio::fs::metadata(&path)
            .await
            .ok()
            .filter(std::fs::Metadata::is_file)
            .ok_or_else(|| Failure::status(404, UNBUILT))?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .unwrap_or_default();
        // libuv's `mtimeMs`, written as JavaScript writes numbers.
        #[expect(
            clippy::cast_precision_loss,
            reason = "seconds since the epoch fit an f64 exactly"
        )]
        let seconds = modified.as_secs() as f64;
        let milliseconds = serde_json::to_string(&JsNumber(
            seconds * 1e3 + f64::from(modified.subsec_nanos()) / 1e6,
        ))
        .unwrap_or_default();
        let etag = format!(
            "W/\"{}-{}-{milliseconds}\"",
            self.asset_version,
            metadata.len()
        );
        let unchanged = unchanged(request, &etag);
        let body = if unchanged || head {
            Body::empty()
        } else {
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|_| Failure::status(404, UNBUILT))?;
            Body::full(bytes)
        };
        let mut response = Response::new(body);
        *response.status_mut() = if unchanged {
            StatusCode::NOT_MODIFIED
        } else {
            StatusCode::OK
        };
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime(&path)));
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        headers.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
        // Only fingerprinted assets survive an update without revalidation.
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(if fingerprinted(relative) {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            }),
        );
        if let Ok(value) = HeaderValue::from_str(&etag) {
            headers.insert(header::ETAG, value);
        }
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP),
        );
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extname_follows_node() {
        for (path, extension) in [
            ("/index.html", ".html"),
            ("/settings", ""),
            ("/.bashrc", ""),
            ("/a.b.c", ".c"),
            ("/file.", "."),
            ("/..", ""),
            ("/...", "."),
            ("/folder.d/", ".d"),
            ("/a/.hidden.js", ".js"),
        ] {
            assert_eq!(extname(path), extension, "{path}");
        }
    }

    #[test]
    fn resolve_never_leaves_the_root_lexically_unnoticed() {
        let root = Path::new("/srv/client");
        assert_eq!(resolve(root, "a/./b"), PathBuf::from("/srv/client/a/b"));
        assert_eq!(
            resolve(root, "../secret.txt"),
            PathBuf::from("/srv/secret.txt")
        );
        assert_eq!(
            resolve(root, "../client-evil.txt"),
            PathBuf::from("/srv/client-evil.txt")
        );
        assert!(!resolve(root, "../client-evil.txt").starts_with(root));
    }

    #[test]
    fn only_vite_fingerprints_are_immutable() {
        assert!(fingerprinted("assets/index-AbCd1234.js"));
        assert!(fingerprinted("assets/font-Zz_-9999.woff2"));
        assert!(fingerprinted("assets/a.b-12345678.css"));
        assert!(!fingerprinted("assets/short-abc.js"));
        assert!(!fingerprinted("assets/nested/deep-AbCd1234.js"));
        assert!(!fingerprinted("assets/-AbCd12345.js"));
        assert!(!fingerprinted("index-AbCd1234.js"));
        assert!(!fingerprinted("assets/index-AbCd1234.mjs"));
    }
}
