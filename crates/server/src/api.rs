//! The management API under `/api`: `src/shared/api.ts`'s routes in `server.ts`'s order, with its
//! path, body and query rules. Sign-in, credential, reset, API-account and core-update routes
//! are parsed here and answered by the account features ([`nsv_accounts::cores::Cores`]).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use nsv_accounts::accounts::listed;
use nsv_accounts::catalog::{ModelCatalog, profile_models};
use nsv_accounts::cores::Cores;
use nsv_accounts::imports::ImportCatalog;
use nsv_accounts::pricing::UsagePrices;
use nsv_agents::{AgentSetup, connection_models};
use nsv_domain::endpoint::{profile_endpoint, profile_path};
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::registry::registry;
use nsv_domain::sync::locked;
use nsv_domain::time::iso_string;
use nsv_domain::wire::{
    AccountRoute, AppState, ClientKeyResponse, Model, PlainRoute, Profile, ProfileRoute,
    ProfileState, ROUTES, Route,
};
use nsv_gateway::{Body, Refusal};
use nsv_net::{Loopback, LoopbackRequest, loopback};
use nsv_store::Store;
use serde::Serialize;
use serde_json::{Value, json};

use crate::daemon::{Shared, VERSION};
use crate::parse;
use crate::session::LOOPBACK_LIMIT;

const BODY_LIMIT: usize = 2_097_152;

/// A matched route with the path parameters it names.
enum Call {
    Plain(PlainRoute),
    Profile(ProfileRoute, ProfileId),
    /// The account parameter as the path decoded it; each route parses it where `server.ts` did.
    Account(AccountRoute, ProfileId, String),
}

/// The call `pattern` makes of a path, if it matches. A route without parameters matches the
/// raw path exactly; one with parameters matches the decoded segments, which skip empty ones,
/// and names the profile the path was checked for.
fn call(
    route: Route,
    pattern: &str,
    raw: &str,
    segments: &[String],
    profile: Option<&ProfileId>,
) -> Option<Call> {
    match route {
        Route::Plain(route) => {
            (raw.strip_prefix("/api") == Some(pattern)).then_some(Call::Plain(route))
        }
        Route::Profile(route) => {
            fit(pattern, segments)?;
            Some(Call::Profile(route, profile?.clone()))
        }
        Route::Account(route) => {
            let (_, account) = fit(pattern, segments)?
                .into_iter()
                .find(|(part, _)| *part == ":accountId")?;
            Some(Call::Account(route, profile?.clone(), account.to_owned()))
        }
    }
}

/// The decoded segments paired with `pattern`'s parts, if the path fits it: as many parts as
/// segments, and each literal part equal to its segment.
fn fit<'a>(pattern: &'a str, segments: &'a [String]) -> Option<Vec<(&'a str, &'a str)>> {
    let parts: Vec<&str> = std::iter::once("api")
        .chain(pattern.split('/').filter(|part| !part.is_empty()))
        .collect();
    if parts.len() != segments.len() {
        return None;
    }
    let pairs: Vec<(&str, &str)> = parts
        .into_iter()
        .zip(segments.iter().map(String::as_str))
        .collect();
    pairs
        .iter()
        .all(|(part, segment)| part.starts_with(':') || part == segment)
        .then_some(pairs)
}

/// `searchParams.get(name)`: the first value.
fn query(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

/// The account parameter; no profile lists an id longer than an account id may be.
fn account_id(account: &str) -> Result<AccountId, AppError> {
    AccountId::parse(account).map_err(|_| AppError::new("Account not found in this profile.", 404))
}

/// Management responses: JSON, never cached, never sniffed.
fn json<T: Serialize>(status: StatusCode, value: &T) -> Result<Response<Body>, AppError> {
    let body = serde_json::to_vec(value).map_err(AppError::internal)?;
    let mut response = Response::new(Body::full(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

/// `jsonBody`: a JSON content type, at most 2 MB, parsed.
async fn body(request: Request<Incoming>) -> Result<Value, AppError> {
    let json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes().starts_with(b"application/json"));
    if !json {
        return Err(AppError::new("Content-Type must be application/json.", 415));
    }
    let mut incoming = request.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = incoming.frame().await {
        // The client hung up: nothing failed in the app, and no one reads the answer.
        let frame = frame.map_err(|_| AppError::new("", 400))?;
        if let Ok(data) = frame.into_data() {
            if bytes.len() + data.len() > BODY_LIMIT {
                return Err(AppError::new("Payload exceeds 2 MB.", 413));
            }
            bytes.extend_from_slice(&data);
        }
    }
    serde_json::from_str(&String::from_utf8_lossy(&bytes))
        .map_err(|_| AppError::new("Invalid JSON.", 400))
}

/// A 405 that lists the methods the path does serve (RFC 9110), ahead of the error JSON's
/// headers as `server.ts` sent them.
fn not_allowed(methods: &str) -> Response<Body> {
    let mut response =
        Refusal::new(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed.").into_response();
    if let Ok(allow) = HeaderValue::from_str(methods) {
        let rest = std::mem::replace(
            response.headers_mut(),
            HeaderMap::from_iter([(header::ALLOW, allow)]),
        );
        response.headers_mut().extend(rest);
    }
    response
}

fn ok() -> Value {
    json!({ "ok": true })
}

/// What the management API answers from.
pub(crate) struct Api {
    shared: Arc<Shared>,
    store: Store,
    cores: Arc<Cores>,
    agents: Arc<AgentSetup>,
    catalog: Arc<ModelCatalog>,
    prices: Arc<UsagePrices>,
    imports: Arc<Mutex<ImportCatalog>>,
}

impl Api {
    pub(crate) fn new(
        shared: Arc<Shared>,
        store: Store,
        cores: Arc<Cores>,
        agents: Arc<AgentSetup>,
        catalog: Arc<ModelCatalog>,
        prices: Arc<UsagePrices>,
        imports: ImportCatalog,
    ) -> Self {
        Self {
            shared,
            store,
            cores,
            agents,
            catalog,
            prices,
            imports: Arc::new(Mutex::new(imports)),
        }
    }

    pub(crate) async fn answer(
        &self,
        request: Request<Incoming>,
        url: &url::Url,
    ) -> Result<Response<Body>, AppError> {
        let raw = url.path();
        // Split on the raw "/" first, so an encoded slash stays inside its segment.
        let segments: Vec<String> = raw
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(parse::decode_component)
            .collect::<Option<_>>()
            .ok_or_else(|| AppError::new("Malformed request path.", 400))?;
        // Any profile path names a real profile before its route is chosen.
        let profile = match segments.get(1..3) {
            Some([profiles, id]) if profiles == "profiles" => {
                let id = parse::profile_id(id)?;
                let found = id.clone();
                self.store.call(move |db| db.profile(&found)).await?;
                Some(id)
            }
            _ => None,
        };
        let mut allowed = Vec::new();
        for (method, pattern, route) in ROUTES {
            if *route == Route::Plain(PlainRoute::DesktopShow) && self.shared.desktop.is_none() {
                continue;
            }
            let Some(call) = call(*route, pattern, raw, &segments, profile.as_ref()) else {
                continue;
            };
            if *method == request.method().as_str() {
                return match call {
                    Call::Plain(route) => self.plain(route, request, url).await,
                    Call::Profile(route, id) => self.profile(route, &id, request, url).await,
                    Call::Account(route, id, account) => {
                        self.account(route, &id, &account, request).await
                    }
                };
            }
            allowed.push(*method);
        }
        if allowed.is_empty() {
            Err(AppError::new("Not found.", 404))
        } else {
            Ok(not_allowed(&allowed.join(", ")))
        }
    }

    async fn plain(
        &self,
        route: PlainRoute,
        request: Request<Incoming>,
        url: &url::Url,
    ) -> Result<Response<Body>, AppError> {
        let cores = &self.cores;
        match route {
            PlainRoute::State => {
                // A UI polls with `watch` while open; the CLI's reads have no side effects.
                if query(url, "watch").as_deref() == Some("1") {
                    cores.watch().await;
                }
                self.state_reply().await
            }
            PlainRoute::Retention => {
                let days = parse::retention(&body(request).await?)?;
                self.store
                    .call(move |db| db.set_usage_retention_days(days))
                    .await?;
                self.state_reply().await
            }
            PlainRoute::CoreCheck => {
                cores.check_core().await?;
                self.state_reply().await
            }
            PlainRoute::CoreInstall => {
                let version = parse::core_version(&body(request).await?)?;
                cores.install_core(&version).await?;
                self.state_reply().await
            }
            PlainRoute::CoreBundled => {
                cores.use_bundled_core().await?;
                self.state_reply().await
            }
            PlainRoute::ImportSources => {
                let imports = Arc::clone(&self.imports);
                let sources = tokio::task::spawn_blocking(move || locked(&imports).scan())
                    .await
                    .map_err(AppError::internal)?;
                json(StatusCode::OK, &sources)
            }
            PlainRoute::CreateProfile => {
                let name = parse::profile_name(&body(request).await?)?;
                let profile = cores.create_profile(name).await?;
                json(StatusCode::CREATED, &profile)
            }
            PlainRoute::DesktopShow => {
                if let Some(desktop) = &self.shared.desktop {
                    desktop.show_main();
                }
                json(StatusCode::OK, &ok())
            }
        }
    }

    async fn profile(
        &self,
        route: ProfileRoute,
        id: &ProfileId,
        request: Request<Incoming>,
        url: &url::Url,
    ) -> Result<Response<Body>, AppError> {
        let cores = &self.cores;
        match route {
            ProfileRoute::Patch => {
                let patch = parse::profile_patch(&body(request).await?)?;
                cores.patch_profile(id, patch).await?;
                self.state_reply().await
            }
            ProfileRoute::Start => {
                cores.start(id).await?;
                self.state_reply().await
            }
            ProfileRoute::Refresh => {
                cores.refresh(id).await?;
                self.state_reply().await
            }
            ProfileRoute::Key => {
                let found = id.clone();
                let key = self.store.call(move |db| db.client_key(&found)).await?;
                json(
                    StatusCode::OK,
                    &ClientKeyResponse {
                        key: key.expose_secret().to_owned(),
                    },
                )
            }
            ProfileRoute::Models => {
                let metadata = query(url, "metadata").as_deref() == Some("1");
                let models = self.core_models(id).await?;
                if metadata {
                    json(
                        StatusCode::OK,
                        &profile_models(&self.catalog, &self.store, id, &models).await?,
                    )
                } else {
                    json(StatusCode::OK, &models)
                }
            }
            ProfileRoute::Usage => self.usage(id, url).await,
            ProfileRoute::AgentStatus => self.agent_status(id, url).await,
            ProfileRoute::AgentInstall => self.agent_install(id, request).await,
            ProfileRoute::AgentRemove => self.agent_remove(id, request).await,
            ProfileRoute::AgentCheck => {
                parse::empty(&body(request).await?)?;
                self.agent_check(id).await
            }
            ProfileRoute::SignIn => {
                let (provider, reconnect) = parse::oauth_start(&body(request).await?)?;
                let session = cores.begin_sign_in(id, provider, reconnect).await?;
                json(StatusCode::OK, &session)
            }
            ProfileRoute::SignInStatus => {
                let state = parse::state_query(query(url, "state").as_deref())?;
                let status = cores.sign_in_status(id, &state).await?;
                json(StatusCode::OK, &status)
            }
            ProfileRoute::SignInCancel => {
                let state = parse::state_query(query(url, "state").as_deref())?;
                cores.cancel_sign_in(id, &state).await?;
                json(StatusCode::OK, &ok())
            }
            ProfileRoute::SignInConfirm => {
                let state = parse::single(&body(request).await?, "state", 500)?;
                let status = cores.confirm_sign_in(id, &state).await?;
                json(StatusCode::OK, &status)
            }
            ProfileRoute::SignInSession => json(StatusCode::OK, &cores.sign_in_session(id)),
            ProfileRoute::Callback => {
                let url = parse::callback(&body(request).await?)?;
                cores.sign_in_callback(id, &url).await?;
                json(StatusCode::OK, &ok())
            }
            ProfileRoute::Import => {
                let contents = parse::import(&body(request).await?)?;
                cores.import(id, contents).await?;
                self.state_reply().await
            }
            ProfileRoute::ImportSource => {
                let source = parse::import_source(&body(request).await?)?;
                let imports = Arc::clone(&self.imports);
                let contents =
                    tokio::task::spawn_blocking(move || locked(&imports).contents(&source))
                        .await
                        .map_err(AppError::internal)??;
                cores.import(id, contents).await?;
                self.state_reply().await
            }
            ProfileRoute::ApiAccount => {
                let input = parse::api_account(&body(request).await?)?;
                cores.add_api_account(id, input).await?;
                let state = self.state().await?;
                json(StatusCode::CREATED, &state)
            }
        }
    }

    async fn account(
        &self,
        route: AccountRoute,
        id: &ProfileId,
        account: &str,
        request: Request<Incoming>,
    ) -> Result<Response<Body>, AppError> {
        let cores = &self.cores;
        match route {
            AccountRoute::Set => {
                let patch = parse::account_patch(&body(request).await?)?;
                cores.set_account(id, &account_id(account)?, patch).await?;
                self.state_reply().await
            }
            AccountRoute::Remove => {
                cores.remove_account(id, &account_id(account)?).await?;
                self.state_reply().await
            }
            AccountRoute::ResetCredits => {
                let credits = cores.reset_credits(id, &account_id(account)?).await?;
                json(StatusCode::OK, &credits)
            }
            AccountRoute::ConsumeReset => {
                let credit = parse::single(&body(request).await?, "creditId", 500)?;
                let result = cores
                    .consume_reset(id, &account_id(account)?, &credit)
                    .await?;
                json(StatusCode::OK, &result)
            }
        }
    }

    /// The models a profile's core lists; a dormant core starts for them.
    async fn core_models(&self, id: &ProfileId) -> Result<Vec<Model>, AppError> {
        self.cores.woken_models(id).await
    }

    async fn stored(&self, id: &ProfileId) -> Result<Profile, AppError> {
        let id = id.clone();
        Ok(self.store.call(move |db| db.profile(&id)).await?)
    }

    async fn usage(&self, id: &ProfileId, url: &url::Url) -> Result<Response<Body>, AppError> {
        let all = registry().limits.all_history_days;
        let days = parse::days(query(url, "days").as_deref(), all)?.unwrap_or(7);
        let since = if days == all {
            String::new()
        } else {
            iso_string((self.shared.clock)() - i64::from(days) * 86_400_000)
        };
        let history = self
            .prices
            .history(&self.store, id, &since, &self.catalog)
            .await?;
        json(StatusCode::OK, &history)
    }

    async fn agent_status(
        &self,
        id: &ProfileId,
        url: &url::Url,
    ) -> Result<Response<Body>, AppError> {
        let mut selection = serde_json::Map::new();
        selection.insert(
            "agent".to_owned(),
            query(url, "agent").map_or(Value::Null, Value::String),
        );
        if let Some(project) = query(url, "projectDirectory") {
            selection.insert("projectDirectory".to_owned(), Value::String(project));
        }
        let selection = parse::agent_selection(&Value::Object(selection))?;
        let profile = self.stored(id).await?;
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

    async fn agent_install(
        &self,
        id: &ProfileId,
        request: Request<Incoming>,
    ) -> Result<Response<Body>, AppError> {
        let input = parse::agent_setup(&body(request).await?)?;
        let models = self.core_models(id).await?;
        let ids = connection_models(&input, models, |models| async move {
            Ok(profile_models(&self.catalog, &self.store, id, &models).await?)
        })
        .await?;
        let profile = self.stored(id).await?;
        json(
            StatusCode::OK,
            &self.agents.install(&profile, input, ids).await?,
        )
    }

    async fn agent_remove(
        &self,
        id: &ProfileId,
        request: Request<Incoming>,
    ) -> Result<Response<Body>, AppError> {
        let selection = parse::agent_selection(&body(request).await?)?;
        let profile = self.stored(id).await?;
        self.agents
            .remove(
                &profile,
                selection.agent,
                selection.project_directory.as_deref(),
            )
            .await?;
        json(StatusCode::OK, &ok())
    }

    /// Asks the profile's own gateway endpoint for its models, as an agent would.
    async fn agent_check(&self, id: &ProfileId) -> Result<Response<Body>, AppError> {
        let profile = self.stored(id).await?;
        let found = id.clone();
        let key = self.store.call(move |db| db.client_key(&found)).await?;
        // `<endpoint>/models` on this daemon's own port.
        let target = format!("{}/v1/models", profile_path(profile.slug.as_str()));
        let authorization = format!("Bearer {}", key.expose_secret());
        let response = loopback(LoopbackRequest {
            to: Loopback::v4(self.shared.port),
            method: &Method::GET,
            target: &target,
            headers: &[("Authorization", &authorization)],
            body: None,
            timeout: Duration::from_secs(10),
            limit: LOOPBACK_LIMIT,
        })
        .await
        .map_err(AppError::internal)?;
        if !response.ok() {
            return Err(AppError::new(
                "The profile connection failed. Check that this profile is running.",
                502,
            ));
        }
        let catalog: Value = serde_json::from_slice(&response.body).map_err(AppError::internal)?;
        let listed = catalog
            .get("data")
            .and_then(Value::as_array)
            .is_some_and(|data| {
                data.iter()
                    .any(|entry| entry.get("id").is_some_and(Value::is_string))
            });
        if !listed {
            return Err(AppError::new(
                "No models are available. Refresh the profile catalog.",
                400,
            ));
        }
        json(StatusCode::OK, &ok())
    }

    async fn state_reply(&self) -> Result<Response<Body>, AppError> {
        let state = self.state().await?;
        json(StatusCode::OK, &state)
    }

    /// `Application.state()`.
    async fn state(&self) -> Result<AppState, AppError> {
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
        let profiles = self.cores.statuses(|status| {
            stored
                .into_iter()
                .map(|(profile, accounts)| {
                    let status = status(&profile.id);
                    ProfileState {
                        error: status.error.as_deref().map(str::to_owned),
                        runtime: status.runtime,
                        accounts,
                        endpoint: profile_endpoint(self.shared.port, profile.slug.as_str()),
                        profile,
                    }
                })
                .collect()
        });
        Ok(AppState {
            profiles,
            core_version: self.cores.core_version(),
            core: self.cores.core_state(),
            gateway: self.shared.origin.as_str().to_owned(),
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

    /// What `pattern` binds on `raw` for the profile `id`: the route's group and its account.
    fn bound(route: Route, pattern: &str, raw: &str, decoded: &[String]) -> Option<String> {
        let id = ProfileId::parse("6e4156ab-2acf-4d89-9a81-264f3b3da50a").unwrap();
        match call(route, pattern, raw, decoded, Some(&id))? {
            Call::Plain(_) => Some("plain".to_owned()),
            Call::Profile(_, profile) => Some(format!("profile {}", profile.as_str())),
            Call::Account(_, _, account) => Some(format!("account {account}")),
        }
    }

    #[test]
    fn literal_routes_match_the_raw_path_and_parameter_routes_the_segments() {
        let state = Route::Plain(PlainRoute::State);
        let plain = |raw: &str| bound(state, "/state", raw, &segments(raw));
        assert_eq!(plain("/api/state").as_deref(), Some("plain"));
        assert_eq!(plain("/api/state/"), None);
        assert_eq!(plain("/api//state"), None);
        let id = "6e4156ab-2acf-4d89-9a81-264f3b3da50a";
        let doubled = format!("/api//profiles//{id}//key");
        assert_eq!(
            bound(
                Route::Profile(ProfileRoute::Key),
                "/profiles/:profileId/key",
                &doubled,
                &segments(&doubled)
            ),
            Some(format!("profile {id}"))
        );
        let reset = format!("/api/profiles/{id}/accounts/a%2Fb/reset-credits");
        let mut decoded = segments(&reset);
        decoded[4] = "a/b".to_owned();
        assert_eq!(
            bound(
                Route::Account(AccountRoute::ResetCredits),
                "/profiles/:profileId/accounts/:accountId/reset-credits",
                &reset,
                &decoded
            )
            .as_deref(),
            Some("account a/b")
        );
    }
}
