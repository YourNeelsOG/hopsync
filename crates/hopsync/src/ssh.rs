//! SSH connections (plan.md §6.1, §7; Decision log: russh).
//!
//! `connect` refuses to talk to a host that has no pinned key, offers the server only the host-key
//! algorithms already pinned for it, and accepts only an exact pinned key. Authentication is by an
//! unencrypted ed25519 or ECDSA key file in v1 (RSA is off: see the Decision log).
//!
//! Cipher order: AES-GCM first when the CPU has AES instructions (Phase 0: about 2x ChaCha20 there),
//! ChaCha20-Poly1305 first otherwise, AES-CTR with ETM MACs last for old servers. No CBC, 3DES, SHA-1
//! MACs or SHA-1 key exchange: russh's defaults already exclude them.

use std::borrow::Cow;
use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{ChannelStream, Preferred, cipher, client};

use crate::hostkeys::{HostId, KnownHosts, fingerprint};

#[derive(Debug, Clone)]
pub struct SshTarget {
    pub host: HostId,
    pub user: String,
    pub key: KeySource,
}

/// Where the private key for authentication comes from.
#[derive(Clone)]
pub enum KeySource {
    /// A key file. Must not be readable by group or others, like OpenSSH requires.
    File(PathBuf),
    /// A key already in memory (the daemon's vault), never written to disk in the clear.
    Loaded(Arc<russh::keys::PrivateKey>),
    /// A password, only for installing a generated key; never stored.
    Password(Arc<zeroize::Zeroizing<String>>),
}

impl fmt::Debug for KeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(p) => f.debug_tuple("File").field(p).finish(),
            // Never print key material, even in debug output.
            Self::Loaded(_) => f.write_str("Loaded(<private key>)"),
            Self::Password(_) => f.write_str("Password(<redacted>)"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SshOptions {
    /// TCP connect, key exchange and authentication together.
    pub connect_timeout: Duration,
    /// SSH keepalive interval; the connection is dropped after 3 unanswered keepalives.
    pub keepalive: Duration,
    /// Receive window per channel. Bounds how much a server may send ahead of us, so it trades
    /// memory for throughput on long paths (Phase 0: 2 MiB caps one lane near 10 MB/s at 200 ms).
    pub window: u32,
}

impl Default for SshOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(20),
            keepalive: Duration::from_secs(15),
            window: 8 << 20,
        }
    }
}

#[derive(Debug)]
pub enum SshError {
    /// No pinned key: the user has to run `hopsync hosts trust` first.
    UnknownHost {
        host: String,
    },
    HostKeyMismatch {
        host: String,
        offered: String,
        pinned: Vec<String>,
    },
    /// Only key types this build cannot verify are pinned (for example RSA).
    UnsupportedHostKey {
        host: String,
        pinned: Vec<String>,
    },
    KeyFile {
        path: PathBuf,
        reason: String,
    },
    AuthRejected {
        user: String,
        host: String,
    },
    Timeout {
        host: String,
        after: Duration,
    },
    Connect {
        host: String,
        reason: String,
    },
}

impl fmt::Display for SshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownHost { host } => write!(
                f,
                "{host} has no pinned host key; run `hopsync hosts trust {host}` and check the fingerprint"
            ),
            Self::HostKeyMismatch {
                host,
                offered,
                pinned,
            } => write!(
                f,
                "HOST KEY CHANGED for {host}: offered {offered}, pinned {}. Refusing to connect.",
                pinned.join(", ")
            ),
            Self::UnsupportedHostKey { host, pinned } => write!(
                f,
                "{host} only has pinned host keys of unsupported types ({}); ed25519 and ECDSA are supported",
                pinned.join(", ")
            ),
            Self::KeyFile { path, reason } => write!(f, "key file {}: {reason}", path.display()),
            Self::AuthRejected { user, host } => {
                write!(f, "{host} rejected the key for user {user}")
            }
            Self::Timeout { host, after } => write!(f, "{host}: no connection after {after:?}"),
            Self::Connect { host, reason } => write!(f, "{host}: {reason}"),
        }
    }
}

impl std::error::Error for SshError {}

/// An authenticated SSH connection. Cheap to share; each SFTP session opens its own channel.
pub struct SshConn {
    handle: Arc<client::Handle<PinnedHostKeys>>,
    host: HostId,
}

impl fmt::Debug for SshConn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshConn")
            .field("host", &self.host.to_string())
            .finish_non_exhaustive()
    }
}

impl SshConn {
    pub fn host(&self) -> &HostId {
        &self.host
    }

    /// Opens a session channel running the `sftp` subsystem.
    pub(crate) async fn open_sftp_channel(&self) -> Result<ChannelStream<client::Msg>, SshError> {
        let lost = |e: russh::Error| SshError::Connect {
            host: self.host.to_string(),
            reason: e.to_string(),
        };
        let channel = self.handle.channel_open_session().await.map_err(lost)?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(lost)?;
        Ok(channel.into_stream())
    }

    /// Keeps the connection alive for as long as an SFTP session needs it.
    pub(crate) fn keepalive_handle(&self) -> Arc<dyn Send + Sync> {
        self.handle.clone()
    }

    pub async fn disconnect(self) {
        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;
    }
}

pub async fn connect(
    target: &SshTarget,
    known: &KnownHosts,
    opts: &SshOptions,
) -> Result<SshConn, SshError> {
    let host = target.host.to_string();
    let pins = known.pinned_keys(&target.host);
    if pins.is_empty() {
        return Err(SshError::UnknownHost { host });
    }
    let algorithms: Vec<Algorithm> = known
        .pinned_algorithms(&target.host)
        .into_iter()
        .filter(|a| matches!(a, Algorithm::Ed25519 | Algorithm::Ecdsa { .. }))
        .collect();
    if algorithms.is_empty() {
        let pinned = known
            .pinned_algorithms(&target.host)
            .iter()
            .map(|a| a.to_string())
            .collect();
        return Err(SshError::UnsupportedHostKey { host, pinned });
    }
    // Key files are read before connecting, so a bad key fails without touching the network.
    let key = match &target.key {
        KeySource::File(path) => Ok(Arc::new(load_key(path)?)),
        KeySource::Loaded(key) => Ok(key.clone()),
        KeySource::Password(pw) => Err(pw.clone()),
    };

    let config = client::Config {
        preferred: Preferred {
            key: Cow::Owned(algorithms),
            cipher: Cow::Borrowed(cipher_order()),
            ..Preferred::DEFAULT
        },
        keepalive_interval: Some(opts.keepalive),
        keepalive_max: 3,
        window_size: opts.window,
        ..Default::default()
    };
    let offered = Arc::new(Mutex::new(None));
    let handler = PinnedHostKeys {
        pins: pins.clone(),
        offered: offered.clone(),
    };

    let attempt = async {
        let mut handle = client::connect(
            Arc::new(config),
            (target.host.host(), target.host.port()),
            handler,
        )
        .await
        .map_err(|e| match offered.lock().ok().and_then(|mut o| o.take()) {
            Some(offered) => SshError::HostKeyMismatch {
                host: host.clone(),
                offered,
                pinned: pins.iter().map(fingerprint).collect(),
            },
            None => SshError::Connect {
                host: host.clone(),
                reason: e.to_string(),
            },
        })?;
        let auth = match &key {
            Ok(key) => {
                handle
                    .authenticate_publickey(
                        &target.user,
                        PrivateKeyWithHashAlg::new(key.clone(), None),
                    )
                    .await
            }
            Err(pw) => {
                handle
                    .authenticate_password(&target.user, pw.as_str())
                    .await
            }
        };
        let auth = auth.map_err(|e| SshError::Connect {
            host: host.clone(),
            reason: e.to_string(),
        })?;
        if !auth.success() {
            return Err(SshError::AuthRejected {
                user: target.user.clone(),
                host: host.clone(),
            });
        }
        Ok(handle)
    };
    let handle = tokio::time::timeout(opts.connect_timeout, attempt)
        .await
        .map_err(|_| SshError::Timeout {
            host: host.clone(),
            after: opts.connect_timeout,
        })??;
    Ok(SshConn {
        handle: Arc::new(handle),
        host: target.host.clone(),
    })
}

fn load_key(path: &std::path::Path) -> Result<russh::keys::PrivateKey, SshError> {
    let err = |reason: String| SshError::KeyFile {
        path: path.to_owned(),
        reason,
    };
    let mode = std::fs::metadata(path)
        .map_err(|e| err(e.to_string()))?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        return Err(err(format!(
            "permissions {mode:o} are too open; run chmod 600"
        )));
    }
    // The error text never includes key material, only what went wrong.
    russh::keys::load_secret_key(path, None).map_err(|e| match e {
        russh::keys::Error::KeyIsEncrypted => {
            err("passphrase-protected keys are not supported yet; use an unencrypted key".into())
        }
        other => err(other.to_string()),
    })
}

/// Accepts the server's host key only if it is one of the pins; remembers what was offered otherwise.
struct PinnedHostKeys {
    pins: Vec<PublicKey>,
    offered: Arc<Mutex<Option<String>>>,
}

impl client::Handler for PinnedHostKeys {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let (trusted, offered) = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => (
                self.pins.iter().any(|p| p.key_data() == key.key_data()),
                fingerprint(key),
            ),
            PublicKeyOrCertificate::Certificate(_) => (false, "a host certificate".to_owned()),
        };
        if !trusted && let Ok(mut slot) = self.offered.lock() {
            *slot = Some(offered);
        }
        Ok(trusted)
    }
}

fn cipher_order() -> &'static [cipher::Name] {
    const AES_FIRST: &[cipher::Name] = &[
        cipher::AES_128_GCM,
        cipher::AES_256_GCM,
        cipher::CHACHA20_POLY1305,
        cipher::AES_128_CTR,
        cipher::AES_256_CTR,
    ];
    const CHACHA_FIRST: &[cipher::Name] = &[
        cipher::CHACHA20_POLY1305,
        cipher::AES_128_GCM,
        cipher::AES_256_GCM,
        cipher::AES_128_CTR,
        cipher::AES_256_CTR,
    ];
    if has_aes_instructions() {
        AES_FIRST
    } else {
        CHACHA_FIRST
    }
}

#[cfg(target_arch = "x86_64")]
fn has_aes_instructions() -> bool {
    std::arch::is_x86_feature_detected!("aes") && std::arch::is_x86_feature_detected!("pclmulqdq")
}

#[cfg(target_arch = "aarch64")]
fn has_aes_instructions() -> bool {
    std::arch::is_aarch64_feature_detected!("aes")
        && std::arch::is_aarch64_feature_detected!("pmull")
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn has_aes_instructions() -> bool {
    false
}
