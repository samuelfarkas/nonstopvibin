//! How the account features run work: in a task that outlives a caller who stops waiting, on
//! the blocking pool, or at most once per key at a time; and how they report a failure.

use std::collections::HashSet;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use nsv_domain::error::AppError;
use nsv_domain::sync::locked;

/// Adds a message to the app's error list.
pub type Report = Arc<dyn Fn(String) + Send + Sync>;

/// The error-list entry for a failure of the profile named `name`.
pub(crate) fn about(name: &str, message: &str) -> String {
    format!("{name}: {message}")
}

/// Runs `work` in a task of its own once `turn` is held, so it ends even if its caller stops
/// waiting; the turn ends with it.
pub(crate) async fn detached<T, G>(
    turn: G,
    work: impl Future<Output = Result<T, AppError>> + Send + 'static,
) -> Result<T, AppError>
where
    T: Send + 'static,
    G: Send + 'static,
{
    tokio::spawn(async move {
        let result = work.await;
        drop(turn);
        result
    })
    .await
    .map_err(AppError::internal)?
}

/// Runs file-system or process work on the blocking pool.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, AppError> + Send + 'static,
) -> Result<T, AppError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(AppError::internal)?
}

/// The keys with work in flight, one at a time per key.
pub(crate) struct Exclusive<K>(Mutex<HashSet<K>>);

/// A key's turn; it ends when this drops, whether its work finished or was dropped.
pub(crate) struct Turn<'a, K: Eq + Hash> {
    keys: &'a Exclusive<K>,
    key: K,
}

impl<K: Eq + Hash + Clone> Exclusive<K> {
    pub(crate) fn new() -> Self {
        Self(Mutex::new(HashSet::new()))
    }

    /// `key`'s turn, or `None` while work for it is in flight.
    pub(crate) fn begin(&self, key: K) -> Option<Turn<'_, K>> {
        let free = locked(&self.0).insert(key.clone());
        // Built only when free: dropping a turn gives its key back.
        free.then(|| Turn { keys: self, key })
    }
}

impl<K: Eq + Hash> Drop for Turn<'_, K> {
    fn drop(&mut self) {
        locked(&self.keys.0).remove(&self.key);
    }
}
