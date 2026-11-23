//! The management API under `/api`: `src/shared/api.ts`'s routes in `server.ts`'s order, with its
//! path, body and query rules. Sign-in, credential, reset, API-account and core-update routes
//! are parsed here and answered by the account features ([`nsv_accounts::cores::Cores`]).

use std::sync::Arc;
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use nsv_accounts::accounts::listed;
use nsv_accounts::fetch::Fetch;
use nsv_agents::offered_models;
use nsv_core::install::Releases;
use nsv_domain::endpoint::{profile_endpoint, profile_path};
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::registry::registry;
use nsv_domain::sync::locked;
use nsv_domain::time::iso_string;
use nsv_domain::wire::{AppState, ClientKeyResponse, Profile, ProfileState, Runtime};
use nsv_gateway::Body;
use nsv_net::{Loopback, LoopbackRequest, loopback};
use serde_json::{Value, json};

use crate::daemon::{App, VERSION};
use crate::http::{Failure, enrich, json};
use crate::parse;

/// The largest management request body.
const BODY_LIMIT: usize = 2_097_152;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    State,
    Retention,
    CoreCheck,
    CoreInstall,
    CoreBundled,
    ImportSources,
    CreateProfile,
    PatchProfile,
    Start,
    Refresh,
    Key,
    Models,
    Usage,
    AgentStatus,
    AgentInstall,
    AgentRemove,
    AgentCheck,
    SignIn,
    SignInStatus,
    SignInCancel,
    SignInConfirm,
    SignInSession,
    Callback,
    Import,
    ImportSource,
    SetAccount,
    RemoveAccount,
    ResetCredits,
    ConsumeReset,
    ApiAccount,
    DesktopShow,
}

/// `server.ts`'s route table, in its order: a 405 lists methods in this order.
const ROUTES: &[(Method, &str, Route)] = &[
    (Method::GET, "/state", Route::State),
    (Method::PUT, "/usage-retention", Route::Retention),
    (Method::POST, "/core/check", Route::CoreCheck),
    (Method::POST, "/core/install", Route::CoreInstall),
    (Method::DELETE, "/core/install", Route::CoreBundled),
    (Method::GET, "/import-sources", Route::ImportSources),
    (Method::POST, "/profiles", Route::CreateProfile),
    (Method::PATCH, "/profiles/:profileId", Route::PatchProfile),
    (Method::POST, "/profiles/:profileId/start", Route::Start),
    (Method::POST, "/profiles/:profileId/refresh", Route::Refresh),
    (Method::GET, "/profiles/:profileId/key", Route::Key),
    (Method::GET, "/profiles/:profileId/models", Route::Models),
    (Method::GET, "/profiles/:profileId/usage", Route::Usage),
    (
        Method::GET,
        "/profiles/:profileId/agent-setup",
        Route::AgentStatus,
    ),
    (
        Method::POST,
        "/profiles/:profileId/agent-setup",
        Route::AgentInstall,
    ),
    (
        Method::DELETE,
        "/profiles/:profileId/agent-setup",
        Route::AgentRemove,
    ),
    (
        Method::POST,
        "/profiles/:profileId/agent-check",
        Route::AgentCheck,
    ),
    (Method::POST, "/profiles/:profileId/oauth", Route::SignIn),
    (
        Method::GET,
        "/profiles/:profileId/oauth",
        Route::SignInStatus,
    ),
    (
        Method::DELETE,
        "/profiles/:profileId/oauth",
        Route::SignInCancel,
    ),
    (
        Method::POST,
        "/profiles/:profileId/oauth-confirm",
        Route::SignInConfirm,
    ),
    (
        Method::GET,
        "/profiles/:profileId/oauth-session",
        Route::SignInSession,
    ),
    (
        Method::POST,
        "/profiles/:profileId/callback",
        Route::Callback,
    ),
    (Method::POST, "/profiles/:profileId/import", Route::Import),
    (
        Method::POST,
        "/profiles/:profileId/import-source",
        Route::ImportSource,
    ),
    (
        Method::PATCH,
        "/profiles/:profileId/accounts/:accountId",
        Route::SetAccount,
    ),
    (
        Method::DELETE,
        "/profiles/:profileId/accounts/:accountId",
        Route::RemoveAccount,
    ),
    (
        Method::GET,
        "/profiles/:profileId/accounts/:accountId/reset-credits",
        Route::ResetCredits,
    ),
    (
        Method::POST,
        "/profiles/:profileId/accounts/:accountId/reset-credits",
        Route::ConsumeReset,
    ),
    (
        Method::POST,
        "/profiles/:profileId/api-account",
        Route::ApiAccount,
    ),
    // A second desktop launch; only the desktop app serves it.
    (Method::POST, "/desktop/show", Route::DesktopShow),
];

/// A route without parameters matches the raw path exactly; one with parameters matches the
/// decoded segments, which skip empty ones. Answers the account parameter, empty if none.
fn matches(pattern: &str, raw: &str, segments: &[String]) -> Option<String> {
    if !pattern.contains(':') {
        return (raw.strip_prefix("/api") == Some(pattern)).then(String::new);
    }
    let parts: Vec<&str> = std::iter::once("api")
        .chain(pattern.split('/').filter(|part| !part.is_empty()))
        .collect();
    if parts.len() != segments.len() {
        return None;
    }
    let mut account = String::new();
    for (part, segment) in parts.iter().zip(segments) {
        match *part {
            ":profileId" => {}
            ":accountId" => account.clone_from(segment),
            literal if literal != segment => return None,
            _ => {}
        }
    }
    Some(account)
}

/// A matched request: its profile, account parameter and query.
struct Call<'a> {
    profile: Option<ProfileId>,
    account: String,
    url: &'a url::Url,
}

impl Call<'_> {
    /// `searchParams.get(name)`: the first value.
    fn query(&self, name: &str) -> Option<String> {
        self.url
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    fn profile(&self) -> Result<ProfileId, Failure> {
        self.profile
            .clone()
            .ok_or_else(|| Failure::Internal("the route has no profile".to_owned()))
    }

    /// The account parameter; no profile lists an id longer than an account id may be.
    fn account(&self) -> Result<AccountId, Failure> {
        AccountId::parse(&self.account)
            .map_err(|_| Failure::status(404, "Account not found in this profile."))
    }
}

/// `jsonBody`: a JSON content type, at most 2 MB, parsed.
async fn body(request: Request<Incoming>) -> Result<Value, Failure> {
    let json = request
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes().starts_with(b"application/json"));
    if !json {
        return Err(Failure::status(
            415,
            "Content-Type must be application/json.",
        ));
    }
    let mut incoming = request.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = incoming.frame().await {
        let frame = frame.map_err(|_| Failure::Aborted)?;
        if let Ok(data) = frame.into_data() {
            if bytes.len() + data.len() > BODY_LIMIT {
                return Err(Failure::status(413, "Payload exceeds 2 MB."));
            }
            bytes.extend_from_slice(&data);
        }
    }
    serde_json::from_str(&String::from_utf8_lossy(&bytes))
        .map_err(|_| Failure::status(400, "Invalid JSON."))
}

fn ok() -> Value {
    json!({ "ok": true })
}

impl<F: Fetch + Releases + Clone + 'static> App<F> {
    pub(crate) async fn api(
        &self,
        request: Request<Incoming>,
        url: &url::Url,
    ) -> Result<Response<Body>, Failure> {
        let raw = url.path();
        // Split on the raw "/" first, so an encoded slash stays inside its segment.
        let segments: Vec<String> = raw
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(parse::decode_component)
            .collect::<Option<_>>()
            .ok_or_else(|| Failure::status(400, "Malformed request path."))?;
        // Any profile path names a real profile before its route is chosen.
        let profile = match segments.get(1..3) {
            Some([profiles, id]) if profiles == "profiles" => {
                parse::uuid(id)?;
                let id = ProfileId::parse(id)
                    .map_err(|_| Failure::Invalid(": Invalid UUID".to_owned()))?;
                let found = id.clone();
                self.store.call(move |db| db.profile(&found)).await?;
                Some(id)
            }
            _ => None,
        };
        let mut allowed = Vec::new();
        for (method, pattern, route) in ROUTES {
            if *route == Route::DesktopShow && !self.shared.desktop {
                continue;
            }
            let Some(account) = matches(pattern, raw, &segments) else {
                continue;
            };
            if method == request.method() {
                let call = Call {
                    profile,
                    account,
                    url,
                };
                return self.route(*route, request, &call).await;
            }
            allowed.push(method.as_str());
        }
        if allowed.is_empty() {
            Err(Failure::status(404, "Not found."))
        } else {
            Err(Failure::NotAllowed(allowed.join(", ")))
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per route keeps the table readable against api.ts"
    )]
    async fn route(
        &self,
        route: Route,
        request: Request<Incoming>,
        call: &Call<'_>,
    ) -> Result<Response<Body>, Failure> {
        let cores = &self.cores;
        match route {
            Route::State => {
                // A UI polls with `watch` while open; the CLI's reads have no side effects.
                if call.query("watch").as_deref() == Some("1") {
                    self.cores.scheduler.watch().await;
                }
                self.state_reply().await
            }
            Route::Retention => {
                let days = parse::retention(&body(request).await?)?;
                self.store
                    .call(move |db| db.set_usage_retention_days(days))
                    .await?;
                self.state_reply().await
            }
            Route::CoreCheck => {
                cores.updates.check().await?;
                self.state_reply().await
            }
            Route::CoreInstall => {
                let version = parse::core_version(&body(request).await?)?;
                cores.updates.install(&version).await?;
                self.state_reply().await
            }
            Route::CoreBundled => {
                cores.updates.use_bundled().await?;
                self.state_reply().await
            }
            Route::ImportSources => {
                let imports = Arc::clone(&self.imports);
                let sources = tokio::task::spawn_blocking(move || locked(&imports).scan())
                    .await
                    .map_err(|error| Failure::Internal(error.to_string()))?;
                json(StatusCode::OK, &sources)
            }
            Route::CreateProfile => {
                let name = parse::profile_name(&body(request).await?)?;
                let profile = self.store.call(move |db| db.create_profile(&name)).await?;
                self.profiles_changed().await?;
                json(StatusCode::CREATED, &profile)
            }
            Route::PatchProfile => {
                let patch = parse::profile_patch(&body(request).await?)?;
                let id = call.profile()?;
                let saved = id.clone();
                self.store
                    .call(move |db| {
                        let mut profile = db.profile(&saved)?;
                        if let Some(name) = patch.name {
                            profile.name = name;
                        }
                        if let Some(strategy) = patch.strategy {
                            profile.strategy = strategy;
                        }
                        if let Some(affinity) = patch.session_affinity {
                            profile.session_affinity = affinity;
                        }
                        db.save_profile(&profile)
                    })
                    .await?;
                self.profiles_changed().await?;
                self.cores.scheduler.pool().configure(&id).await?;
                self.state_reply().await
            }
            Route::Start => {
                self.cores
                    .scheduler
                    .pool()
                    .start(&call.profile()?, false)
                    .await?;
                self.state_reply().await
            }
            Route::Refresh => {
                cores.refresh(&call.profile()?).await?;
                self.state_reply().await
            }
            Route::Key => {
                let id = call.profile()?;
                let key = self.store.call(move |db| db.client_key(&id)).await?;
                json(
                    StatusCode::OK,
                    &ClientKeyResponse {
                        key: key.expose_secret().to_owned(),
                    },
                )
            }
            Route::Models => {
                let metadata = call.query("metadata").as_deref() == Some("1");
                let id = call.profile()?;
                // The catalog comes from the core: a dormant one starts for it.
                self.cores.scheduler.wake(&id).await?;
                let models = self.cores.scheduler.pool().models(&id).await?;
                if metadata {
                    json(
                        StatusCode::OK,
                        &enrich(&self.catalog, &self.store, &id, &models).await?,
                    )
                } else {
                    json(StatusCode::OK, &models)
                }
            }
            Route::Usage => {
                let all = registry().limits.all_history_days;
                let days = parse::days(call.query("days").as_deref(), all)?.unwrap_or(7);
                let since = if days == all {
                    String::new()
                } else {
                    iso_string((self.clock)() - i64::from(days) * 86_400_000)
                };
                let history = self
                    .prices
                    .history(&self.store, &call.profile()?, &since, &self.catalog)
                    .await?;
                json(StatusCode::OK, &history)
            }
            Route::AgentStatus => {
                let mut query = serde_json::Map::new();
                query.insert(
                    "agent".to_owned(),
                    call.query("agent").map_or(Value::Null, Value::String),
                );
                if let Some(project) = call.query("projectDirectory") {
                    query.insert("projectDirectory".to_owned(), Value::String(project));
                }
                let selection = parse::agent_selection(&Value::Object(query))?;
                let profile = self.profile(call).await?;
                let status = self
                    .agents
                    .status(
                        &profile,
                        selection.agent,
                        selection.project_directory.as_deref(),
                    )
                    .await?;
                json(StatusCode::OK, &status)
            }
            Route::AgentInstall => {
                let input = parse::agent_setup(&body(request).await?)?;
                let id = call.profile()?;
                self.cores.scheduler.wake(&id).await?;
                let models = self.cores.scheduler.pool().models(&id).await?;
                if models.is_empty() {
                    return Err(Failure::status(
                        400,
                        "This profile has no available models.",
                    ));
                }
                let enriched = if registry().curated(input.agent) {
                    None
                } else {
                    Some(enrich(&self.catalog, &self.store, &id, &models).await?)
                };
                let offered = offered_models(
                    input.agent,
                    models,
                    input.other_providers.unwrap_or(true),
                    |_| async { enriched.unwrap_or_default() },
                )
                .await?;
                // Only an agent with a native vendor can be left without models.
                if offered.is_empty() {
                    return Err(Failure::status(
                        400,
                        format!(
                            "This profile has no {} models. Add one, or offer models from other providers.",
                            registry()
                                .agents
                                .get(input.agent)
                                .native
                                .as_deref()
                                .unwrap_or("")
                        ),
                    ));
                }
                let profile = self.profile(call).await?;
                let ids = offered.into_iter().map(|model| model.id).collect();
                json(
                    StatusCode::OK,
                    &self.agents.install(&profile, input, ids).await?,
                )
            }
            Route::AgentRemove => {
                let selection = parse::agent_selection(&body(request).await?)?;
                let profile = self.profile(call).await?;
                self.agents
                    .remove(
                        &profile,
                        selection.agent,
                        selection.project_directory.as_deref(),
                    )
                    .await?;
                json(StatusCode::OK, &ok())
            }
            Route::AgentCheck => {
                parse::empty(&body(request).await?)?;
                self.agent_check(call).await
            }
            Route::SignIn => {
                let (provider, reconnect) = parse::oauth_start(&body(request).await?)?;
                let session = cores
                    .sign_in
                    .begin(&call.profile()?, provider, reconnect)
                    .await?;
                json(StatusCode::OK, &session)
            }
            Route::SignInStatus => {
                let state = parse::state_query(call.query("state").as_deref())?;
                let status = cores.sign_in.status(&call.profile()?, &state).await?;
                json(StatusCode::OK, &status)
            }
            Route::SignInCancel => {
                let state = parse::state_query(call.query("state").as_deref())?;
                cores.sign_in.cancel(&call.profile()?, &state).await?;
                json(StatusCode::OK, &ok())
            }
            Route::SignInConfirm => {
                let state = parse::single(&body(request).await?, "state", 500)?;
                let status = cores.sign_in.confirm(&call.profile()?, &state).await?;
                json(StatusCode::OK, &status)
            }
            Route::SignInSession => json(StatusCode::OK, &cores.sign_in.session(&call.profile()?)),
            Route::Callback => {
                let url = parse::callback(&body(request).await?)?;
                cores.sign_in.callback(&call.profile()?, &url).await?;
                json(StatusCode::OK, &ok())
            }
            Route::Import => {
                let contents = parse::import(&body(request).await?)?;
                cores.credentials.import(&call.profile()?, contents).await?;
                self.state_reply().await
            }
            Route::ImportSource => {
                let id = parse::import_source(&body(request).await?)?;
                let imports = Arc::clone(&self.imports);
                let contents = tokio::task::spawn_blocking(move || locked(&imports).contents(&id))
                    .await
                    .map_err(|error| Failure::Internal(error.to_string()))??;
                cores.credentials.import(&call.profile()?, contents).await?;
                self.state_reply().await
            }
            Route::SetAccount => {
                let patch = parse::account_patch(&body(request).await?)?;
                cores
                    .credentials
                    .set_account(&call.profile()?, &call.account()?, patch)
                    .await?;
                self.state_reply().await
            }
            Route::RemoveAccount => {
                cores
                    .credentials
                    .remove_account(&call.profile()?, &call.account()?)
                    .await?;
                self.state_reply().await
            }
            Route::ResetCredits => {
                let source = cores.quotas.source();
                let credits = cores
                    .resets
                    .credits(source, &call.profile()?, &call.account()?)
                    .await?;
                json(StatusCode::OK, &credits)
            }
            Route::ConsumeReset => {
                let credit = parse::single(&body(request).await?, "creditId", 500)?;
                let result = cores
                    .resets
                    .consume(
                        cores.quotas.source(),
                        cores.quotas.as_ref(),
                        &call.profile()?,
                        &call.account()?,
                        &credit,
                    )
                    .await?;
                json(StatusCode::OK, &result)
            }
            Route::ApiAccount => {
                let input = parse::api_account(&body(request).await?)?;
                cores
                    .credentials
                    .add_api_account(&call.profile()?, input)
                    .await?;
                self.profiles_changed().await?;
                let state = self.state().await?;
                json(StatusCode::CREATED, &state)
            }
            Route::DesktopShow => {
                self.shared.show_main();
                json(StatusCode::OK, &ok())
            }
        }
    }

    async fn profile(&self, call: &Call<'_>) -> Result<Profile, Failure> {
        let id = call.profile()?;
        Ok(self.store.call(move |db| db.profile(&id)).await?)
    }

    /// The scheduler and the tray read profiles again.
    async fn profiles_changed(&self) -> Result<(), Failure> {
        self.cores.scheduler.changed().await?;
        self.refresh_names().await?;
        Ok(())
    }

    /// Asks the profile's own gateway endpoint for its models, as an agent would.
    async fn agent_check(&self, call: &Call<'_>) -> Result<Response<Body>, Failure> {
        let id = call.profile()?;
        let profile = self.profile(call).await?;
        let key = self.store.call(move |db| db.client_key(&id)).await?;
        // `<endpoint>/models` on this daemon's own port.
        let target = format!("{}/v1/models", profile_path(profile.slug.as_str()));
        let authorization = format!("Bearer {}", key.expose_secret());
        let response = loopback(LoopbackRequest {
            to: Loopback::v4(self.port),
            method: &Method::GET,
            target: &target,
            headers: &[("Authorization", &authorization)],
            body: None,
            timeout: Duration::from_secs(10),
            limit: 16 << 20,
        })
        .await
        .map_err(|error| Failure::Internal(error.to_string()))?;
        if !response.ok() {
            return Err(Failure::status(
                502,
                "The profile connection failed. Check that this profile is running.",
            ));
        }
        let catalog: Value = serde_json::from_slice(&response.body)
            .map_err(|error| Failure::Internal(error.to_string()))?;
        let listed = catalog
            .get("data")
            .and_then(Value::as_array)
            .is_some_and(|data| {
                data.iter()
                    .any(|entry| entry.get("id").is_some_and(Value::is_string))
            });
        if !listed {
            return Err(Failure::status(
                400,
                "No models are available. Refresh the profile catalog.",
            ));
        }
        json(StatusCode::OK, &ok())
    }

    async fn state_reply(&self) -> Result<Response<Body>, Failure> {
        let state = self.state().await?;
        json(StatusCode::OK, &state)
    }

    /// `Application.state()`.
    pub(crate) async fn state(&self) -> Result<AppState, Failure> {
        let (stored, retention) = self
            .store
            .call(|db| {
                let mut stored = Vec::new();
                for profile in db.profiles()? {
                    let accounts = listed(db, &profile.id)?;
                    stored.push((profile, accounts));
                }
                Ok((stored, db.usage_retention_days()?))
            })
            .await?;
        let pool = self.cores.scheduler.pool().state();
        let profiles = stored
            .into_iter()
            .map(|(profile, accounts)| {
                let (runtime, error) = pool.runtime(&profile.id);
                // Once the scheduler stopped restarting it, the profile says so instead of its
                // last failure.
                let gave_up = (runtime != Runtime::Running)
                    .then(|| self.cores.scheduler.gave_up(&profile.id))
                    .flatten();
                ProfileState {
                    error: gave_up.or_else(|| error.map(str::to_owned)),
                    runtime,
                    accounts,
                    endpoint: profile_endpoint(self.port, profile.slug.as_str()),
                    profile,
                }
            })
            .collect();
        Ok(AppState {
            profiles,
            core_version: self.cores.updates.version(),
            core: self.cores.updates.state(),
            gateway: self.origin.as_str().to_owned(),
            usage_retention_days: retention,
            version: VERSION.to_owned(),
            errors: self.shared.errors.list(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(path: &str) -> Vec<String> {
        path.split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn literal_routes_match_the_raw_path_and_parameter_routes_the_segments() {
        assert_eq!(
            matches("/state", "/api/state", &segments("/api/state")),
            Some(String::new())
        );
        assert_eq!(
            matches("/state", "/api/state/", &segments("/api/state/")),
            None
        );
        assert_eq!(
            matches("/state", "/api//state", &segments("/api//state")),
            None
        );
        let id = "6e4156ab-2acf-4d89-9a81-264f3b3da50a";
        let doubled = format!("/api//profiles//{id}//key");
        assert_eq!(
            matches("/profiles/:profileId/key", &doubled, &segments(&doubled)),
            Some(String::new())
        );
        let reset = format!("/api/profiles/{id}/accounts/a%2Fb/reset-credits");
        let mut decoded = segments(&reset);
        decoded[4] = "a/b".to_owned();
        assert_eq!(
            matches(
                "/profiles/:profileId/accounts/:accountId/reset-credits",
                &reset,
                &decoded
            ),
            Some("a/b".to_owned())
        );
    }
}
