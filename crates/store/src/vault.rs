//! The vault: secrets at rest under an owner-only local key (`vault.key`, 32 raw bytes). A stored
//! value is standard base64 of `nonce(12) ‖ tag(16) ‖ ciphertext`, AES-256-GCM without
//! associated data, exactly as the TypeScript `fileKeyCodec` wrote it.

use std::fmt;
use std::fs::{File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

use aes_gcm::aead::array::Array;
use aes_gcm::{AeadInOut, Aes256Gcm, KeyInit};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use nsv_domain::random;
use nsv_files::{DataDir, EntryKind, own_entry};
use rustix::fs::{Mode, OFlags};
use zeroize::Zeroizing;

const NONCE: usize = 12;
const TAG: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error(
        "vault.key is missing for an existing database. Restore its matching key from your backup."
    )]
    MissingKey,
    #[error("The local vault key is invalid. Restore vault.key from your backup.")]
    InvalidKey,
    /// A symbolic link, a FIFO or another user's file at `vault.key`.
    #[error(
        "vault.key must be an ordinary file you own, not a link. Restore vault.key from your backup."
    )]
    UnsafeKey,
    /// A stored value that is not base64, is too short, fails authentication or is not UTF-8.
    #[error("Unsupported state or unable to authenticate data")]
    Undecryptable,
    #[error("the vault could not encrypt a value")]
    Encrypt,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// The key of one data directory. Cheap to clone; sandbox stores share their parent's. The
/// cipher's key schedule is zeroized when the last clone drops.
#[derive(Clone)]
pub struct Vault(Arc<Aes256Gcm>);

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Vault(<redacted>)")
    }
}

impl Vault {
    /// Opens `vault.key` in `directory`. A missing key is created (0600, never replacing a file)
    /// only while no database exists: a new key cannot decrypt an existing database. The key
    /// must be a regular file owned by this user, never followed through a symbolic link; its
    /// mode is set to 0600 on every open. Blocks on the file system.
    ///
    /// # Errors
    /// [`VaultError::MissingKey`] for a database without its key, [`VaultError::UnsafeKey`] for
    /// a link, a non-regular file or another user's file, [`VaultError::InvalidKey`] for a key
    /// that is not 32 bytes, or a file-system error.
    pub fn open(directory: &DataDir) -> Result<Self, VaultError> {
        let path = directory.vault_key();
        let mut file = match open_key(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if directory.database().exists() {
                    return Err(VaultError::MissingKey);
                }
                let key = Zeroizing::new(random::bytes::<32>()?);
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?
                    .write_all(key.as_slice())?;
                open_key(&path)?
            }
            // A symbolic link fails with ELOOP under O_NOFOLLOW.
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) => {
                return Err(VaultError::UnsafeKey);
            }
            opened => opened?,
        };
        let status = file.metadata()?;
        if !own_entry(&status, EntryKind::File, 0) {
            return Err(VaultError::UnsafeKey);
        }
        if status.len() != 32 {
            return Err(VaultError::InvalidKey);
        }
        file.set_permissions(Permissions::from_mode(0o600))?;
        let mut key = Zeroizing::new([0_u8; 32]);
        file.read_exact(key.as_mut_slice())?;
        let cipher =
            Aes256Gcm::new_from_slice(key.as_slice()).map_err(|_| VaultError::InvalidKey)?;
        Ok(Self(Arc::new(cipher)))
    }

    pub(crate) fn encrypt(&self, plaintext: &str) -> Result<String, VaultError> {
        let nonce = random::bytes::<NONCE>()?;
        let mut sealed = Vec::with_capacity(NONCE + TAG + plaintext.len());
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&[0; TAG]);
        sealed.extend_from_slice(plaintext.as_bytes());
        let (head, body) = sealed.split_at_mut(NONCE + TAG);
        let tag = self
            .0
            .encrypt_inout_detached(&Array::from(nonce), &[], body.into())
            .map_err(|_| VaultError::Encrypt)?;
        head.get_mut(NONCE..)
            .ok_or(VaultError::Encrypt)?
            .copy_from_slice(&tag);
        Ok(STANDARD.encode(&sealed))
    }

    pub(crate) fn decrypt(&self, value: &str) -> Result<String, VaultError> {
        let mut sealed = Zeroizing::new(
            STANDARD
                .decode(value)
                .map_err(|_| VaultError::Undecryptable)?,
        );
        if sealed.len() < NONCE + TAG {
            return Err(VaultError::Undecryptable);
        }
        let (head, body) = sealed.split_at_mut(NONCE + TAG);
        let (nonce, tag) = head.split_at(NONCE);
        let nonce = Array::try_from(nonce).map_err(|_| VaultError::Undecryptable)?;
        let tag = Array::try_from(tag).map_err(|_| VaultError::Undecryptable)?;
        self.0
            .decrypt_inout_detached(&nonce, &[], body.into(), &tag)
            .map_err(|_| VaultError::Undecryptable)?;
        String::from_utf8(body.to_vec()).map_err(|_| VaultError::Undecryptable)
    }
}

/// Opens the key without following a symbolic link or blocking on a FIFO.
fn open_key(path: &Path) -> io::Result<File> {
    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(File::from(descriptor))
}
