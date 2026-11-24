//! The gateway's view of the daemon: the store's key snapshot, the scheduler's wake and the
//! agents' curated catalogs. It holds no management key.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use nsv_accounts::catalog::{ModelCatalog, profile_models};
use nsv_accounts::cores::Cores;
use nsv_agents::{CatalogAnswer, curated, gateway_catalog};
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::registry::Agent;
use nsv_domain::secret::KeyDigest;
use nsv_domain::sync::locked;
use nsv_gateway::{Activity, Catalog, Host, ProfileAuth, Refusal};
use nsv_store::{AuthSnapshot, Store};
use tokio::sync::watch;

use crate::failure;
use crate::shared::Shared;

/// One auth snapshot as the gateway takes it, shared per request instead of copied.
struct GatewayProfiles {
    source: Arc<AuthSnapshot>,
    entries: HashMap<KeyDigest, Arc<ProfileAuth>>,
}

impl GatewayProfiles {
    /// A store that could not read the keys has no entries: every key fails closed.
    fn of(source: Arc<AuthSnapshot>) -> Self {
        let entries = source
            .entries()
            .into_iter()
            .flatten()
            .filter_map(|(digest, entry)| {
                let auth = ProfileAuth::new(
                    entry.profile.clone(),
                    entry.slug.clone(),
                    entry.client_key.clone(),
                    &entry.core_key,
                )?;
                Some((*digest, Arc::new(auth)))
            })
            .collect();
        Self { source, entries }
    }
}

pub(crate) struct GatewayHost {
    shared: Arc<Shared>,
    auth: watch::Receiver<Arc<AuthSnapshot>>,
    /// The gateway's entries for the store's latest snapshot, built when it changes.
    profiles: Mutex<Option<GatewayProfiles>>,
    cores: Arc<Cores>,
    catalog: Arc<ModelCatalog>,
    store: Store,
}

impl GatewayHost {
    pub(crate) fn new(
        shared: Arc<Shared>,
        store: Store,
        cores: Arc<Cores>,
        catalog: Arc<ModelCatalog>,
    ) -> Self {
        Self {
            shared,
            auth: store.auth(),
            profiles: Mutex::default(),
            cores,
            catalog,
            store,
        }
    }

    async fn curated(&self, profile: &ProfileId, agent: Agent) -> Result<Catalog, AppError> {
        let models = self.cores.models(profile).await?;
        let offered = curated(agent, models, false, |models| async move {
            Ok(profile_models(&self.catalog, &self.store, profile, &models).await?)
        })
        .await?;
        Ok(match gateway_catalog(agent, &offered)? {
            CatalogAnswer::List(body) => Catalog::List(body.into()),
            CatalogAnswer::Entries(body) => Catalog::Entries(body.into()),
        })
    }
}

impl Host for GatewayHost {
    fn profile(&self, digest: &KeyDigest) -> Option<Arc<ProfileAuth>> {
        // The current snapshot on every request, so a deleted or rotated key fails at once.
        let snapshot = Arc::clone(&self.auth.borrow());
        let mut profiles = locked(&self.profiles);
        if !profiles
            .as_ref()
            .is_some_and(|profiles| Arc::ptr_eq(&profiles.source, &snapshot))
        {
            *profiles = Some(GatewayProfiles::of(snapshot));
        }
        profiles.as_ref()?.entries.get(digest).cloned()
    }

    async fn wake(&self, profile: &ProfileId) -> Result<SocketAddr, Refusal> {
        self.cores
            .wake(profile)
            .await
            .map_err(|error| failure::refusal(error, &self.shared.errors))?;
        let port = self
            .cores
            .port(profile)
            .map_err(|error| failure::refusal(error, &self.shared.errors))?;
        Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
    }

    async fn catalog(&self, profile: &ProfileId, agent: Agent) -> Result<Catalog, Refusal> {
        self.curated(profile, agent)
            .await
            .map_err(|error| failure::refusal(error, &self.shared.errors))
    }

    fn activity(&self, profile: &ProfileId, activity: Activity) {
        let open = match activity {
            Activity::Opened { .. } => true,
            Activity::Closed { last } => !last,
        };
        self.cores.activity(profile, open);
    }
}
