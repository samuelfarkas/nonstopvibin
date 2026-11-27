//! Throwaway stores, each with one profile and a pool of its own, under `oauth-pending/`: a
//! core update's check. The directory keeps the name it had when Claude sign-ins awaiting
//! review used it too. Startup reaps and deletes what a crash left.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nsv_core::pool::{Backend, Launch, NoHooks, Pool};
use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_files::{DataDir, owner_only_directory, unique_directory};
use nsv_store::Store;

use crate::task::blocking;

/// Launches a data directory's cores: `Processes::new(directory)` in the daemon, so a throwaway
/// store's cores are recorded in its own ledger.
pub type Backends = Arc<dyn Fn(&DataDir) -> Arc<dyn Backend> + Send + Sync>;

pub(crate) struct Sandbox {
    pub(crate) directory: DataDir,
    pub(crate) store: Store,
    pub(crate) pool: Pool,
    pub(crate) profile: ProfileId,
}

impl Sandbox {
    /// A new store `<prefix>…` beside `parent`'s other sandboxes, sealed with its vault and
    /// holding one profile `name`, and a pool that starts `binary`. Nothing is left behind
    /// when it fails.
    ///
    /// # Errors
    /// A file-system or store error.
    pub(crate) async fn open(
        parent: &Store,
        prefix: String,
        name: &'static str,
        binary: PathBuf,
        backends: &Backends,
        ready: Duration,
    ) -> Result<Self, AppError> {
        let pending = parent.directory().oauth_pending();
        let directory = DataDir::new(
            blocking(move || {
                owner_only_directory(&pending, || Ok::<(), io::Error>(()))
                    .and_then(|()| unique_directory(&pending, &prefix))
                    .map_err(AppError::internal)
            })
            .await?,
        );
        let opened = async {
            let store = Store::open(&directory, parent.vault().clone()).await?;
            match store.call(move |db| Ok(db.create_profile(name)?.id)).await {
                Ok(profile) => Ok((store, profile)),
                Err(error) => {
                    store.close().await;
                    Err(AppError::from(error))
                }
            }
        }
        .await;
        let (store, profile) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                remove_tree(directory.path().to_owned()).await?;
                return Err(error);
            }
        };
        let pool = Pool::new(
            store.clone(),
            Launch {
                binary,
                backend: backends(&directory),
                ready,
                daemon: None,
            },
            Arc::new(NoHooks),
        );
        Ok(Self {
            directory,
            store,
            pool,
            profile,
        })
    }

    /// Stops its core, closes its store and deletes it. A step that fails leaves the rest for
    /// the next try.
    ///
    /// # Errors
    /// The step's.
    pub(crate) async fn discard(&self) -> Result<(), AppError> {
        self.pool.shutdown().await?;
        self.store.close().await;
        remove_tree(self.directory.path().to_owned()).await
    }
}

/// At startup, before any core runs: core update checks running when the app last stopped
/// never finish, and older builds' pending Claude sign-ins never resume. Their cores are
/// reaped and their stores deleted.
///
/// # Errors
/// A ledger or file-system error.
pub(crate) async fn discard_all(data: &DataDir, backends: &Backends) -> Result<(), AppError> {
    let pending = data.oauth_pending();
    let listed = pending.clone();
    let entries = blocking(move || match std::fs::read_dir(&listed) {
        Ok(listing) => listing
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<PathBuf>>>()
            .map_err(AppError::internal),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(AppError::internal(error)),
    })
    .await?;
    for entry in entries {
        backends(&DataDir::new(entry))
            .reap_orphans()
            .await
            .map_err(AppError::internal)?;
    }
    remove_tree(pending).await
}

/// Deletes a directory tree; a missing one is no error.
pub(crate) async fn remove_tree(directory: PathBuf) -> Result<(), AppError> {
    blocking(move || match std::fs::remove_dir_all(&directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other.map_err(AppError::internal),
    })
    .await
}
