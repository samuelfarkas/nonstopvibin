//! Helpers shared by the store's integration tests.

use std::fs::{self, Permissions};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use nsv_domain::secret::KeyDigest;
use nsv_store::files::owner_only_directory;
use nsv_store::{AuthEntry, AuthSnapshot, Store, StoreError, Vault};

/// The entry whose live client key is `presented`, looked up by digest and compared in constant
/// time as the gateway does; a snapshot the store could not read refuses every key.
pub(crate) fn owner<'a>(
    snapshot: &'a AuthSnapshot,
    presented: &str,
) -> Result<Option<&'a AuthEntry>, &'a StoreError> {
    Ok(snapshot
        .entries()?
        .get(&KeyDigest::of(presented))
        .filter(|entry| entry.client_key.matches(presented)))
}

/// Opens a data directory the way the server does: owner-only directory, vault, store.
pub(crate) async fn open(directory: &Path) -> Store {
    owner_only_directory(directory, || Ok::<(), std::io::Error>(())).unwrap();
    Store::open(directory, Vault::open(directory).unwrap())
        .await
        .unwrap()
}

/// A second connection to the store's database, as another process would have.
pub(crate) fn raw(directory: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(directory.join("nonstopvibin.sqlite")).unwrap()
}

pub(crate) fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o777
}

pub(crate) fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
}
