//! The seam for what another branch ports: sign-in, credential import and edits, reset credits,
//! API-account creation, quota refresh and core updates. The server parses and validates each of
//! their routes, then hands the typed input to [`Features`]; the hooks put that work in the
//! daemon's startup, scheduling and shutdown where `cores.ts` put it.
//!
//! [`Unwired`] stands in until those libraries merge: every action answers 501, and every hook
//! does nothing, which is what a build without these features does.

use std::path::PathBuf;
use std::sync::Arc;

use nsv_accounts::accounts::Accounts;
use nsv_core::pool::{BoxFuture, Pool};
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::registry::OAuthProvider;
use nsv_domain::wire::{
    AccountPatch, ApiAccountInput, CorePhase, OAuthSession, ResetCredits, ResetResult,
};
use nsv_store::Store;
use serde_json::{Map, Value};

use crate::daemon::Shared;

/// What the daemon gives the features when it assembles them, before anything starts.
pub struct FeatureContext<F> {
    pub store: Store,
    pub pool: Pool,
    pub accounts: Arc<Accounts>,
    pub directory: PathBuf,
    /// The outside requests: models.dev, provider APIs, the core feed and downloads.
    pub fetch: F,
    /// `Application.report` and the sign-in event the desktop shell shows.
    pub host: FeatureHost,
}

/// The daemon as the features reach it.
#[derive(Clone)]
pub struct FeatureHost(pub(crate) Arc<Shared>);

impl FeatureHost {
    /// Adds a message to the error list the interface shows.
    pub fn report(&self, message: String) {
        self.0.errors.report(message);
    }

    /// A provider's browser sign-in callback was accepted: the desktop brings its main window
    /// forward.
    pub fn sign_in_accepted(&self) {
        self.0.sign_in_accepted();
    }
}

/// Sign-in, credentials, resets, quota refresh and core updates. Each method answers one route
/// after the server validated it, or runs at one point of the daemon's life.
pub trait Features: Send + Sync + 'static {
    /// At launch, once orphaned cores are reaped and before any profile starts: pending
    /// sign-ins of an earlier run are dropped.
    fn restore(&self) -> BoxFuture<'_, Result<(), AppError>>;

    /// Once scheduling began: an installed build checks the core feed in the background.
    fn launched(&self);

    /// Shutdown, after the pool refuses new work and before the scheduler settles and stops
    /// the cores: a core download ends, sign-in actions and credential changes finish, and the
    /// pending sign-in is dropped.
    fn shutdown(&self) -> BoxFuture<'_, ()>;

    /// A sign-in is pending in the profile's core, so it must not stop for being idle.
    fn signing_in(&self, profile: &ProfileId) -> bool;

    /// The scheduler's periodic quota refresh of one running profile.
    fn refresh_quotas(
        &self,
        pool: Pool,
        profile: ProfileId,
    ) -> BoxFuture<'static, Result<(), AppError>>;

    /// `POST /profiles/:profileId/refresh` after its start: every enabled account whose quota
    /// can be read now (API-key accounts always, signed-in ones while the core is usable).
    fn refresh_accounts(&self, profile: ProfileId) -> BoxFuture<'_, Result<(), AppError>>;

    /// The newer reviewed core the last check found, and the update in progress.
    fn core_progress(&self) -> (Option<String>, Option<CorePhase>);

    /// `POST /core/check`.
    fn check_core(&self) -> BoxFuture<'_, Result<(), AppError>>;

    /// `POST /core/install`: the version profiles start from afterwards.
    fn install_core(&self, version: String) -> BoxFuture<'_, Result<String, AppError>>;

    /// `DELETE /core/install`: the version profiles start from afterwards.
    fn use_bundled_core(&self) -> BoxFuture<'_, Result<String, AppError>>;

    /// `POST /profiles/:profileId/oauth`.
    fn begin_sign_in(
        &self,
        profile: ProfileId,
        provider: OAuthProvider,
        reconnect: Option<String>,
    ) -> BoxFuture<'_, Result<OAuthSession, AppError>>;

    /// `GET /profiles/:profileId/oauth?state=`.
    fn sign_in_status(
        &self,
        profile: ProfileId,
        state: String,
    ) -> BoxFuture<'_, Result<OAuthSession, AppError>>;

    /// `DELETE /profiles/:profileId/oauth?state=`.
    fn cancel_sign_in(
        &self,
        profile: ProfileId,
        state: String,
    ) -> BoxFuture<'_, Result<(), AppError>>;

    /// `POST /profiles/:profileId/oauth-confirm`.
    fn confirm_sign_in(
        &self,
        profile: ProfileId,
        state: String,
    ) -> BoxFuture<'_, Result<OAuthSession, AppError>>;

    /// `GET /profiles/:profileId/oauth-session`: the pending sign-in, if it is this profile's.
    fn sign_in_session(&self, profile: &ProfileId) -> Option<OAuthSession>;

    /// `POST /profiles/:profileId/callback`.
    fn callback(&self, profile: ProfileId, url: String) -> BoxFuture<'_, Result<(), AppError>>;

    /// `POST /profiles/:profileId/import` and `import-source`: credential files by name.
    fn import(
        &self,
        profile: ProfileId,
        contents: Map<String, Value>,
    ) -> BoxFuture<'_, Result<(), AppError>>;

    /// `PATCH /profiles/:profileId/accounts/:accountId`.
    fn set_account(
        &self,
        profile: ProfileId,
        account: String,
        patch: AccountPatch,
    ) -> BoxFuture<'_, Result<(), AppError>>;

    /// `DELETE /profiles/:profileId/accounts/:accountId`.
    fn remove_account(
        &self,
        profile: ProfileId,
        account: String,
    ) -> BoxFuture<'_, Result<(), AppError>>;

    /// `GET /profiles/:profileId/accounts/:accountId/reset-credits`.
    fn reset_credits(
        &self,
        profile: ProfileId,
        account: String,
    ) -> BoxFuture<'_, Result<ResetCredits, AppError>>;

    /// `POST /profiles/:profileId/accounts/:accountId/reset-credits`.
    fn consume_reset(
        &self,
        profile: ProfileId,
        account: String,
        credit: String,
    ) -> BoxFuture<'_, Result<ResetResult, AppError>>;

    /// `POST /profiles/:profileId/api-account`.
    fn add_api_account(
        &self,
        profile: ProfileId,
        input: ApiAccountInput,
    ) -> BoxFuture<'_, Result<(), AppError>>;
}

/// The features before their libraries are wired in.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unwired;

const UNWIRED: &str = "This build does not include this feature yet.";

fn unwired<T: Send + 'static>() -> BoxFuture<'static, Result<T, AppError>> {
    Box::pin(async { Err(AppError::new(UNWIRED, 501)) })
}

impl Features for Unwired {
    fn restore(&self) -> BoxFuture<'_, Result<(), AppError>> {
        Box::pin(async { Ok(()) })
    }

    fn launched(&self) {}

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn signing_in(&self, _: &ProfileId) -> bool {
        false
    }

    fn refresh_quotas(&self, _: Pool, _: ProfileId) -> BoxFuture<'static, Result<(), AppError>> {
        Box::pin(async { Ok(()) })
    }

    fn refresh_accounts(&self, _: ProfileId) -> BoxFuture<'_, Result<(), AppError>> {
        Box::pin(async { Ok(()) })
    }

    fn core_progress(&self) -> (Option<String>, Option<CorePhase>) {
        (None, None)
    }

    fn check_core(&self) -> BoxFuture<'_, Result<(), AppError>> {
        unwired()
    }

    fn install_core(&self, _: String) -> BoxFuture<'_, Result<String, AppError>> {
        unwired()
    }

    fn use_bundled_core(&self) -> BoxFuture<'_, Result<String, AppError>> {
        unwired()
    }

    fn begin_sign_in(
        &self,
        _: ProfileId,
        _: OAuthProvider,
        _: Option<String>,
    ) -> BoxFuture<'_, Result<OAuthSession, AppError>> {
        unwired()
    }

    fn sign_in_status(
        &self,
        _: ProfileId,
        _: String,
    ) -> BoxFuture<'_, Result<OAuthSession, AppError>> {
        unwired()
    }

    fn cancel_sign_in(&self, _: ProfileId, _: String) -> BoxFuture<'_, Result<(), AppError>> {
        unwired()
    }

    fn confirm_sign_in(
        &self,
        _: ProfileId,
        _: String,
    ) -> BoxFuture<'_, Result<OAuthSession, AppError>> {
        unwired()
    }

    /// No sign-in can be pending: the interface polls this on every profile page.
    fn sign_in_session(&self, _: &ProfileId) -> Option<OAuthSession> {
        None
    }

    fn callback(&self, _: ProfileId, _: String) -> BoxFuture<'_, Result<(), AppError>> {
        unwired()
    }

    fn import(&self, _: ProfileId, _: Map<String, Value>) -> BoxFuture<'_, Result<(), AppError>> {
        unwired()
    }

    fn set_account(
        &self,
        _: ProfileId,
        _: String,
        _: AccountPatch,
    ) -> BoxFuture<'_, Result<(), AppError>> {
        unwired()
    }

    fn remove_account(&self, _: ProfileId, _: String) -> BoxFuture<'_, Result<(), AppError>> {
        unwired()
    }

    fn reset_credits(
        &self,
        _: ProfileId,
        _: String,
    ) -> BoxFuture<'_, Result<ResetCredits, AppError>> {
        unwired()
    }

    fn consume_reset(
        &self,
        _: ProfileId,
        _: String,
        _: String,
    ) -> BoxFuture<'_, Result<ResetResult, AppError>> {
        unwired()
    }

    fn add_api_account(
        &self,
        _: ProfileId,
        _: ApiAccountInput,
    ) -> BoxFuture<'_, Result<(), AppError>> {
        unwired()
    }
}
