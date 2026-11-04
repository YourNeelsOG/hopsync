//! Secret vault (Phase 2 step 3, plan.md §7): seals secrets for the store with ChaCha20-Poly1305
//! under a key that lives in its own file, never in the database.
//!
//! - The key is 32 random bytes in `vault.key`, mode 0600. It is created once and never
//!   overwritten: a new key would orphan every sealed secret. It is written to a temporary file,
//!   synced, then hard-linked into place, so a crash cannot leave a short key under the real name.
//! - A sealed value is `version (1) || nonce (12) || ciphertext || tag (16)` with a random nonce.
//!   The associated data names the table, row id and purpose, so a sealed value copied to another
//!   row or used for another purpose fails to open.
//! - Opened secrets come back in `Zeroizing` buffers that are wiped when dropped.

use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use aws_lc_rs::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use zeroize::Zeroizing;

const KEY_LEN: usize = 32;
const VERSION: u8 = 1;

#[derive(Debug)]
pub enum VaultError {
    Io {
        path: String,
        source: io::Error,
    },
    /// The key file is readable by group or others, or is not a regular file.
    Permissions(String),
    /// The key file does not hold exactly one key.
    BadKey(String),
    /// The key file is gone, but the store holds secrets sealed with it.
    MissingKey(String),
    /// Wrong key, damaged data, or a value moved to another row or purpose.
    Open,
    Random,
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{path}: {source}"),
            Self::Permissions(p) => write!(
                f,
                "{p}: the vault key must be a regular file readable only by its owner (chmod 600)"
            ),
            Self::BadKey(p) => write!(f, "{p}: not a Hopsync vault key"),
            Self::MissingKey(p) => write!(
                f,
                "{p} is missing, but the database holds secrets sealed with it. Restore the key file from a backup; a new key cannot open them"
            ),
            Self::Open => {
                f.write_str("a stored secret could not be opened (wrong vault key or damaged data)")
            }
            Self::Random => f.write_str("the system random number generator failed"),
        }
    }
}

impl std::error::Error for VaultError {}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> VaultError + '_ {
    move |source| VaultError::Io {
        path: path.display().to_string(),
        source,
    }
}

pub struct Vault {
    key: LessSafeKey,
}

impl Vault {
    /// Loads the key at `path`. When it does not exist: creates one if `may_create`, else fails
    /// with `MissingKey` (the caller passes false once the store holds secrets).
    pub fn open_or_create(path: &Path, may_create: bool) -> Result<Self, VaultError> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => Self::load(path),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if !may_create {
                    return Err(VaultError::MissingKey(path.display().to_string()));
                }
                Self::create(path)
            }
            Err(e) => Err(io_err(path)(e)),
        }
    }

    fn load(path: &Path) -> Result<Self, VaultError> {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(io_err(path))?;
        let meta = f.metadata().map_err(io_err(path))?;
        if !meta.is_file() || meta.mode() & 0o077 != 0 {
            return Err(VaultError::Permissions(path.display().to_string()));
        }
        let mut key = Zeroizing::new(Vec::with_capacity(KEY_LEN + 1));
        // One byte more than a key, to notice a longer file.
        (&mut f)
            .take(KEY_LEN as u64 + 1)
            .read_to_end(&mut key)
            .map_err(io_err(path))?;
        if key.len() != KEY_LEN {
            return Err(VaultError::BadKey(path.display().to_string()));
        }
        Self::from_key(&key)
    }

    fn create(path: &Path) -> Result<Self, VaultError> {
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        aws_lc_rs::rand::fill(&mut key[..]).map_err(|_| VaultError::Random)?;
        let tmp = path.with_extension("key.tmp");
        // A leftover from a crash during an earlier create holds no secret anyone used.
        match std::fs::remove_file(&tmp) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(io_err(&tmp)(e)),
            _ => {}
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(io_err(&tmp))?;
        f.write_all(&key[..]).map_err(io_err(&tmp))?;
        f.sync_all().map_err(io_err(&tmp))?;
        // link fails if the key appeared meanwhile: never replace a key.
        let linked = std::fs::hard_link(&tmp, path).map_err(io_err(path));
        let _ = std::fs::remove_file(&tmp);
        linked?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::File::open(dir)
                .and_then(|d| d.sync_all())
                .map_err(io_err(dir))?;
        }
        Self::from_key(&key[..])
    }

    fn from_key(key: &[u8]) -> Result<Self, VaultError> {
        let unbound = UnboundKey::new(&CHACHA20_POLY1305, key).map_err(|_| VaultError::Open)?;
        Ok(Self {
            key: LessSafeKey::new(unbound),
        })
    }

    /// Seals `plain` for row `id` of `table`, for `purpose`.
    pub fn seal(
        &self,
        table: &str,
        id: i64,
        purpose: &str,
        plain: &[u8],
    ) -> Result<Vec<u8>, VaultError> {
        let mut nonce = [0u8; NONCE_LEN];
        aws_lc_rs::rand::fill(&mut nonce).map_err(|_| VaultError::Random)?;
        let mut body = plain.to_vec();
        let aad = associated(table, id, purpose);
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(&aad),
                &mut body,
            )
            .map_err(|_| VaultError::Open)?;
        let mut out = Vec::with_capacity(1 + NONCE_LEN + body.len());
        out.push(VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&body);
        // `body` held only ciphertext after sealing, so nothing secret is left to wipe.
        Ok(out)
    }

    /// Opens what `seal` produced for the same table, row and purpose.
    pub fn open(
        &self,
        table: &str,
        id: i64,
        purpose: &str,
        sealed: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        let (&version, rest) = sealed.split_first().ok_or(VaultError::Open)?;
        if version != VERSION || rest.len() < NONCE_LEN {
            return Err(VaultError::Open);
        }
        let (nonce, body) = rest.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| VaultError::Open)?;
        let mut buf = Zeroizing::new(body.to_vec());
        let aad = associated(table, id, purpose);
        let n = self
            .key
            .open_in_place(nonce, Aad::from(&aad), &mut buf[..])
            .map_err(|_| VaultError::Open)?
            .len();
        buf.truncate(n);
        Ok(buf)
    }
}

fn associated(table: &str, id: i64, purpose: &str) -> Vec<u8> {
    format!("hopsync-vault-v1\0{table}\0{id}\0{purpose}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_sealed_secret_opens_only_for_its_row_and_purpose() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vault = Vault::open_or_create(&dir.path().join("vault.key"), true).expect("create");
        let sealed = vault
            .seal("secrets", 7, "ssh-key", b"hunter2")
            .expect("seal");
        assert!(
            !sealed.windows(7).any(|w| w == b"hunter2"),
            "no plaintext in the sealed bytes"
        );
        assert_eq!(
            &vault.open("secrets", 7, "ssh-key", &sealed).expect("open")[..],
            b"hunter2"
        );
        assert!(matches!(
            vault.open("secrets", 8, "ssh-key", &sealed),
            Err(VaultError::Open)
        ));
        assert!(matches!(
            vault.open("secrets", 7, "password", &sealed),
            Err(VaultError::Open)
        ));
        let mut flipped = sealed.clone();
        *flipped.last_mut().expect("tag") ^= 1;
        assert!(matches!(
            vault.open("secrets", 7, "ssh-key", &flipped),
            Err(VaultError::Open)
        ));
        assert_ne!(
            vault
                .seal("secrets", 7, "ssh-key", b"hunter2")
                .expect("seal"),
            sealed,
            "every seal uses a fresh nonce"
        );
    }

    #[test]
    fn the_key_persists_and_another_key_cannot_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("vault.key");
        let sealed = Vault::open_or_create(&path, true)
            .expect("create")
            .seal("t", 1, "p", b"s")
            .expect("seal");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!dir.path().join("vault.key.tmp").exists());
        let again = Vault::open_or_create(&path, true).expect("load");
        assert_eq!(&again.open("t", 1, "p", &sealed).expect("open")[..], b"s");

        let other = Vault::open_or_create(&dir.path().join("other.key"), true).expect("other");
        assert!(matches!(
            other.open("t", 1, "p", &sealed),
            Err(VaultError::Open)
        ));
    }

    #[test]
    fn a_missing_key_is_not_replaced_when_secrets_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("vault.key");
        assert!(matches!(
            Vault::open_or_create(&path, false),
            Err(VaultError::MissingKey(_))
        ));
        assert!(!path.exists());
    }

    #[test]
    fn a_readable_short_or_linked_key_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("vault.key");
        Vault::open_or_create(&path, true).expect("create");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");
        assert!(matches!(
            Vault::open_or_create(&path, true),
            Err(VaultError::Permissions(_))
        ));

        let short = dir.path().join("short.key");
        std::fs::write(&short, [0u8; 16]).expect("write");
        std::fs::set_permissions(&short, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(matches!(
            Vault::open_or_create(&short, true),
            Err(VaultError::BadKey(_))
        ));

        let link = dir.path().join("link.key");
        std::os::unix::fs::symlink(&short, &link).expect("symlink");
        assert!(
            Vault::open_or_create(&link, true).is_err(),
            "symlinks are not followed"
        );
    }
}
