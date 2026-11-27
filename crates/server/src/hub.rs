//! A profile's usage hub on the daemon's origin, `/p/<slug>/v0/management/…`: the four
//! `CLIProxyAPI` management endpoints T3 Code reads subscription limits through, behind the
//! profile's hub key. Loopback-only like the agent gateway; [`Cores`] answers each one.

use std::sync::Arc;

use hyper::body::Incoming;
use hyper::header::{AUTHORIZATION, HeaderMap};
use hyper::{Request, Response, StatusCode};
use nsv_accounts::cores::Cores;
use nsv_domain::endpoint::PROFILE_ROUTES;
use nsv_domain::error::AppError;
use nsv_domain::id::{ProfileId, ProfileSlug};
use nsv_gateway::Body;
use nsv_store::Store;

use crate::api::{json, query, read_json};

/// Where the hub's endpoints are below `/p/<slug>/`, as `CLIProxyAPI` serves its own.
const MANAGEMENT: &str = "v0/management";
const BODY_LIMIT: usize = 64 * 1024;

/// What a hub request asks for.
#[derive(Debug, Clone, Copy)]
enum Endpoint {
    Files,
    Models,
    ApiCall,
    ResetQuota,
}

/// `(method, path below /v0/management, endpoint)`: every request the hub answers.
const ENDPOINTS: [(&str, &str, Endpoint); 4] = [
    ("GET", "/auth-files", Endpoint::Files),
    ("GET", "/auth-files/models", Endpoint::Models),
    ("POST", "/api-call", Endpoint::ApiCall),
    ("POST", "/reset-quota", Endpoint::ResetQuota),
];

/// The slug and the rest of a hub path, `/p/<slug>/v0/management` and anything below it.
pub(crate) fn target(path: &str) -> Option<(&str, &str)> {
    let (slug, rest) = path.strip_prefix(PROFILE_ROUTES)?.split_once('/')?;
    let tail = rest.strip_prefix(MANAGEMENT)?;
    (tail.is_empty() || tail.starts_with('/')).then_some((slug, tail))
}

fn not_found() -> AppError {
    AppError::new("Not found.", 404)
}

pub(crate) struct Hub {
    store: Store,
    cores: Arc<Cores>,
}

impl Hub {
    pub(crate) fn new(store: Store, cores: Arc<Cores>) -> Self {
        Self { store, cores }
    }

    /// Answers a request [`target`] names, once it presented the profile's hub key.
    pub(crate) async fn answer(
        &self,
        request: Request<Incoming>,
        url: &url::Url,
    ) -> Result<Response<Body>, AppError> {
        let (slug, tail) = target(url.path()).ok_or_else(not_found)?;
        let slug = ProfileSlug::parse(slug).map_err(|_| not_found())?;
        let profile = self.authorize(slug, request.headers()).await?;
        let (.., endpoint) = ENDPOINTS
            .iter()
            .find(|(method, path, _)| *method == request.method().as_str() && *path == tail)
            .ok_or_else(not_found)?;
        let cores = &self.cores;
        let answer = match endpoint {
            Endpoint::Files => cores.hub_files(&profile).await?,
            Endpoint::Models => {
                let name = query(url, "name")
                    .ok_or_else(|| AppError::new("The name query is required.", 400))?;
                cores.hub_models(&profile, &name).await?
            }
            Endpoint::ApiCall => {
                let body = read_json(request, BODY_LIMIT, "Payload exceeds 64 KB.").await?;
                cores.hub_api_call(&profile, &body).await?
            }
            Endpoint::ResetQuota => {
                let body = read_json(request, BODY_LIMIT, "Payload exceeds 64 KB.").await?;
                cores.hub_reset_quota(&profile, &body).await?
            }
        };
        json(StatusCode::OK, &answer)
    }

    /// The profile whose hub key `headers` present as `Authorization: Bearer <key>`. Every
    /// failure is the same 401, so a client cannot tell which slugs exist.
    async fn authorize(
        &self,
        slug: ProfileSlug,
        headers: &HeaderMap,
    ) -> Result<ProfileId, AppError> {
        let refused = || AppError::new("Invalid usage hub key.", 401);
        let presented = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(refused)?;
        let stored = self
            .store
            .call(move |db| {
                let Some(profile) = db.profile_by_slug(slug.as_str())? else {
                    return Ok(None);
                };
                Ok(db.hub_key(&profile.id)?.map(|key| (profile.id, key)))
            })
            .await?;
        match stored {
            Some((profile, key)) if key.matches(presented) => Ok(profile),
            _ => Err(refused()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_paths_are_the_management_tree_of_one_profile_route() {
        assert_eq!(
            target("/p/work/v0/management/auth-files"),
            Some(("work", "/auth-files"))
        );
        assert_eq!(target("/p/work/v0/management"), Some(("work", "")));
        assert_eq!(target("/p/work/v0/management/"), Some(("work", "/")));
        assert_eq!(target("/p/work/v0/managementx"), None);
        assert_eq!(target("/p/work/v1/chat/completions"), None);
        assert_eq!(target("/p/work"), None);
        assert_eq!(target("/v0/management/auth-files"), None);
    }
}
