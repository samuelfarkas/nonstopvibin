//! Header rewriting in both directions (`forwardedHeaders` and `responseHeaders` in
//! `gateway.ts`), done in place on the parsed maps so no header is copied.
//!
//! Unlike the TypeScript gateway, hop-by-hop headers (RFC 9110 §7.6.1) and the names a
//! `Connection` header lists are dropped in both directions; the gateway sets its own
//! `Connection`. Repeated headers pass through as separate lines where Node joined them with
//! `", "`, which HTTP defines as the same field.

use std::net::SocketAddr;

use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use nsv_domain::endpoint::{OPENCODE_SESSION_HEADER, PROFILE_HEADER};
use nsv_domain::random::request_uuid;

use crate::route::{AGENT_HEADER, joined};

const SESSION: HeaderName = HeaderName::from_static(OPENCODE_SESSION_HEADER);
/// The caller's session, first header present wins; it keeps Go prompt caching per session.
const SESSION_SOURCES: [HeaderName; 4] = [
    SESSION,
    HeaderName::from_static("x-pi-session-id"),
    HeaderName::from_static("session_id"),
    HeaderName::from_static("x-session-id"),
];
pub(crate) const PROFILE: HeaderName = HeaderName::from_static(PROFILE_HEADER);

/// Agent credentials, browser and proxy headers the core never sees.
const DROPPED_REQUEST: [HeaderName; 13] = [
    header::HOST,
    header::AUTHORIZATION,
    HeaderName::from_static("x-api-key"),
    HeaderName::from_static("x-goog-api-key"),
    header::COOKIE,
    header::ORIGIN,
    header::REFERER,
    header::FORWARDED,
    HeaderName::from_static("x-forwarded-for"),
    HeaderName::from_static("x-forwarded-host"),
    HeaderName::from_static("x-forwarded-proto"),
    header::PROXY_AUTHORIZATION,
    AGENT_HEADER,
];
const DROPPED_RESPONSE: [HeaderName; 2] = [header::ACCESS_CONTROL_ALLOW_ORIGIN, header::SET_COOKIE];
const HOP_BY_HOP: [HeaderName; 8] = [
    header::CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
    header::PROXY_AUTHENTICATE,
];

/// The headers the core receives: the agent's own minus credentials, proxy and hop-by-hop
/// headers, plus the core key, the core's address as `Host`, a session id and `Connection`
/// (`close`, since every request has its own connection, or `upgrade` with the agent's
/// protocol). `None` when a value is not a valid header value, which a well-formed core key
/// never is.
pub(crate) fn forwarded(
    mut headers: HeaderMap,
    authorization: HeaderValue,
    core: SocketAddr,
    upgrade: Option<HeaderValue>,
) -> Option<HeaderMap> {
    let session = match SESSION_SOURCES
        .iter()
        .find_map(|name| joined(&headers, name))
    {
        Some(value) => HeaderValue::from_bytes(&value).ok()?,
        None => HeaderValue::try_from(request_uuid()).ok()?,
    };
    for name in DROPPED_REQUEST
        .iter()
        .chain(&HOP_BY_HOP)
        .chain(&connection_listed(&headers))
    {
        headers.remove(name);
    }
    headers.insert(header::HOST, HeaderValue::try_from(core.to_string()).ok()?);
    headers.insert(header::AUTHORIZATION, authorization);
    headers.insert(SESSION, session);
    match upgrade {
        Some(protocol) => {
            headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
            headers.insert(header::UPGRADE, protocol);
        }
        None => {
            headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
        }
    }
    Some(headers)
}

/// The core's response headers as the agent gets them: no CORS grant, cookies or hop-by-hop
/// headers (a `101` keeps `Connection` and `Upgrade`), and the profile's slug.
pub(crate) fn rewrite_response(headers: &mut HeaderMap, slug: HeaderValue, switching: bool) {
    let kept =
        |name: &HeaderName| switching && (name == header::CONNECTION || name == header::UPGRADE);
    for name in DROPPED_RESPONSE
        .iter()
        .chain(&HOP_BY_HOP)
        .chain(&connection_listed(headers))
    {
        if !kept(name) {
            headers.remove(name);
        }
    }
    headers.insert(PROFILE, slug);
}

/// The header names a `Connection` header lists, which belong to this hop only.
fn connection_listed(headers: &HeaderMap) -> Vec<HeaderName> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .flat_map(|value| value.as_bytes().split(|&byte| byte == b','))
        .filter_map(|token| HeaderName::from_bytes(token.trim_ascii()).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_listed_headers_are_dropped() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, HeaderValue::from_static("close, X-Hop"));
        headers.insert("x-hop", HeaderValue::from_static("1"));
        headers.insert("x-kept", HeaderValue::from_static("2"));
        headers.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        let core: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let out = forwarded(headers, HeaderValue::from_static("Bearer k"), core, None).unwrap();
        assert!(!out.contains_key("x-hop"));
        assert!(!out.contains_key(header::TRANSFER_ENCODING));
        assert_eq!(out["x-kept"], "2");
        assert_eq!(out[header::CONNECTION], "close");
        assert_eq!(out[header::HOST], "127.0.0.1:9");
        assert_eq!(out[header::AUTHORIZATION], "Bearer k");
    }

    #[test]
    fn a_switching_response_keeps_its_upgrade_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(header::SET_COOKIE, HeaderValue::from_static("a=1"));
        rewrite_response(&mut headers, HeaderValue::from_static("work"), true);
        assert_eq!(headers[header::UPGRADE], "websocket");
        assert_eq!(headers[header::CONNECTION], "Upgrade");
        assert!(!headers.contains_key(header::SET_COOKIE));
        assert_eq!(headers[PROFILE], "work");
    }
}
