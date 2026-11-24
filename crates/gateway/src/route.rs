//! Which profile a request belongs to and where it may go: key extraction, the `/p/<slug>`
//! prefix, the endpoint allowlist and the curated-catalog trigger (`routeRequest` and
//! `curatedCatalog` in `gateway.ts`). Functions over the request head; the host looks up the
//! key's profile.
//!
//! The TypeScript gateway checked the path that the WHATWG URL parser produced. This one checks
//! the raw request target, and refuses every target that parser would have rewritten in a way
//! that matters: `%`, `..`, `.` segments, and bytes outside RFC 3986 `pchar`. Each refusal
//! TypeScript made is still a refusal here; a few paths TypeScript normalised and forwarded
//! (`/v1/responses/./x`, `/v1/responses/a\b`) are refused instead.

use std::borrow::Cow;
use std::sync::Arc;

use hyper::header::{AUTHORIZATION, HeaderMap, HeaderName};
use hyper::{Method, StatusCode, Uri};
use nsv_domain::endpoint::{CATALOG_HEADER, CATALOG_QUERY, PROFILE_ROUTES};
use nsv_domain::registry::{Agent, AgentCatalog};
use nsv_domain::secret::KeyDigest;

use crate::{ProfileAuth, Refusal};

const API_KEY: HeaderName = HeaderName::from_static("x-api-key");
pub(crate) const AGENT_HEADER: HeaderName = HeaderName::from_static(CATALOG_HEADER);

/// The `permitted` expression of `gateway.ts`, split into exact paths and subtrees.
const EXACT: [&str; 14] = [
    "/v1/models",
    "/v1/chat/completions",
    "/v1/responses",
    "/v1/messages",
    "/v1/messages/count_tokens",
    "/v1/completions",
    "/v1/embeddings",
    "/v1/images/generations",
    "/v1/images/edits",
    "/v1/audio/speech",
    "/v1/audio/transcriptions",
    "/v1/ws",
    "/v1beta/models",
    "/v1interactions",
];
const SUBTREES: [&str; 3] = ["/v1/responses/", "/v1beta/models/", "/v1interactions/"];

/// A request the gateway accepted.
#[derive(Debug)]
pub(crate) struct Route {
    pub(crate) auth: Arc<ProfileAuth>,
    /// What the core receives: the path without `/p/<slug>`, and the query unchanged.
    pub(crate) target: Uri,
    /// Set for `GET /v1/models` from an agent asking for its curated catalog.
    pub(crate) catalog: Option<Agent>,
}

/// Routes a request head, in the TypeScript order: credentials (401), profile prefix (403),
/// endpoint (404). `profile` looks up the profile a key digest belongs to.
pub(crate) async fn route<F>(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    catalogs: &[(Agent, AgentCatalog)],
    profile: impl FnOnce(KeyDigest) -> F,
) -> Result<Route, Refusal>
where
    F: Future<Output = Option<Arc<ProfileAuth>>>,
{
    let key = presented_key(headers)?;
    // The live key decides, so deletion and rotation fail closed even past a stale index.
    let auth = match std::str::from_utf8(&key) {
        Ok(key) => profile(KeyDigest::of(key))
            .await
            .filter(|auth| auth.client_key.matches(key)),
        Err(_) => None,
    }
    .ok_or_else(|| Refusal::new(StatusCode::UNAUTHORIZED, "Invalid profile API key."))?;
    let path = match uri.path().strip_prefix(PROFILE_ROUTES) {
        None => uri.path(),
        // A slug is `[a-z0-9-]+`, so the prefix names this profile exactly when the slug is
        // followed by a slash.
        Some(rest) => rest
            .strip_prefix(auth.slug.as_str())
            .filter(|tail| tail.starts_with('/'))
            .ok_or_else(|| {
                Refusal::new(
                    StatusCode::FORBIDDEN,
                    "This API key does not belong to the requested profile.",
                )
            })?,
    };
    let query = uri.query().filter(|query| !query.is_empty());
    let unsupported = || Refusal::new(StatusCode::NOT_FOUND, "Unsupported proxy endpoint.");
    if !allowed(path, query) {
        return Err(unsupported());
    }
    let target = match query {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    };
    let target = Uri::try_from(target).map_err(|_| unsupported())?;
    let catalog = if method == Method::GET && path == "/v1/models" {
        curated(headers, query, catalogs)
    } else {
        None
    };
    Ok(Route {
        auth,
        target,
        catalog,
    })
}

/// The key from `Authorization: Bearer` (its first line, as Node keeps) or `x-api-key` (its
/// lines joined, as Node joins them, so a repeated key never matches a profile).
fn presented_key(headers: &HeaderMap) -> Result<Cow<'_, [u8]>, Refusal> {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|value| bearer_token(value.as_bytes()));
    let header_key = joined(headers, &API_KEY).filter(|key| !key.is_empty());
    if let (Some(bearer), Some(header_key)) = (bearer, &header_key)
        && bearer != &**header_key
    {
        return Err(Refusal::new(
            StatusCode::UNAUTHORIZED,
            "Conflicting client credentials.",
        ));
    }
    bearer
        .map(Cow::Borrowed)
        .or(header_key)
        .ok_or_else(|| Refusal::new(StatusCode::UNAUTHORIZED, "A profile API key is required."))
}

/// `/^Bearer (\S+)$/i` over the trimmed value: one space, then one token with no JavaScript
/// whitespace (Node reads header bytes as Latin-1, so `0xA0` is a no-break space).
fn bearer_token(value: &[u8]) -> Option<&[u8]> {
    let (scheme, rest) = value.trim_ascii().split_at_checked(6)?;
    let token = rest.strip_prefix(b" ")?;
    let space = |byte: &u8| matches!(byte, b'\t'..=b'\r' | b' ' | 0xA0);
    (scheme.eq_ignore_ascii_case(b"Bearer") && !token.is_empty() && !token.iter().any(space))
        .then_some(token)
}

/// A header's trimmed lines joined with `", "`, as Node presents a repeated header.
pub(crate) fn joined<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<Cow<'a, [u8]>> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.as_bytes().trim_ascii();
    let Some(second) = values.next() else {
        return Some(Cow::Borrowed(first));
    };
    let mut out = first.to_vec();
    for value in std::iter::once(second).chain(values) {
        out.extend_from_slice(b", ");
        out.extend_from_slice(value.as_bytes().trim_ascii());
    }
    Some(Cow::Owned(out))
}

fn allowed(path: &str, query: Option<&str>) -> bool {
    (EXACT.contains(&path) || SUBTREES.iter().any(|subtree| path.starts_with(subtree)))
        && !path.contains("..")
        && path.bytes().all(path_byte)
        && !path.split('/').any(|segment| segment == ".")
        && query.is_none_or(|query| {
            // `#` never reaches here from a parsed target; refusing it keeps the query the core
            // parses identical to the one checked.
            !query.contains('#')
                && !pairs(query)
                    .any(|(name, _)| decodes_to(name, "key") || decodes_to(name, "api_key"))
        })
}

/// RFC 3986 `pchar` and `/`, without `%`.
fn path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&byte)
}

/// `URLSearchParams` pairs: `&`-separated, split at the first `=`, still encoded.
fn pairs(query: &str) -> impl Iterator<Item = (&str, &str)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
}

fn decodes_to(encoded: &str, expected: &str) -> bool {
    FormDecoded(encoded.as_bytes()).eq(expected.bytes())
}

/// `application/x-www-form-urlencoded` decoding as `URLSearchParams` does it: `+` is a space,
/// `%XX` is that byte, and any other `%` stands for itself.
struct FormDecoded<'a>(&'a [u8]);

impl Iterator for FormDecoded<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        let (&first, rest) = self.0.split_first()?;
        if first == b'%'
            && let [high, low, tail @ ..] = rest
            && let (Some(high), Some(low)) = (hex(*high), hex(*low))
        {
            self.0 = tail;
            return Some((high << 4) | low);
        }
        self.0 = rest;
        Some(if first == b'+' { b' ' } else { first })
    }
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The agent asking for its curated catalog: the header first, then the first `nonstopvibin`
/// query parameter, each only for an agent whose registry entry names that mechanism.
fn curated(
    headers: &HeaderMap,
    query: Option<&str>,
    catalogs: &[(Agent, AgentCatalog)],
) -> Option<Agent> {
    let agent = |mechanism: AgentCatalog, matches: &dyn Fn(&str) -> bool| {
        catalogs
            .iter()
            .find(|&&(agent, by)| by == mechanism && matches(agent.as_str()))
            .map(|&(agent, _)| agent)
    };
    joined(headers, &AGENT_HEADER)
        .and_then(|value| agent(AgentCatalog::Header, &|id| id.as_bytes() == &*value))
        .or_else(|| {
            let (_, value) = pairs(query?).find(|&(name, _)| decodes_to(name, CATALOG_QUERY))?;
            agent(AgentCatalog::Query, &|id| decodes_to(value, id))
        })
}

#[cfg(test)]
mod tests {
    use hyper::header::HeaderValue;
    use nsv_domain::id::{ProfileId, ProfileSlug};
    use nsv_domain::secret::{ClientKey, CoreKey};
    use proptest::prelude::*;

    use super::*;

    const WORK_KEY: &str = "nv_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PERSONAL_KEY: &str = "nv_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CATALOGS: [(Agent, AgentCatalog); 3] = [
        (Agent::Codex, AgentCatalog::Header),
        (Agent::Pi, AgentCatalog::Query),
        (Agent::Opencode, AgentCatalog::Query),
    ];

    fn work(digest: KeyDigest) -> std::future::Ready<Option<Arc<ProfileAuth>>> {
        std::future::ready((digest == KeyDigest::of(WORK_KEY)).then(|| {
            Arc::new(
                ProfileAuth::new(
                    ProfileId::parse("11111111-1111-4111-8111-111111111111").unwrap(),
                    ProfileSlug::parse("work").unwrap(),
                    ClientKey::new(WORK_KEY),
                    &CoreKey::new("core-work"),
                )
                .unwrap(),
            )
        }))
    }

    /// Routes with [`work`], which answers at once, so the first poll ends the route.
    fn routed_with(method: &Method, uri: &Uri, headers: &HeaderMap) -> Result<Route, Refusal> {
        let routing = std::pin::pin!(route(method, uri, headers, &CATALOGS, work));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match routing.poll(&mut context) {
            std::task::Poll::Ready(routed) => routed,
            std::task::Poll::Pending => panic!("the lookup answers at once"),
        }
    }

    fn headers(pairs: &[(&str, &[u8])]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_bytes(value).unwrap(),
            );
        }
        map
    }

    fn routed(target: &str, pairs: &[(&str, &[u8])]) -> Result<Route, Refusal> {
        let uri = Uri::try_from(target).unwrap();
        routed_with(&Method::GET, &uri, &headers(pairs))
    }

    fn bearer(key: &str) -> Vec<u8> {
        format!("Bearer {key}").into_bytes()
    }

    #[test]
    fn conflicting_credentials_are_refused_before_lookup() {
        let refusal = routed(
            "/v1/models",
            &[
                ("authorization", &bearer(WORK_KEY)),
                ("x-api-key", PERSONAL_KEY.as_bytes()),
            ],
        )
        .unwrap_err();
        assert_eq!(refusal.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(refusal.message(), "Conflicting client credentials.");
        // The same key twice is one credential.
        let same = [
            ("authorization", bearer(WORK_KEY)),
            ("x-api-key", WORK_KEY.as_bytes().to_vec()),
        ];
        let same: Vec<(&str, &[u8])> = same.iter().map(|(n, v)| (*n, v.as_slice())).collect();
        assert_eq!(
            routed("/v1/models", &same).unwrap().auth.slug.as_str(),
            "work"
        );
    }

    #[test]
    fn query_names_are_decoded_before_the_key_check() {
        let auth = bearer(WORK_KEY);
        for target in [
            "/v1/models?%6Bey=1",
            "/v1/models?api%5Fkey=1",
            "/v1/models?a&k%65y",
        ] {
            let refusal = routed(target, &[("authorization", &auth)]).unwrap_err();
            assert_eq!(refusal.status(), StatusCode::NOT_FOUND, "{target}");
        }
        // Names compare exactly, as `URLSearchParams.has` does.
        assert!(routed("/v1/models?KEY=1", &[("authorization", &auth)]).is_ok());
        assert!(routed("/v1/models?key%3D1", &[("authorization", &auth)]).is_ok());
    }

    #[test]
    fn the_catalog_header_wins_over_the_query() {
        let auth = bearer(WORK_KEY);
        let route = routed(
            "/v1/models?nonstopvibin=pi",
            &[("authorization", &auth), ("x-nonstopvibin-agent", b"codex")],
        )
        .unwrap();
        assert_eq!(route.catalog, Some(Agent::Codex));
        // pi is a query agent, so its header falls through to the query.
        let route = routed(
            "/v1/models?nonstopvibin=%70i",
            &[("authorization", &auth), ("x-nonstopvibin-agent", b"pi")],
        )
        .unwrap();
        assert_eq!(route.catalog, Some(Agent::Pi));
    }

    /// A query parameter name spelling `key` or `api_key` in a random mix of raw and
    /// percent-encoded bytes.
    fn encoded_name() -> impl Strategy<Value = String> {
        (prop_oneof![Just("key"), Just("api_key")], any::<u64>()).prop_map(|(name, bits)| {
            name.bytes()
                .enumerate()
                .map(|(index, byte)| match (bits >> (2 * index)) & 3 {
                    0 => format!("%{byte:02X}"),
                    1 => format!("%{byte:02x}"),
                    _ => char::from(byte).to_string(),
                })
                .collect()
        })
    }

    /// Request targets assembled from the pieces the allowlist and the key check look at.
    fn target() -> impl Strategy<Value = String> {
        let piece = prop_oneof![
            Just("/"),
            Just("/v1"),
            Just("/v1/responses"),
            Just("/v1/models"),
            Just("/v1beta/models/"),
            Just("/v1interactions"),
            Just("/p/work"),
            Just("/p/"),
            Just("/v0/management"),
            Just(".."),
            Just("."),
            Just("%"),
            Just("%2e"),
            Just("%2F"),
            Just("?"),
            Just("&"),
            Just("="),
            Just("+"),
            Just("#"),
            Just(";"),
            Just("\\"),
            Just("key"),
            Just("api_key"),
            Just("k"),
            Just("%6B"),
            Just("e"),
            Just("%65"),
            Just("y"),
            Just("_"),
            Just("%5F"),
            Just("nonstopvibin"),
            Just("pi"),
            Just("x"),
        ];
        prop::collection::vec(piece, 1..12).prop_map(|pieces| pieces.concat())
    }

    /// The core's view of a query: Go's `url.ParseQuery` decodes names like this.
    fn go_names(query: &str) -> Vec<Vec<u8>> {
        query
            .split('&')
            .map(|pair| pair.split_once('=').map_or(pair, |(name, _)| name))
            .map(|name| FormDecoded(name.as_bytes()).collect())
            .collect()
    }

    fn assert_safe(route: &Route) {
        let path = route.target.path();
        assert!(
            EXACT.contains(&path) || SUBTREES.iter().any(|subtree| path.starts_with(subtree)),
            "{path}"
        );
        assert!(!path.contains("..") && !path.contains('%') && !path.contains('\\'));
        assert!(!path.split('/').any(|segment| segment == "."));
        if let Some(query) = route.target.query() {
            for name in go_names(query) {
                assert!(name != b"key" && name != b"api_key", "{query}");
            }
        }
    }

    proptest! {
        #[test]
        fn accepted_targets_stay_on_the_allowlist(target in target(), post in any::<bool>()) {
            let Ok(uri) = Uri::try_from(target.as_str()) else { return Ok(()) };
            let auth = bearer(WORK_KEY);
            let method = if post { Method::POST } else { Method::GET };
            if let Ok(route) = routed_with(&method, &uri, &headers(&[("authorization", &auth)])) {
                assert_safe(&route);
            }
        }

        #[test]
        fn key_parameters_are_refused_in_every_encoding(
            path in prop_oneof![Just("/v1/models"), Just("/p/work/v1/messages"), Just("/v1beta/models/x")],
            name in encoded_name(),
            value in "[a-z0-9%+]{0,6}",
            before in "[a-z]{0,3}",
            after in "[a-z]{0,3}",
        ) {
            let target = format!("{path}?{before}={before}&{name}={value}&{after}");
            let auth = bearer(WORK_KEY);
            let refusal = routed(&target, &[("authorization", &auth)]).unwrap_err();
            prop_assert_eq!(refusal.status(), StatusCode::NOT_FOUND);
        }

        #[test]
        fn header_bytes_never_panic_and_only_the_live_key_passes(
            authorization in prop::collection::vec(prop_oneof![
                Just(b"Bearer ".to_vec()), Just(b"bearer".to_vec()), Just(b" ".to_vec()),
                Just(b"\t".to_vec()), Just(vec![0xA0]), Just(WORK_KEY.as_bytes().to_vec()),
                Just(b"nv_".to_vec()), Just(b"x".to_vec()),
            ], 0..4),
            api_keys in prop::collection::vec(prop_oneof![
                Just(WORK_KEY.as_bytes().to_vec()), Just(PERSONAL_KEY.as_bytes().to_vec()),
                Just(Vec::new()), Just(vec![0xA0, b'x']),
            ], 0..3),
        ) {
            let authorization = authorization.concat();
            let mut pairs: Vec<(&str, &[u8])> = Vec::new();
            if HeaderValue::from_bytes(&authorization).is_ok() {
                pairs.push(("authorization", &authorization));
            }
            for key in &api_keys {
                pairs.push(("x-api-key", key));
            }
            if routed("/v1/models", &pairs).is_ok() {
                let bearer = headers(&pairs).get(AUTHORIZATION).and_then(|v| bearer_token(v.as_bytes()).map(<[u8]>::to_vec));
                let header = (api_keys.len() == 1).then(|| api_keys[0].trim_ascii().to_vec()).filter(|k| !k.is_empty());
                let presented = bearer.or(header).unwrap();
                prop_assert_eq!(presented, WORK_KEY.as_bytes().to_vec());
            }
        }
    }
}
