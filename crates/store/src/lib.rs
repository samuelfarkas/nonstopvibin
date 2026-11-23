//! The data directory: layout, owner-only file checks, atomic writes, the vault and the SQLite store.
#![forbid(unsafe_code)]

mod db;
pub mod files;
pub mod layout;
mod lock;
mod store;
mod vault;

pub use db::{AuthEntry, AuthSnapshot, Db, ProfileNames, UsageCursor, UsagePage, UsageRange};
pub use lock::{LockError, ServerLock};
pub use store::{Store, StoreError};
pub use vault::{Vault, VaultError};
