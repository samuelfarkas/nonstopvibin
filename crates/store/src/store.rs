//! The store thread. SQLite runs on one dedicated thread fed by a bounded queue of closures;
//! callers await each closure's reply, so no async thread ever blocks on SQLite.

use std::io;
use std::sync::Arc;
use std::thread;

use nsv_domain::error::AppError;
use nsv_domain::id::InvalidId;
use nsv_files::DataDir;
use tokio::sync::{mpsc, oneshot, watch};

use crate::db::{AuthSnapshot, Db, ProfileFacts, Published};
use crate::vault::{Vault, VaultError};

/// Calls waiting for the store thread before a caller waits to enqueue.
const QUEUE: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// An error the user sees, with its HTTP status.
    #[error(transparent)]
    App(#[from] AppError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Vault(#[from] VaultError),
    #[error("a body could not be encoded as JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Id(#[from] InvalidId),
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A symbolic link, a hard link or another user's entry at the database path.
    #[error("nonstopvibin.sqlite must be an ordinary file you own, without links.")]
    UnsafeDatabase,
    /// The store was closed, or its thread ended.
    #[error("the store has stopped")]
    Closed,
}

type Work = Box<dyn FnOnce(&mut Db) + Send>;

enum Job {
    Run(Work),
    Close(oneshot::Sender<()>),
}

/// A handle to one data directory's store. Clones share the thread; it stops after
/// [`Store::close`] or when the last clone drops.
#[derive(Clone)]
pub struct Store {
    jobs: mpsc::Sender<Job>,
    auth: watch::Receiver<Arc<AuthSnapshot>>,
    facts: watch::Receiver<ProfileFacts>,
    directory: DataDir,
    vault: Vault,
}

/// What the API answers for a store failure: its own error, or a hidden 500.
impl From<StoreError> for AppError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::App(error) => error,
            other => Self::internal(other),
        }
    }
}

impl Store {
    /// Makes `directory` owner-only, opens (or creates) its database with mode 0600 and the
    /// schema, and starts the store thread. A sandbox store passes its parent's vault.
    ///
    /// # Errors
    /// A file-system or database error, or [`StoreError::Closed`] if the thread failed.
    pub async fn open(directory: &DataDir, vault: Vault) -> Result<Self, StoreError> {
        let (opened, opening) = oneshot::channel();
        let (jobs, queue) = mpsc::channel(QUEUE);
        let thread_directory = directory.clone();
        let thread_vault = vault.clone();
        thread::Builder::new()
            .name("nsv-store".to_owned())
            .spawn(move || serve(&thread_directory, thread_vault, opened, queue))?;
        let Published { auth, facts } = opening.await.map_err(|_| StoreError::Closed)??;
        Ok(Self {
            jobs,
            auth,
            facts,
            directory: directory.clone(),
            vault,
        })
    }

    /// Runs `work` on the store thread as one atomic section and returns its result. A profile
    /// or secret write in `work` has published a new [`Store::auth`] snapshot, and a profile or
    /// account write new [`Store::facts`], by the time this returns.
    ///
    /// # Errors
    /// What `work` returns, or [`StoreError::Closed`].
    pub async fn call<T, F>(&self, work: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Db) -> Result<T, StoreError> + Send + 'static,
    {
        let (reply, replied) = oneshot::channel();
        let job = Job::Run(Box::new(move |db: &mut Db| {
            let result = work(db);
            db.publish_if_stale();
            // A caller that stopped waiting needs no answer; the work is done either way.
            let _ = reply.send(result);
        }));
        self.jobs.send(job).await.map_err(|_| StoreError::Closed)?;
        replied.await.map_err(|_| StoreError::Closed)?
    }

    /// The gateway's view of every client key, current before any write is acknowledged.
    #[must_use]
    pub fn auth(&self) -> watch::Receiver<Arc<AuthSnapshot>> {
        self.auth.clone()
    }

    /// The gateway's view of every client key, read again first if another connection, such as
    /// another process or a hand edit, committed since the last call: a key deleted or rotated
    /// there fails closed on the next request, as it did when main read the live rows.
    ///
    /// # Errors
    /// [`StoreError::Closed`].
    pub async fn current_auth(&self) -> Result<Arc<AuthSnapshot>, StoreError> {
        self.call(|_| Ok(())).await?;
        Ok(Arc::clone(&self.auth.borrow()))
    }

    /// Every profile's facts, current before any write is acknowledged.
    #[must_use]
    pub fn facts(&self) -> watch::Receiver<ProfileFacts> {
        self.facts.clone()
    }

    #[must_use]
    pub fn directory(&self) -> &DataDir {
        &self.directory
    }

    #[must_use]
    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    /// Finishes the calls queued before it, closes the database and stops the thread. Later
    /// calls fail with [`StoreError::Closed`]; closing again does nothing.
    pub async fn close(&self) {
        let (done, closed) = oneshot::channel();
        if self.jobs.send(Job::Close(done)).await.is_ok() {
            // An error means the thread already ended, which is what was asked for.
            let _ = closed.await;
        }
    }
}

type Opened = Result<Published, StoreError>;

fn serve(
    directory: &DataDir,
    vault: Vault,
    opened: oneshot::Sender<Opened>,
    mut queue: mpsc::Receiver<Job>,
) {
    let mut db = match Db::open(directory, vault) {
        Ok((db, published)) => {
            if opened.send(Ok(published)).is_err() {
                return;
            }
            db
        }
        Err(error) => {
            // The opener reports the error; if it stopped waiting, nobody is left to tell.
            let _ = opened.send(Err(error));
            return;
        }
    };
    while let Some(job) = queue.blocking_recv() {
        match job {
            Job::Run(work) => work(&mut db),
            Job::Close(done) => {
                drop(db);
                let _ = done.send(());
                return;
            }
        }
    }
}
