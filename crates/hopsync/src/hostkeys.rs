//! SSH host-key pinning (plan.md §7, CLAUDE.md §3).
//!
//! Hopsync keeps its own known_hosts file, separate from `~/.ssh`, in OpenSSH format so it stays
//! readable. A host key is either pinned (`Trusted`), never seen (`Unknown`: refuse until the user
//! confirms the fingerprint), or different from the pin (`Mismatch`: hard failure, never auto-fixed).
//!
//! Pins are per key algorithm. The SSH layer restricts negotiation to the algorithms already pinned for
//! a host, so a known host offering any unpinned key is treated as a mismatch, not as a new host.
//!
//! Only plain entries are supported: no hashed hosts, wildcards or `@cert-authority`/`@revoked`
//! markers. Hopsync never writes those, so finding one means someone else edited the file, and loading
//! fails with the line number rather than silently ignoring the line.

use std::fmt;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use russh::keys::{Algorithm, HashAlg, PublicKey, PublicKeyOrCertificate};

/// A host as SSH sees it: lowercased name or IP literal, plus port.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostId {
    host: String,
    port: u16,
}

impl HostId {
    /// Accepts DNS names (ASCII; IDN must be punycode), IPv4 and IPv6 literals. Rejects anything that
    /// could be read as a known_hosts pattern, marker or hash, or as a command-line option.
    pub fn new(host: &str, port: u16) -> Result<Self, HostKeyError> {
        let allowed =
            |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_' | '%');
        if host.is_empty() || host.starts_with('-') || !host.chars().all(allowed) || port == 0 {
            return Err(HostKeyError::BadHost(host.to_owned()));
        }
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    /// Parses one known_hosts pattern: `host` (port 22) or `[host]:port`.
    fn parse_pattern(p: &str) -> Result<Self, String> {
        let (host, port) = match p.strip_prefix('[') {
            Some(rest) => {
                let (h, port) = rest
                    .split_once("]:")
                    .ok_or_else(|| format!("bad pattern {p:?}"))?;
                (h, port.parse().map_err(|_| format!("bad port in {p:?}"))?)
            }
            None => (p, 22),
        };
        if host.starts_with('|') {
            return Err("hashed host names are not supported".into());
        }
        Self::new(host, port).map_err(|_| format!("unsupported host pattern {p:?}"))
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

/// The known_hosts pattern: `host` on port 22, `[host]:port` otherwise.
impl fmt::Display for HostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.port == 22 {
            f.write_str(&self.host)
        } else {
            write!(f, "[{}]:{}", self.host, self.port)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Trusted,
    Unknown,
    /// The offered key is not pinned but the host has pins. Fingerprints are `SHA256:...`.
    Mismatch {
        offered: String,
        pinned: Vec<String>,
    },
}

#[derive(Debug)]
pub enum HostKeyError {
    Io(io::Error),
    BadHost(String),
    Parse {
        line: usize,
        reason: String,
    },
    /// The file can be written by group or others, so its pins cannot be trusted.
    InsecurePermissions {
        path: PathBuf,
        mode: u32,
    },
    /// Pinning would replace a different key of the same algorithm; the user must unpin first.
    AlreadyPinned {
        host: String,
        pinned: String,
        offered: String,
    },
    /// The server offered a key that does not match the pins. Possible man-in-the-middle.
    Mismatch {
        host: String,
        offered: String,
        pinned: Vec<String>,
    },
    /// The server presented a host certificate; v1 pins plain keys only.
    Certificate(String),
    /// Could not reach the server or finish key exchange. Never contains secrets.
    Connect(String),
}

impl fmt::Display for HostKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "known_hosts I/O error: {e}"),
            Self::BadHost(h) => write!(f, "invalid host name {h:?}"),
            Self::Parse { line, reason } => write!(f, "known_hosts line {line}: {reason}"),
            Self::InsecurePermissions { path, mode } => write!(
                f,
                "{} is writable by group or others (mode {mode:o}); fix with chmod 600",
                path.display()
            ),
            Self::AlreadyPinned {
                host,
                pinned,
                offered,
            } => write!(
                f,
                "{host} already has a different pinned key ({pinned}); refusing to replace it with \
                 {offered}. If the server was really reinstalled, unpin it first."
            ),
            Self::Mismatch {
                host,
                offered,
                pinned,
            } => write!(
                f,
                "HOST KEY CHANGED for {host}: offered {offered}, pinned {}. This can mean someone is \
                 intercepting the connection. Refusing to connect. If the server was really \
                 reinstalled, verify the new fingerprint out of band, then unpin and trust it again.",
                pinned.join(", ")
            ),
            Self::Certificate(host) => {
                write!(
                    f,
                    "{host} presented a host certificate; only plain host keys are supported"
                )
            }
            Self::Connect(msg) => write!(f, "could not fetch host key: {msg}"),
        }
    }
}

impl std::error::Error for HostKeyError {}

impl From<io::Error> for HostKeyError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// `SHA256:<base64>`, the form OpenSSH prints.
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// `$XDG_CONFIG_HOME/hopsync/known_hosts`, else `$HOME/.config/hopsync/known_hosts`.
pub fn default_path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config.join("hopsync").join("known_hosts"))
}

pub struct KnownHosts {
    path: PathBuf,
    entries: Vec<(HostId, PublicKey)>,
}

impl KnownHosts {
    /// Loads the file; a missing file is an empty set of pins.
    pub fn load(path: &Path) -> Result<Self, HostKeyError> {
        let empty = Self {
            path: path.to_owned(),
            entries: Vec::new(),
        };
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(empty),
            Err(e) => return Err(e.into()),
        };
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o022 != 0 {
            return Err(HostKeyError::InsecurePermissions {
                path: path.to_owned(),
                mode,
            });
        }
        let text = std::fs::read_to_string(path)?;
        let mut entries = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let parse_err = |reason: String| HostKeyError::Parse {
                line: i + 1,
                reason,
            };
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('@') {
                return Err(parse_err(
                    "markers (@cert-authority, @revoked) are not supported".into(),
                ));
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [patterns, key_type, key_b64, ..] = fields[..] else {
                return Err(parse_err("expected \"<host> <key-type> <base64>\"".into()));
            };
            let key = PublicKey::from_openssh(&format!("{key_type} {key_b64}"))
                .map_err(|e| parse_err(format!("bad key: {e}")))?;
            for pattern in patterns.split(',') {
                entries.push((
                    HostId::parse_pattern(pattern).map_err(parse_err)?,
                    key.clone(),
                ));
            }
        }
        Ok(Self { entries, ..empty })
    }

    pub fn check(&self, host: &HostId, key: &PublicKey) -> Verdict {
        let pins: Vec<&PublicKey> = self.pins(host).collect();
        if pins.is_empty() {
            Verdict::Unknown
        } else if pins.iter().any(|p| p.key_data() == key.key_data()) {
            Verdict::Trusted
        } else {
            Verdict::Mismatch {
                offered: fingerprint(key),
                pinned: pins.into_iter().map(fingerprint).collect(),
            }
        }
    }

    /// Keys pinned for `host`, in file order. Empty for an unknown host.
    pub fn pinned_keys(&self, host: &HostId) -> Vec<PublicKey> {
        self.pins(host).cloned().collect()
    }

    /// Algorithms pinned for `host`, in file order. Empty for an unknown host.
    pub fn pinned_algorithms(&self, host: &HostId) -> Vec<Algorithm> {
        let mut algs = Vec::new();
        for alg in self.pins(host).map(PublicKey::algorithm) {
            if !algs.contains(&alg) {
                algs.push(alg);
            }
        }
        algs
    }

    /// Pins `key` for `host` and saves the file. Pinning an already pinned key is a no-op.
    pub fn pin(&mut self, host: &HostId, key: &PublicKey) -> Result<(), HostKeyError> {
        if self.pins(host).any(|p| p.key_data() == key.key_data()) {
            return Ok(());
        }
        if let Some(old) = self.pins(host).find(|p| p.algorithm() == key.algorithm()) {
            return Err(HostKeyError::AlreadyPinned {
                host: host.to_string(),
                pinned: fingerprint(old),
                offered: fingerprint(key),
            });
        }
        self.entries.push((host.clone(), key.clone()));
        self.save()
    }

    /// Removes every pin for `host` and saves the file. Returns how many were removed.
    pub fn unpin(&mut self, host: &HostId) -> Result<usize, HostKeyError> {
        let before = self.entries.len();
        self.entries.retain(|(h, _)| h != host);
        let removed = before - self.entries.len();
        if removed > 0 {
            self.save()?;
        }
        Ok(removed)
    }

    /// Every pin, in file order.
    pub fn iter(&self) -> impl Iterator<Item = (&HostId, &PublicKey)> {
        self.entries.iter().map(|(h, k)| (h, k))
    }

    fn pins<'a>(&'a self, host: &'a HostId) -> impl Iterator<Item = &'a PublicKey> {
        self.entries
            .iter()
            .filter(move |(h, _)| h == host)
            .map(|(_, k)| k)
    }

    /// Writes all entries atomically: a 0600 temp file in the same directory, fsync, rename, fsync dir.
    /// Comments and comma-joined lines from a hand-edited file are rewritten as one line per pin.
    fn save(&self) -> Result<(), HostKeyError> {
        let dir = self
            .path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let mut text = String::new();
        for (host, key) in &self.entries {
            let line = PublicKey::new(key.key_data().clone(), "")
                .to_openssh()
                .map_err(|e| io::Error::other(format!("encode key: {e}")))?;
            text.push_str(&format!("{host} {line}\n"));
        }
        let tmp = dir.join(format!(".known_hosts.tmp.{}", std::process::id()));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        let written = f.write_all(text.as_bytes()).and_then(|()| f.sync_all());
        if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, &self.path)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustOutcome {
    AlreadyTrusted,
    /// Newly pinned; carries the fingerprint the user confirmed.
    Pinned(String),
    Declined,
}

/// How long to wait for TCP connect plus key exchange when fetching a host key.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Connects to `host`, records the host key it offers, and pins it only if `confirm` returns true.
/// `confirm` receives the host, the `SHA256:` fingerprint and the key algorithm, and is never called
/// for an already trusted key or for a mismatch (which is returned as `HostKeyError::Mismatch`).
pub async fn trust(
    known: &mut KnownHosts,
    host: &HostId,
    confirm: impl FnOnce(&HostId, &str, &Algorithm) -> bool,
) -> Result<TrustOutcome, HostKeyError> {
    let key = offered_key(host).await?;
    match known.check(host, &key) {
        Verdict::Trusted => Ok(TrustOutcome::AlreadyTrusted),
        Verdict::Mismatch { offered, pinned } => Err(HostKeyError::Mismatch {
            host: host.to_string(),
            offered,
            pinned,
        }),
        Verdict::Unknown => {
            let fp = fingerprint(&key);
            if !confirm(host, &fp, &key.algorithm()) {
                return Ok(TrustOutcome::Declined);
            }
            known.pin(host, &key)?;
            Ok(TrustOutcome::Pinned(fp))
        }
    }
}

/// Runs key exchange far enough to see the server's host key, then aborts the connection. No
/// authentication is attempted and nothing is sent after the key is seen.
pub async fn offered_key(host: &HostId) -> Result<PublicKey, HostKeyError> {
    use std::sync::{Arc, Mutex};

    enum Seen {
        Key(PublicKey),
        Certificate,
    }

    struct Recorder(Arc<Mutex<Option<Seen>>>);

    impl russh::client::Handler for Recorder {
        type Error = russh::Error;
        async fn check_server_key(
            &mut self,
            key: &PublicKeyOrCertificate,
        ) -> Result<bool, Self::Error> {
            let seen = match key {
                PublicKeyOrCertificate::PublicKey { key, .. } => Seen::Key(key.clone()),
                PublicKeyOrCertificate::Certificate(_) => Seen::Certificate,
            };
            if let Ok(mut slot) = self.0.lock() {
                *slot = Some(seen);
            }
            // Never trust here: rejecting ends the handshake before any authentication.
            Ok(false)
        }
    }

    let seen = Arc::new(Mutex::new(None));
    let config = Arc::new(russh::client::Config::default());
    let connect =
        russh::client::connect(config, (host.host(), host.port()), Recorder(seen.clone()));
    let result = tokio::time::timeout(FETCH_TIMEOUT, connect).await;
    let offered = seen.lock().ok().and_then(|mut s| s.take());
    match (offered, result) {
        (Some(Seen::Key(key)), _) => Ok(key),
        (Some(Seen::Certificate), _) => Err(HostKeyError::Certificate(host.to_string())),
        (None, Err(_)) => Err(HostKeyError::Connect(format!(
            "{host}: timed out after {FETCH_TIMEOUT:?}"
        ))),
        (None, Ok(Err(e))) => Err(HostKeyError::Connect(format!("{host}: {e}"))),
        (None, Ok(Ok(_))) => Err(HostKeyError::Connect(format!(
            "{host}: connected without a host key check"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEG8meRvoV7EyXJhuHGtXcGoPbVudsgcnhBv8fpUxVzM";
    const ED_A_FP: &str = "SHA256:VL06C/1/KJcNhEefDpGgd5PJrWOWxMjysvWp7XQobRs";
    const ED_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHxKhpBmV7dGDc1bPnAfhHcbkt/iM0y5SXA0N7ZPFjBc";
    const ED_B_FP: &str = "SHA256:N2GIyywYjvseqGv7w1j0GH7DEiGwBPIMz8h1XKeztZE";
    const EC_A: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBL3pihbLA7UGMWko9UzDxjGICy7FaYQaIP7e96X9twv9gbtYRkUBq1D99I6wIH5sOZjev8WWp0mYrIKQJeWc+U0=";
    const EC_A_FP: &str = "SHA256:EaeReo9jssTEmaf3wXFGMYIz/ECOk7S3CFNUfJJT6rI";

    fn key(s: &str) -> PublicKey {
        PublicKey::from_openssh(s).expect("valid test key")
    }

    fn host(h: &str, port: u16) -> HostId {
        HostId::new(h, port).expect("valid host")
    }

    fn file_with(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("known_hosts");
        std::fs::write(&path, content).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        (dir, path)
    }

    #[test]
    fn fingerprint_matches_ssh_keygen() {
        assert_eq!(fingerprint(&key(ED_A)), ED_A_FP);
        assert_eq!(fingerprint(&key(EC_A)), EC_A_FP);
    }

    #[test]
    fn host_id_normalizes_and_formats() {
        assert_eq!(
            host("SeedBox.Example.COM", 22).to_string(),
            "seedbox.example.com"
        );
        assert_eq!(host("10.0.0.5", 2222).to_string(), "[10.0.0.5]:2222");
        assert_eq!(host("::1", 22).to_string(), "::1");
        assert_eq!(host("::1", 2222).to_string(), "[::1]:2222");
    }

    #[test]
    fn host_id_rejects_unsafe_names() {
        for bad in [
            "",
            "a b",
            "a,b",
            "-oProxyCommand=x",
            "a\nb",
            "[a]",
            "a*",
            "a?",
            "!a",
            "|1|x",
            "@a",
        ] {
            assert!(HostId::new(bad, 22).is_err(), "{bad:?} must be rejected");
        }
        assert!(
            HostId::new("ok.example", 0).is_err(),
            "port 0 must be rejected"
        );
    }

    #[test]
    fn missing_file_means_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kh = KnownHosts::load(&dir.path().join("absent")).expect("missing file is fine");
        assert_eq!(
            kh.check(&host("a.example", 22), &key(ED_A)),
            Verdict::Unknown
        );
        assert!(kh.pinned_algorithms(&host("a.example", 22)).is_empty());
    }

    #[test]
    fn pin_persists_with_private_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub/known_hosts");
        let mut kh = KnownHosts::load(&path).expect("load");
        kh.pin(&host("a.example", 2222), &key(ED_A)).expect("pin");

        let text = std::fs::read_to_string(&path).expect("file written");
        assert_eq!(text, format!("[a.example]:2222 {ED_A}\n"));
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let again = KnownHosts::load(&path).expect("reload");
        assert_eq!(
            again.check(&host("A.EXAMPLE", 2222), &key(ED_A)),
            Verdict::Trusted
        );
        assert_eq!(
            again.check(&host("a.example", 22), &key(ED_A)),
            Verdict::Unknown,
            "port matters"
        );
    }

    #[test]
    fn changed_key_is_a_mismatch_naming_both_fingerprints() {
        let (_d, path) = file_with(&format!("a.example {ED_A}\n"));
        let kh = KnownHosts::load(&path).expect("load");
        assert_eq!(
            kh.check(&host("a.example", 22), &key(ED_B)),
            Verdict::Mismatch {
                offered: ED_B_FP.into(),
                pinned: vec![ED_A_FP.into()]
            }
        );
    }

    #[test]
    fn unpinned_algorithm_on_known_host_is_a_mismatch() {
        let (_d, path) = file_with(&format!("a.example {ED_A}\n"));
        let kh = KnownHosts::load(&path).expect("load");
        assert!(matches!(
            kh.check(&host("a.example", 22), &key(EC_A)),
            Verdict::Mismatch { .. }
        ));
    }

    #[test]
    fn several_key_types_per_host() {
        let (_d, path) = file_with(&format!(
            "# comment\n\na.example {ED_A}\na.example {EC_A} root@box\n"
        ));
        let kh = KnownHosts::load(&path).expect("load");
        assert_eq!(
            kh.check(&host("a.example", 22), &key(ED_A)),
            Verdict::Trusted
        );
        assert_eq!(
            kh.check(&host("a.example", 22), &key(EC_A)),
            Verdict::Trusted
        );
        let algs = kh.pinned_algorithms(&host("a.example", 22));
        assert_eq!(algs, vec![key(ED_A).algorithm(), key(EC_A).algorithm()]);
    }

    #[test]
    fn comma_separated_patterns_and_bracketed_ports() {
        let (_d, path) = file_with(&format!("a.example,[10.0.0.5]:2222 {ED_A}\n"));
        let kh = KnownHosts::load(&path).expect("load");
        assert_eq!(
            kh.check(&host("a.example", 22), &key(ED_A)),
            Verdict::Trusted
        );
        assert_eq!(
            kh.check(&host("10.0.0.5", 2222), &key(ED_A)),
            Verdict::Trusted
        );
    }

    #[test]
    fn pin_never_replaces_a_different_key_of_the_same_type() {
        let (_d, path) = file_with(&format!("a.example {ED_A}\n"));
        let mut kh = KnownHosts::load(&path).expect("load");
        assert!(matches!(
            kh.pin(&host("a.example", 22), &key(ED_B)),
            Err(HostKeyError::AlreadyPinned { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            format!("a.example {ED_A}\n")
        );

        assert_eq!(kh.unpin(&host("a.example", 22)).expect("unpin"), 1);
        kh.pin(&host("a.example", 22), &key(ED_B))
            .expect("pin after unpin");
        assert_eq!(
            KnownHosts::load(&path)
                .expect("reload")
                .check(&host("a.example", 22), &key(ED_B)),
            Verdict::Trusted
        );
    }

    #[test]
    fn pinning_the_same_key_twice_is_a_no_op() {
        let (_d, path) = file_with(&format!("a.example {ED_A}\n"));
        let mut kh = KnownHosts::load(&path).expect("load");
        kh.pin(&host("a.example", 22), &key(ED_A)).expect("pin");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            format!("a.example {ED_A}\n")
        );
    }

    #[test]
    fn unsupported_or_corrupt_lines_fail_with_line_number() {
        for (content, line) in [
            (format!("a.example {ED_A}\ngarbage\n"), 2),
            (format!("|1|c2FsdA==|aGFzaA== {ED_A}\n"), 1),
            (format!("@cert-authority *.example {ED_A}\n"), 1),
            (format!("*.example {ED_A}\n"), 1),
            ("a.example ssh-ed25519 not-base64!!\n".to_owned(), 1),
        ] {
            let (_d, path) = file_with(&content);
            match KnownHosts::load(&path) {
                Err(HostKeyError::Parse { line: l, .. }) => assert_eq!(l, line, "{content:?}"),
                other => panic!(
                    "expected parse error for {content:?}, got {:?}",
                    other.err()
                ),
            }
        }
    }

    #[test]
    fn group_or_world_writable_file_is_refused() {
        for mode in [0o620, 0o602, 0o666] {
            let (_d, path) = file_with(&format!("a.example {ED_A}\n"));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
            assert!(
                matches!(
                    KnownHosts::load(&path),
                    Err(HostKeyError::InsecurePermissions { .. })
                ),
                "mode {mode:o}"
            );
        }
    }

    #[test]
    fn default_path_uses_xdg_or_home() {
        let p = default_path().expect("HOME is set in tests");
        assert!(p.ends_with("hopsync/known_hosts"), "{}", p.display());
    }
}
