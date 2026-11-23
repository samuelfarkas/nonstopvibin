//! The data directory: where it is, the names inside it, its single-instance lock, and the
//! owner-only checks and atomic writes every crate uses for files the user must own.
#![forbid(unsafe_code)]

mod data_dir;
mod files;
mod lock;

pub use data_dir::DataDir;
pub use files::{
    EntryKind, own_entry, owner_only_directory, read_own_file, unique_directory, write_atomic,
    write_atomic_unsynced,
};
pub use lock::{LockError, ServerLock};
