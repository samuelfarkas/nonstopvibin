use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};

use nsv_store::layout::server_lock_path;
use nsv_store::{LockError, ServerLock};
use nsv_testkit::TempDir;

use crate::common::{mode, set_mode};

#[test]
fn one_owner_holds_a_directory_until_it_releases_and_the_inode_survives() {
    let temp = TempDir::new("lock-owner");
    let directory = temp.path().join("data");
    fs::create_dir(&directory).unwrap();
    set_mode(&directory, 0o755);

    let lock = ServerLock::acquire(&directory).unwrap();
    let path = server_lock_path(&directory);
    assert_eq!(mode(&directory), 0o700);
    assert_eq!(mode(&path), 0o600);
    let inode = fs::metadata(&path).unwrap().ino();

    let canonical = fs::canonicalize(&directory).unwrap();
    match ServerLock::acquire(&directory) {
        Err(error @ LockError::Occupied(_)) => assert_eq!(
            error.to_string(),
            format!(
                "NonstopVibin is already starting, running or stopping for {}. Run `nonstopvibin url` for its link.",
                canonical.display()
            )
        ),
        other => panic!("expected Occupied, got {other:?}"),
    }

    lock.release();
    let successor = ServerLock::acquire(&directory).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    drop(successor);
    assert!(path.exists(), "the lock file is never unlinked");
}

#[test]
fn another_process_writer_occupies_the_directory_but_a_reader_does_not() {
    let temp = TempDir::new("lock-contention");
    let directory = temp.path();
    let path = server_lock_path(directory);
    fs::write(&path, "").unwrap();
    set_mode(&path, 0o600);

    // A competing SHARED reader must not stop the single RESERVED writer.
    let reader = rusqlite::Connection::open(&path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT name FROM sqlite_master")
        .unwrap();
    let lock = ServerLock::acquire(directory).unwrap();
    drop(lock);
    drop(reader);

    let writer = rusqlite::Connection::open(&path).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(matches!(
        ServerLock::acquire(directory),
        Err(LockError::Occupied(_))
    ));
    drop(writer);
    ServerLock::acquire(directory).unwrap();
}

#[test]
fn unsafe_lock_files_and_directories_are_refused_unchanged() {
    let temp = TempDir::new("lock-unsafe");
    let directory = temp.path().join("data");
    ServerLock::acquire(&directory).unwrap().release();
    let path = server_lock_path(&directory);

    set_mode(&path, 0o644);
    let refused = ServerLock::acquire(&directory).unwrap_err();
    assert!(matches!(refused, LockError::UnsafeLockFile));
    assert_eq!(
        refused.to_string(),
        "server.lock.sqlite must be an owner-only ordinary file you own, without links."
    );
    assert_eq!(mode(&path), 0o644, "permissions are not silently changed");

    fs::remove_file(&path).unwrap();
    symlink(directory.join("unexpected"), &path).unwrap();
    assert!(matches!(
        ServerLock::acquire(&directory),
        Err(LockError::UnsafeLockFile)
    ));
    assert!(fs::symlink_metadata(&path).unwrap().is_symlink());

    let linked = temp.path().join("linked");
    symlink(&directory, &linked).unwrap();
    let refused = ServerLock::acquire(&linked).unwrap_err();
    assert!(matches!(refused, LockError::UnsafeDirectory));
    assert_eq!(
        refused.to_string(),
        "The server data directory must be owner-only, owned by you, and not a symbolic link."
    );
}
