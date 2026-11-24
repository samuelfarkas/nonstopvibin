use std::fs;
use std::io;
use std::os::unix::fs::symlink;

use nsv_files::{EntryKind, own_entry, owner_only_directory, read_own_file, write_atomic};
use nsv_testkit::{TempDir, mode, set_mode};

#[derive(Debug)]
enum ReadError {
    Refused,
    Io(io::Error),
}

impl From<io::Error> for ReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn read(path: &std::path::Path, forbidden: u32, limit: u64) -> Result<Option<String>, ReadError> {
    read_own_file(path, forbidden, limit, || ReadError::Refused)
}

#[test]
fn checked_reads_accept_only_this_users_ordinary_files() {
    let temp = TempDir::new("read-own");
    let path = temp.path().join("server.json");
    assert!(matches!(read(&path, 0o077, 64), Ok(None)));

    fs::write(&path, "{\"pid\":1}").unwrap();
    set_mode(&path, 0o600);
    assert_eq!(
        read(&path, 0o077, 64).unwrap().as_deref(),
        Some("{\"pid\":1}")
    );
    assert!(matches!(read(&path, 0o077, 8), Err(ReadError::Refused)));
    set_mode(&path, 0o644);
    assert!(matches!(read(&path, 0o077, 64), Err(ReadError::Refused)));
    assert!(read(&path, 0o022, 64).unwrap().is_some());

    // A symbolic link is not followed: the open fails (ELOOP), it is not a refusal.
    let link = temp.path().join("link.json");
    symlink(&path, &link).unwrap();
    let looped = Some(rustix::io::Errno::LOOP.raw_os_error());
    assert!(
        matches!(read(&link, 0, 64), Err(ReadError::Io(error)) if error.raw_os_error() == looped)
    );

    // A FIFO neither blocks the open nor passes as a file.
    let fifo = temp.path().join("fifo");
    // mkfifo(1) exists on Linux and macOS; rustix's mknodat does not build for macOS.
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    assert!(matches!(read(&fifo, 0o077, 64), Err(ReadError::Refused)));

    // Invalid UTF-8 reads as U+FFFD, as Node's readFile(path, "utf8") did.
    fs::write(&path, b"a\xffb").unwrap();
    assert_eq!(read(&path, 0, 64).unwrap().as_deref(), Some("a\u{fffd}b"));
}

#[test]
fn atomic_writes_replace_the_file_with_its_mode_and_leave_no_temporary() {
    let temp = TempDir::new("write-atomic");
    let path = temp.path().join("server.json");
    write_atomic(&path, b"first", 0o600).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "first");
    assert_eq!(mode(&path), 0o600);
    fs::remove_file(&path).unwrap();
    write_atomic(&path, b"helper", 0o700).unwrap();
    assert_eq!(mode(&path), 0o700);
    write_atomic(&path, b"second", 0o600).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "second");
    let entries: Vec<_> = fs::read_dir(temp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["server.json"]);
}

#[test]
fn a_replaced_file_is_written_even_when_its_directory_cannot_be_synced() {
    let temp = TempDir::new("write-atomic-unsynced");
    let folder = temp.path().join("folder");
    fs::create_dir(&folder).unwrap();
    let path = folder.join("settings.json");
    fs::write(&path, "old").unwrap();
    // Write and search but no read: the rename works, opening the folder to sync it does not.
    set_mode(&folder, 0o300);
    let written = write_atomic(&path, b"new", 0o600);
    set_mode(&folder, 0o700);
    written.unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "new");
}

#[test]
fn owner_only_directories_are_created_tightened_and_vetted_before_chmod() {
    let temp = TempDir::new("owner-only");
    let nested = temp.path().join("a").join("b");
    owner_only_directory(&nested, || Ok::<(), io::Error>(())).unwrap();
    assert_eq!(mode(&nested), 0o700);
    assert_eq!(mode(&temp.path().join("a")), 0o700);

    set_mode(&nested, 0o755);
    owner_only_directory(&nested, || Ok::<(), io::Error>(())).unwrap();
    assert_eq!(mode(&nested), 0o700);

    set_mode(&nested, 0o755);
    let vetoed = owner_only_directory(&nested, || Err(io::Error::other("vetoed")));
    assert!(vetoed.is_err());
    assert_eq!(mode(&nested), 0o755, "a rejected directory keeps its mode");
}

#[test]
fn own_entries_match_kind_owner_and_forbidden_bits() {
    let temp = TempDir::new("own-entry");
    let file = temp.path().join("file");
    fs::write(&file, "").unwrap();
    set_mode(&file, 0o640);
    let status = fs::symlink_metadata(&file).unwrap();
    assert!(own_entry(&status, EntryKind::File, 0o022));
    assert!(!own_entry(&status, EntryKind::File, 0o077));
    assert!(!own_entry(&status, EntryKind::Directory, 0));
    let link = temp.path().join("link");
    symlink(&file, &link).unwrap();
    assert!(!own_entry(
        &fs::symlink_metadata(&link).unwrap(),
        EntryKind::File,
        0
    ));
    assert!(own_entry(
        &fs::symlink_metadata(temp.path()).unwrap(),
        EntryKind::Directory,
        0
    ));
}
