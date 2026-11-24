//! The vault and the SQLite store of a data directory.
#![forbid(unsafe_code)]

mod db;
mod store;
mod vault;

pub use db::{
    AuthEntry, AuthSnapshot, Db, ProfileFact, ProfileFacts, UsageCursor, UsagePage, UsageRange,
};
pub use store::{Store, StoreError};
pub use vault::{Vault, VaultError};
