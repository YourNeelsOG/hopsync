//! The daemon's core (Phase 2 steps 5 and 6): servers and their host keys, browsing, and the job
//! queue. `api` exposes it over HTTP; nothing here knows about HTTP.
//!
//! - Every input is parsed at this boundary: server names, hosts and users are checked, paths
//!   become `RelPath`s under the local root or the server's root, policies become `Existing`.
//! - Host keys live in the Phase 1 `known_hosts` file inside the data directory. A server is
//!   usable only once the user has confirmed its fingerprint (`trust`). A changed key fails the
//!   job and blocks the server until the user confirms the new key with `replace` set.
//! - Generated keys are sealed in the vault and opened only to connect.
//! - The scheduler runs queued jobs in queue order, `concurrency` at a time. Pause and cancel
//!   stop a running transfer through the engine's cancel signal, which keeps part files and the
//!   journal, so a paused or restarted job resumes. A daemon restart puts running jobs back in
//!   the queue, except one you asked to stop, which ends as you asked. A held queue stays held.
//! - Progress is one shared snapshot behind a `watch` channel, refreshed twice a second: any
//!   number of watchers cost the same, and a slow one only skips snapshots.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{PrivateKey, PublicKey};
use serde::Serialize;
use tokio::sync::{Notify, watch};

use crate::auth::Auth;
use crate::cli::printable;
use crate::engine::{
    self, FileReport, ItemOutcome, Problem, Remote, TransferError, TransferOptions, TreeReport,
};
use crate::hostkeys::{self, HostId, HostKeyError, KnownHosts, Verdict, fingerprint};
use crate::journal::Journal;
use crate::localio::{CachePolicy, LocalReader, LocalWriter};
use crate::paths::{EntryKind, Hidden, LocalRoot, Name, OpenMode, RelPath, RemoteRoot};
use crate::policy::Existing;
use crate::sftp::{RemoteKind, SftpError};
use crate::ssh::{KeySource, SshError, SshOptions, SshTarget};
use crate::store::{
    Direction, Job, JobItem, JobSpec, JobState, Server, ServerSpec, Store, StoreError,
};
use crate::vault::{Vault, VaultError};

const KEY_PURPOSE: &str = "ssh-key";
const SNAPSHOT_EVERY: Duration = Duration::from_millis(500);
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// How long a browse connection stays open unused. It holds memory here and a session on the
/// server, so an idle daemon, or one whose user has logged out, must not keep it.
const BROWSE_IDLE: Duration = Duration::from_secs(180);
/// Store setting: "true" while the user holds the queue. Not part of the settings API.
const QUEUE_HELD: &str = "queue_held";
/// Items a dry run returns; the counts cover everything.
pub const DRY_RUN_ITEMS: usize = 10_000;
pub const MAX_LANES: u32 = 64;
/// Directory entries one browse listing may return. A server sending more is cut off with an
/// error instead of growing the daemon's memory and the JSON reply without bound.
pub const BROWSE_MAX_ENTRIES: usize = 100_000;
/// The same bound for listings during a transfer: names of up to 255 bytes keep one listing
/// under about 300 MB even at the cap (the CLI keeps the engine default of 10 million).
pub const TRANSFER_MAX_ENTRIES: usize = 1_000_000;

#[derive(Debug)]
pub enum ServiceError {
    BadRequest(String),
    NotFound,
    Conflict(String),
    Store(StoreError),
    Vault(VaultError),
    HostKey(HostKeyError),
    Transfer(TransferError),
    Internal(String),
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(s) | Self::Conflict(s) | Self::Internal(s) => f.write_str(s),
            Self::NotFound => f.write_str("not found"),
            Self::Store(e) => write!(f, "{e}"),
            Self::Vault(e) => write!(f, "{e}"),
            Self::HostKey(e) => write!(f, "{e}"),
            Self::Transfer(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<StoreError> for ServiceError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound => Self::NotFound,
            StoreError::Conflict(s) => Self::Conflict(s),
            e => Self::Store(e),
        }
    }
}

impl From<VaultError> for ServiceError {
    fn from(e: VaultError) -> Self {
        Self::Vault(e)
    }
}

impl From<HostKeyError> for ServiceError {
    fn from(e: HostKeyError) -> Self {
        Self::HostKey(e)
    }
}

impl From<TransferError> for ServiceError {
    fn from(e: TransferError) -> Self {
        Self::Transfer(e)
    }
}

type Result<T> = std::result::Result<T, ServiceError>;

/// A file opened for a browser to save.
pub struct Download {
    pub reader: LocalReader,
    pub size: u64,
    pub name: String,
}

/// One running upload: frees its destination and removes its part file however it ends, including
/// when the browser goes away and the request is dropped.
struct Upload<'a> {
    uploads: &'a Mutex<std::collections::HashSet<String>>,
    key: String,
    local: Option<LocalRoot>,
    part: RelPath,
}

impl Drop for Upload<'_> {
    fn drop(&mut self) {
        lock(self.uploads).remove(&self.key);
        if let Some(local) = &self.local {
            // After a finished upload the part file is already gone, which is not an error.
            let _ = local.remove_file(&self.part);
        }
    }
}

/// A local file operation's failure, in the terms the API answers with.
fn local_error(e: std::io::Error) -> ServiceError {
    match e.kind() {
        std::io::ErrorKind::NotFound => ServiceError::NotFound,
        std::io::ErrorKind::AlreadyExists => {
            ServiceError::Conflict("that file already exists".into())
        }
        std::io::ErrorKind::StorageFull => ServiceError::Conflict("the disk is full".into()),
        std::io::ErrorKind::PermissionDenied => bad("Hopsync may not use this path"),
        // A symlink on the way (ELOOP), or a file where a folder should be.
        _ if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
            bad("this path is not a plain file or folder")
        }
        _ => ServiceError::Internal(e.to_string()),
    }
}

fn bad(why: impl Into<String>) -> ServiceError {
    ServiceError::BadRequest(why.into())
}

pub struct ServiceConfig {
    /// Holds `hopsync.db`, `vault.key`, `known_hosts` and `journal.sqlite`; created 0700.
    pub data_dir: PathBuf,
    /// Local directory every local path stays under.
    pub local_root: PathBuf,
    /// What stays out of reach under the local root.
    pub hidden: Hidden,
    /// Jobs running at once.
    pub concurrency: usize,
    pub ssh: SshOptions,
}

/// How the server's current host key compares with what is pinned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum KeyStatus {
    Trusted,
    Unknown,
    /// The server offers a key other than the pinned ones.
    Changed {
        pinned: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Probe {
    pub fingerprint: String,
    pub algorithm: String,
    #[serde(flatten)]
    pub status: KeyStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// Escaped for display when not `valid`.
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime: Option<u64>,
    /// Hopsync can transfer it: a valid name, and a file or directory.
    pub valid: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DryRun {
    pub files: u64,
    pub bytes: u64,
    pub problems: u64,
    pub items: Vec<JobItem>,
    /// More items than `DRY_RUN_ITEMS`; the counts are still complete.
    pub truncated: bool,
}

/// Defaults the UI applies to new jobs, and the local free-space reserve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Settings {
    pub default_existing: String,
    pub default_verify: bool,
    /// Bytes to leave free on this box's filesystem when pulling.
    pub local_min_free: u64,
}

/// A partial update of `Settings`; unknown keys are refused.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsUpdate {
    pub default_existing: Option<String>,
    pub default_verify: Option<bool>,
    pub local_min_free: Option<u64>,
}

/// One running job's live numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Progress {
    pub job: i64,
    /// Lets a client total upload and download speed without fetching the job list.
    pub direction: Direction,
    /// Bytes moved so far; pieces redone after a reconnect count again.
    pub bytes: u64,
    pub files: u64,
    pub problems: u64,
    /// Bytes per second, smoothed.
    pub rate: u64,
    /// Known for single files; a tree's size is only known after its walk.
    pub total: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    /// Increases whenever a job is added, changes state or is removed, so a client knows when
    /// to fetch the job list again.
    pub changes: u64,
    pub queue_paused: bool,
    pub running: Vec<Progress>,
}

#[derive(Default)]
struct Counters {
    bytes: AtomicU64,
    files: AtomicU64,
    problems: AtomicU64,
}

struct Running {
    cancel: watch::Sender<bool>,
    /// Why the job is being stopped: `Paused` or `Cancelled`.
    stop: Mutex<Option<JobState>>,
    counters: Arc<Counters>,
    direction: Direction,
    total: Option<u64>,
    /// Bytes and time at the last snapshot, and the smoothed rate.
    last: Mutex<(u64, Instant, f64)>,
}

pub struct Service {
    /// Exclusive `flock` on `hopsync.lock`: one daemon per data directory.
    _lock: std::fs::File,
    data_dir: PathBuf,
    pub store: Arc<Store>,
    pub auth: Auth,
    vault: Vault,
    known_path: PathBuf,
    /// Serializes changes to the known_hosts file.
    pins: tokio::sync::Mutex<()>,
    local_root: PathBuf,
    hidden: Hidden,
    journal: Arc<Journal>,
    concurrency: usize,
    ssh: SshOptions,
    running: Mutex<HashMap<i64, Arc<Running>>>,
    queue_paused: AtomicBool,
    wake: Arc<Notify>,
    changes: AtomicU64,
    snapshot: watch::Sender<Arc<Snapshot>>,
    /// Set once the daemon is stopping; long-lived responses (event streams) end on it.
    stopping: watch::Sender<bool>,
    /// Remotes kept warm for browsing, by server id, each with when it was last used.
    browse: Mutex<HashMap<i64, (Remote, Instant)>>,
    /// Destinations a browser is uploading to right now.
    uploads: Mutex<std::collections::HashSet<String>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn parse_existing(s: &str) -> Result<Existing> {
    Ok(match s {
        "skip" => Existing::Skip,
        "update" => Existing::Update,
        "overwrite" => Existing::Overwrite,
        "rename" => Existing::Rename,
        _ => return Err(bad("existing must be skip, update, overwrite or rename")),
    })
}

/// A path relative to a root, as the API sends it: slash-separated, no `..`, no control
/// characters. Empty means the root itself.
pub fn parse_rel(s: &str) -> Result<RelPath> {
    let s = s.trim_matches('/');
    if s.is_empty() {
        return Ok(RelPath::default());
    }
    RelPath::parse(s).map_err(|e| bad(format!("bad path: {e}")))
}

fn check_text(what: &str, s: &str, max: usize) -> Result<()> {
    if s.is_empty() || s.len() > max {
        return Err(bad(format!("{what} must have 1 to {max} bytes")));
    }
    if s.chars().any(char::is_control) {
        return Err(bad(format!("{what} must not contain control characters")));
    }
    Ok(())
}

fn check_spec(s: &ServerSpec) -> Result<()> {
    check_text("name", &s.name, 64)?;
    check_text("user", &s.user, 64)?;
    if s.user
        .contains(|c: char| c.is_whitespace() || c == '@' || c == ':')
    {
        return Err(bad("user must not contain spaces, '@' or ':'"));
    }
    HostId::new(&s.host, s.port).map_err(|e| bad(format!("host: {e}")))?;
    RemoteRoot::parse(&s.root).map_err(|e| bad(format!("root: {e}")))?;
    if let Some(k) = &s.key_file
        && !Path::new(k).is_absolute()
    {
        return Err(bad("key_file must be an absolute path"));
    }
    Ok(())
}

fn host_of(server: &Server) -> Result<HostId> {
    HostId::new(&server.host, server.port).map_err(|e| bad(format!("host: {e}")))
}

fn item_of(path: &str, out: ItemOutcome<'_>) -> JobItem {
    let (outcome, bytes, detail) = match out {
        ItemOutcome::Sent { bytes, dst } => ("sent", bytes, dst.to_owned()),
        ItemOutcome::Planned {
            bytes,
            dst,
            replace,
        } => (
            if replace { "replace" } else { "create" },
            bytes,
            dst.to_owned(),
        ),
        ItemOutcome::Unchanged => ("unchanged", 0, String::new()),
        ItemOutcome::Problem(p @ Problem::Failed(_)) => ("failed", 0, p.to_string()),
        ItemOutcome::Problem(p) => ("skipped", 0, p.to_string()),
    };
    // Paths and error texts can come from the server: escape them once, here, for every reader.
    JobItem {
        path: printable(path),
        outcome: outcome.to_owned(),
        bytes,
        detail: printable(&detail),
    }
}

/// A host key failure that means the server is not who it was: block it until re-trusted.
fn changed_key(e: &TransferError) -> Option<String> {
    match e {
        TransferError::Remote(SftpError::Ssh(SshError::HostKeyMismatch { offered, .. })) => {
            Some(format!("host key changed: the server now offers {offered}"))
        }
        TransferError::RetriesExhausted(SftpError::Ssh(SshError::HostKeyMismatch {
            offered,
            ..
        })) => Some(format!("host key changed: the server now offers {offered}")),
        _ => None,
    }
}

impl Service {
    /// Opens the data directory and the store; jobs left running by a stopped daemon go back to
    /// the queue. Call `start` to run the scheduler.
    pub fn open(config: ServiceConfig) -> Result<Arc<Self>> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&config.data_dir)
            .map_err(|e| ServiceError::Internal(format!("{}: {e}", config.data_dir.display())))?;
        // Taken before anything else: a second daemon must not touch the store (it would put the
        // first one's running jobs back in the queue). Released when the process exits.
        let lock_path = config.data_dir.join("hopsync.lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| ServiceError::Internal(format!("{}: {e}", lock_path.display())))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |_| {
                ServiceError::Conflict(format!(
                    "{} is already in use by another hopsync daemon",
                    config.data_dir.display()
                ))
            },
        )?;
        let store = Arc::new(Store::open(&config.data_dir.join("hopsync.db"))?);
        // A missing key with secrets in the store must stop the daemon, never mint a new key.
        let vault = Vault::open_or_create(
            &config.data_dir.join("vault.key"),
            store.secret_count()? == 0,
        )?;
        let token_file = config.data_dir.join("setup-token");
        let auth = Auth::new(store.clone(), Some(&token_file))
            .map_err(|e| ServiceError::Internal(e.to_string()))?;
        let journal = Arc::new(
            Journal::open(&config.data_dir.join("journal.sqlite"))
                .map_err(|e| ServiceError::Internal(e.to_string()))?,
        );
        let queue_held = store.setting(QUEUE_HELD)?.as_deref() == Some("true");
        let requeued = store.requeue_running()?;
        if requeued > 0 {
            store.audit("restart", &format!("{requeued} running jobs requeued"))?;
        }
        LocalRoot::open(&config.local_root).map_err(|e| {
            ServiceError::Internal(format!("local root {}: {e}", config.local_root.display()))
        })?;
        Ok(Arc::new(Self {
            _lock: lock,
            data_dir: config.data_dir.clone(),
            store,
            auth,
            vault,
            known_path: config.data_dir.join("known_hosts"),
            pins: tokio::sync::Mutex::new(()),
            local_root: config.local_root,
            hidden: config.hidden,
            journal,
            concurrency: config.concurrency.max(1),
            ssh: config.ssh,
            running: Mutex::new(HashMap::new()),
            queue_paused: AtomicBool::new(queue_held),
            wake: Arc::new(Notify::new()),
            changes: AtomicU64::new(0),
            snapshot: watch::channel(Arc::new(Snapshot::default())).0,
            stopping: watch::channel(false).0,
            browse: Mutex::new(HashMap::new()),
            uploads: Mutex::new(std::collections::HashSet::new()),
        }))
    }

    /// Starts the scheduler and the progress ticker on the current runtime.
    pub fn start(self: &Arc<Self>) {
        // The loops hold only weak references, so dropping the last `Arc<Service>` stops them.
        let (weak, wake) = (Arc::downgrade(self), self.wake.clone());
        tokio::spawn(async move {
            loop {
                match weak.upgrade() {
                    Some(me) => {
                        me.fill();
                        if me.close_idle_browse(BROWSE_IDLE) > 0 {
                            // Hands the connection's buffers back to the system.
                            tokio::task::spawn_blocking(crate::pool::release_free_memory);
                        }
                    }
                    None => break,
                }
                // The timeout only lets the loop notice that the service is gone.
                let _ = tokio::time::timeout(Duration::from_secs(5), wake.notified()).await;
            }
        });
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SNAPSHOT_EVERY);
            loop {
                tick.tick().await;
                match weak.upgrade() {
                    Some(me) => me.publish(),
                    None => break,
                }
            }
        });
    }

    /// Closes browse connections unused for at least `idle_for`, returning how many. The next
    /// listing opens a new one. A listing still running keeps its own handle and finishes.
    pub fn close_idle_browse(&self, idle_for: Duration) -> usize {
        let mut browse = lock(&self.browse);
        let before = browse.len();
        browse.retain(|_, (_, used)| used.elapsed() < idle_for);
        before - browse.len()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.snapshot.subscribe()
    }

    /// Marks the daemon as stopping, which ends every open event stream.
    pub fn begin_shutdown(&self) {
        self.stopping.send_replace(true);
    }

    /// Resolves once `begin_shutdown` has been called.
    pub fn stopping(&self) -> impl Future<Output = ()> + Send + 'static + use<> {
        let mut rx = self.stopping.subscribe();
        async move {
            let _ = rx.wait_for(|s| *s).await;
        }
    }

    /// The file holding the setup token until setup is done.
    pub fn setup_token_file(&self) -> PathBuf {
        self.data_dir.join("setup-token")
    }

    pub fn local_root(&self) -> &Path {
        &self.local_root
    }

    fn changed(&self) {
        self.changes.fetch_add(1, Ordering::Relaxed);
        self.publish();
    }

    fn publish(&self) {
        let now = Instant::now();
        let mut running: Vec<Progress> = lock(&self.running)
            .iter()
            .map(|(&job, r)| {
                let bytes = r.counters.bytes.load(Ordering::Relaxed);
                let mut last = lock(&r.last);
                let dt = now.duration_since(last.1).as_secs_f64();
                if dt >= 0.2 {
                    let instant = bytes.saturating_sub(last.0) as f64 / dt;
                    // Smooth over about two seconds.
                    last.2 = if last.2 == 0.0 {
                        instant
                    } else {
                        last.2 * 0.75 + instant * 0.25
                    };
                    (last.0, last.1) = (bytes, now);
                }
                Progress {
                    job,
                    direction: r.direction,
                    bytes,
                    files: r.counters.files.load(Ordering::Relaxed),
                    problems: r.counters.problems.load(Ordering::Relaxed),
                    rate: last.2 as u64,
                    total: r.total,
                }
            })
            .collect();
        running.sort_by_key(|p| p.job);
        let snap = Snapshot {
            changes: self.changes.load(Ordering::Relaxed),
            queue_paused: self.queue_paused.load(Ordering::Relaxed),
            running,
        };
        self.snapshot.send_if_modified(|old| {
            if **old == snap {
                return false;
            }
            *old = Arc::new(snap);
            true
        });
    }

    // Servers.

    pub fn add_server(&self, spec: &ServerSpec, generate_key: bool) -> Result<Server> {
        check_spec(spec)?;
        if generate_key == spec.key_file.is_some() {
            return Err(bad(
                "give either key_file or generate_key, not both or neither",
            ));
        }
        let server = self.store.add_server(spec)?;
        if generate_key && let Err(e) = self.generate_key(server.id) {
            let _ = self.store.delete_server(server.id);
            return Err(e);
        }
        self.store.audit(
            "server-added",
            &format!(
                "{} {}@{}:{}",
                server.name, server.user, server.host, server.port
            ),
        )?;
        self.store.server(server.id).map_err(Into::into)
    }

    fn generate_key(&self, server: i64) -> Result<()> {
        let mut seed = zeroize::Zeroizing::new([0u8; 32]);
        aws_lc_rs::rand::fill(&mut seed[..])
            .map_err(|_| ServiceError::Internal("random failed".into()))?;
        let mut key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
        key.set_comment(format!("hopsync-server-{server}"));
        let pem = key
            .to_openssh(LineEnding::LF)
            .map_err(|e| ServiceError::Internal(format!("key encoding: {e}")))?;
        // The row id is part of the sealed value's associated data, so the row comes first.
        let id = self.store.add_secret(KEY_PURPOSE, &[])?;
        let sealed = self
            .vault
            .seal("secrets", id, KEY_PURPOSE, pem.as_bytes())?;
        self.store.set_secret(id, &sealed)?;
        self.store.set_server_key_secret(server, Some(id))?;
        Ok(())
    }

    fn sealed_key(&self, secret: i64) -> Result<PrivateKey> {
        let (purpose, sealed) = self.store.secret(secret)?;
        if purpose != KEY_PURPOSE {
            return Err(ServiceError::Internal(
                "secret has the wrong purpose".into(),
            ));
        }
        let pem = self.vault.open("secrets", secret, KEY_PURPOSE, &sealed)?;
        PrivateKey::from_openssh(&pem[..])
            .map_err(|e| ServiceError::Internal(format!("stored key: {e}")))
    }

    /// The public half of a generated key, as an `authorized_keys` line.
    pub fn public_key(&self, server: i64) -> Result<Option<String>> {
        let s = self.store.server(server)?;
        let Some(secret) = s.key_secret else {
            return Ok(None);
        };
        let line = self
            .sealed_key(secret)?
            .public_key()
            .to_openssh()
            .map_err(|e| ServiceError::Internal(format!("key encoding: {e}")))?;
        Ok(Some(line))
    }

    /// Adds the server's generated public key to its `authorized_keys`, logging in once with
    /// `auth` (a password or another key the user already has). The password is used for that
    /// one connection and never stored or logged. `authorized_keys` defaults to
    /// `.ssh/authorized_keys` in the login directory; only files named `authorized_keys` or
    /// `authorized_keys2` are accepted. Installing a key that is already there changes nothing.
    pub async fn install_key(
        &self,
        server: i64,
        auth: KeySource,
        authorized_keys: Option<&str>,
    ) -> Result<()> {
        let s = self.store.server(server)?;
        let line = self
            .public_key(server)?
            .ok_or_else(|| bad("this server uses a key file, not a generated key"))?;
        let path = authorized_keys.unwrap_or(".ssh/authorized_keys");
        check_text("authorized_keys", path, 4096)?;
        let (dir, name) = match path.rsplit_once('/') {
            Some((d, n)) => (Some(d), n),
            None => (None, path),
        };
        if !matches!(name, "authorized_keys" | "authorized_keys2") {
            return Err(bad(
                "the file must be named authorized_keys or authorized_keys2",
            ));
        }
        if matches!(auth, KeySource::Loaded(_)) {
            return Err(bad("install with a password or a key file"));
        }
        let remote = self.remote(&s)?.with_auth(auth);
        let sftp = remote.session(&self.base_opts()).await?;
        let io = |e: SftpError| ServiceError::Transfer(e.into());
        if let Some(d) = dir.filter(|d| !d.is_empty())
            && sftp.mkdir(d).await.map_err(io)?
        {
            sftp.chmod(d, 0o700).await.map_err(io)?;
        }
        // The key blob (second field) identifies the key; comments may differ.
        let blob = line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        let (existing, created) = match sftp.lstat(path).await {
            Ok(a) if a.kind == RemoteKind::File => {
                if a.size > 1 << 20 {
                    return Err(bad(format!(
                        "{path} is larger than 1 MiB; install the key by hand"
                    )));
                }
                let f = sftp
                    .open(path, crate::sftp::RemoteOpenMode::Read)
                    .await
                    .map_err(io)?;
                let mut buf = vec![0u8; usize::try_from(a.size).unwrap_or(0)];
                let mut got = 0;
                while got < buf.len() {
                    let n = f.read_at(got as u64, &mut buf[got..]).await.map_err(io)?;
                    if n == 0 {
                        break;
                    }
                    got += n;
                }
                buf.truncate(got);
                let _ = f.close().await;
                (buf, false)
            }
            Ok(_) => {
                return Err(bad(format!(
                    "{path} is not a regular file (a symlink?); install the key by hand"
                )));
            }
            Err(SftpError::NotFound(_)) => (Vec::new(), true),
            Err(e) => return Err(io(e)),
        };
        let text = String::from_utf8_lossy(&existing);
        if !text
            .lines()
            .any(|l| l.split_whitespace().any(|f| f == blob))
        {
            let mut add = String::new();
            if !existing.is_empty() && !existing.ends_with(b"\n") {
                add.push('\n');
            }
            add.push_str(&line);
            add.push('\n');
            let f = sftp
                .open(path, crate::sftp::RemoteOpenMode::WriteCreate)
                .await
                .map_err(io)?;
            f.write_at(existing.len() as u64, add.as_bytes())
                .await
                .map_err(io)?;
            f.close().await.map_err(io)?;
            if created {
                sftp.chmod(path, 0o600).await.map_err(io)?;
            }
            self.store
                .audit("key-installed", &format!("{} {path}", s.name))?;
        }
        // Prove it: log in with the generated key alone.
        lock(&self.browse).remove(&server);
        let check = self.remote(&s)?;
        check.list(&RelPath::default(), &self.base_opts()).await.map_err(|e| {
            ServiceError::Conflict(format!(
                "the key was added to {path}, but logging in with it still fails ({e}); check that sshd reads that file and that its permissions are strict"
            ))
        })?;
        Ok(())
    }

    pub fn update_server(&self, id: i64, spec: &ServerSpec) -> Result<Server> {
        check_spec(spec)?;
        let old = self.store.server(id)?;
        if old.key_secret.is_some() && spec.key_file.is_some() {
            return Err(bad(
                "this server uses a generated key; remove and add it again to use a key file",
            ));
        }
        if old.key_secret.is_none() && spec.key_file.is_none() {
            return Err(bad("key_file is required"));
        }
        let s = self.store.update_server(id, spec)?;
        lock(&self.browse).remove(&id);
        self.store.audit("server-changed", &s.name)?;
        Ok(s)
    }

    pub fn delete_server(&self, id: i64) -> Result<()> {
        let s = self.store.server(id)?;
        self.store.delete_server(id)?;
        lock(&self.browse).remove(&id);
        self.store.audit("server-removed", &s.name)?;
        self.changed();
        Ok(())
    }

    /// Fetches the host key the server offers now and compares it with the pins. Trusts nothing.
    pub async fn probe(&self, server: i64) -> Result<Probe> {
        let s = self.store.server(server)?;
        let host = host_of(&s)?;
        let key = hostkeys::offered_key(&host).await?;
        let known = KnownHosts::load(&self.known_path)?;
        let status = match known.check(&host, &key) {
            Verdict::Trusted => KeyStatus::Trusted,
            Verdict::Unknown => KeyStatus::Unknown,
            Verdict::Mismatch { pinned, .. } => KeyStatus::Changed { pinned },
        };
        Ok(Probe {
            fingerprint: fingerprint(&key),
            algorithm: key.algorithm().to_string(),
            status,
        })
    }

    /// Pins the server's key after the user confirmed `fingerprint`. The key is fetched again
    /// and must still have that fingerprint. Replacing a different pinned key needs `replace`.
    pub async fn trust(&self, server: i64, confirmed: &str, replace: bool) -> Result<()> {
        let s = self.store.server(server)?;
        let host = host_of(&s)?;
        let key: PublicKey = hostkeys::offered_key(&host).await?;
        let offered = fingerprint(&key);
        if offered != confirmed {
            return Err(ServiceError::Conflict(format!(
                "the server now offers {offered}, not the confirmed key; probe it again"
            )));
        }
        let _one = self.pins.lock().await;
        let mut known = KnownHosts::load(&self.known_path)?;
        match known.check(&host, &key) {
            Verdict::Trusted => {}
            Verdict::Unknown => {
                known.pin(&host, &key)?;
                self.store
                    .audit("host-key-trusted", &format!("{host} {offered}"))?;
            }
            Verdict::Mismatch { pinned, .. } => {
                if !replace {
                    return Err(ServiceError::Conflict(format!(
                        "{host} has a different pinned key ({}); confirm the replacement",
                        pinned.join(", ")
                    )));
                }
                known.unpin(&host)?;
                known.pin(&host, &key)?;
                self.store.audit(
                    "host-key-replaced",
                    &format!("{host} {} -> {offered}", pinned.join(",")),
                )?;
            }
        }
        // Other servers on the same host and port share the pin, so they are fixed as well.
        for other in self.store.servers()? {
            if other.blocked.is_some() && host_of(&other).is_ok_and(|h| h == host) {
                self.store.set_server_blocked(other.id, None)?;
                lock(&self.browse).remove(&other.id);
            }
        }
        Ok(())
    }

    fn remote(&self, s: &Server) -> Result<Remote> {
        if let Some(why) = &s.blocked {
            return Err(ServiceError::Conflict(format!(
                "{} is blocked: {why}; confirm its new host key first",
                s.name
            )));
        }
        let key = match (&s.key_file, s.key_secret) {
            (_, Some(secret)) => KeySource::Loaded(Arc::new(self.sealed_key(secret)?)),
            (Some(f), None) => KeySource::File(PathBuf::from(f)),
            (None, None) => return Err(bad("the server has no key")),
        };
        let target = SshTarget {
            host: host_of(s)?,
            user: s.user.clone(),
            key,
        };
        let root = RemoteRoot::parse(&s.root).map_err(|e| bad(format!("root: {e}")))?;
        let known = KnownHosts::load(&self.known_path)?;
        Ok(Remote::new(target, known, root))
    }

    fn base_opts(&self) -> TransferOptions {
        TransferOptions {
            ssh: self.ssh.clone(),
            sftp: crate::sftp::SftpOptions {
                max_dir_entries: TRANSFER_MAX_ENTRIES,
                ..crate::sftp::SftpOptions::default()
            },
            ..TransferOptions::default()
        }
    }

    /// Lists a directory on a server, under its root.
    pub async fn list_remote(&self, server: i64, path: &str) -> Result<Vec<Entry>> {
        let rel = parse_rel(path)?;
        let s = self.store.server(server)?;
        let cached = lock(&self.browse).get_mut(&server).map(|(r, used)| {
            *used = Instant::now();
            r.clone()
        });
        let remote = match cached {
            Some(r) => r,
            None => {
                let r = self.remote(&s)?;
                lock(&self.browse).insert(server, (r.clone(), Instant::now()));
                r
            }
        };
        let mut opts = self.base_opts();
        opts.sftp.max_dir_entries = BROWSE_MAX_ENTRIES;
        let listed = remote.list(&rel, &opts).await;
        // A slow listing counts as use up to the moment it ends.
        if let Some((_, used)) = lock(&self.browse).get_mut(&server) {
            *used = Instant::now();
        }
        let listed = match listed {
            Ok(l) => l,
            Err(e) => {
                lock(&self.browse).remove(&server);
                if let Some(why) = changed_key(&e) {
                    self.store.set_server_blocked(server, Some(&why))?;
                }
                return Err(e.into());
            }
        };
        let mut out: Vec<Entry> = listed
            .into_iter()
            .map(|(name, a)| {
                let kind = match a.kind {
                    RemoteKind::File => Kind::File,
                    RemoteKind::Dir => Kind::Dir,
                    RemoteKind::Symlink => Kind::Symlink,
                    RemoteKind::Other => Kind::Other,
                };
                let valid = Name::new(&name).is_ok() && matches!(kind, Kind::File | Kind::Dir);
                Entry {
                    name: printable(&name),
                    kind,
                    size: a.size,
                    mtime: a.mtime,
                    valid,
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Opens a file under the local root for a browser to save.
    pub async fn open_download(&self, path: &str) -> Result<Download> {
        let rel = parse_rel(path)?;
        let name = rel
            .components()
            .last()
            .ok_or_else(|| bad("name a file"))?
            .as_str()
            .to_owned();
        let (root, hidden) = (self.local_root.clone(), self.hidden);
        let reader =
            tokio::task::spawn_blocking(move || -> std::io::Result<Option<LocalReader>> {
                let local = LocalRoot::open_hiding(&root, hidden)?;
                // Checked first: opening a FIFO would wait forever. A symlink is not a file here.
                if !local.metadata(&rel)?.is_file() {
                    return Ok(None);
                }
                let file = local.open_file(&rel, OpenMode::Read)?;
                Ok(Some(LocalReader::new(file, CachePolicy::Drop)))
            })
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?
            .map_err(local_error)?
            .ok_or_else(|| bad("only files can be saved"))?;
        let size = reader.size().map_err(local_error)?;
        self.store.audit("download", path)?;
        Ok(Download { reader, size, name })
    }

    /// Stores a browser's upload at `path` under the local root. Data goes to a part file that
    /// becomes the file only once it is complete and on disk, and is removed if anything fails.
    pub async fn upload_local<S, E>(
        &self,
        path: &str,
        overwrite: bool,
        declared: Option<u64>,
        mut body: S,
    ) -> Result<u64>
    where
        S: futures_util::Stream<Item = std::result::Result<axum::body::Bytes, E>> + Unpin,
        E: std::fmt::Display,
    {
        use futures_util::StreamExt;

        let rel = parse_rel(path)?;
        let (parents, name) = match rel.components().split_last() {
            Some((name, parents)) => (parents.to_vec(), name.as_str().to_owned()),
            None => return Err(bad("name a file")),
        };
        // Its own suffix: a pull job to the same file must not share this part file.
        let part_name = Name::new(&format!("{name}.hopsync-upload"))
            .map_err(|_| bad("the file name is too long"))?;
        let mut part = RelPath::default();
        for p in parents {
            part = part.join(p);
        }
        let part = part.join(part_name);
        if !lock(&self.uploads).insert(rel.to_slash_string()) {
            return Err(ServiceError::Conflict(
                "this file is already being uploaded".into(),
            ));
        }
        let min_free: u64 = self
            .store
            .setting("local_min_free")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let (root, hidden) = (self.local_root.clone(), self.hidden);
        let mut guard = Upload {
            uploads: &self.uploads,
            key: rel.to_slash_string(),
            local: None,
            part: part.clone(),
        };
        let (local, writer) = {
            let (rel, part) = (rel.clone(), part.clone());
            tokio::task::spawn_blocking(move || -> Result<(LocalRoot, LocalWriter)> {
                let local = LocalRoot::open_hiding(&root, hidden).map_err(local_error)?;
                match local.metadata(&rel) {
                    Ok(m) if m.is_dir() => {
                        return Err(ServiceError::Conflict("a folder has that name".into()));
                    }
                    Ok(_) if !overwrite => {
                        return Err(ServiceError::Conflict("that file already exists".into()));
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(local_error(e)),
                }
                if let Some(need) = declared {
                    let free = local.available_bytes(&rel).map_err(local_error)?;
                    if free < need.saturating_add(min_free) {
                        return Err(ServiceError::Conflict(format!(
                            "not enough space: {need} bytes needed, {free} free, {min_free} kept free"
                        )));
                    }
                }
                let file = local
                    .open_file(&part, OpenMode::WriteCreate)
                    .map_err(local_error)?;
                // Whatever an earlier crash left there is not this upload.
                file.set_len(0).map_err(local_error)?;
                Ok((local, LocalWriter::new(file, CachePolicy::Drop)))
            })
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))??
        };
        guard.local = Some(local.try_clone().map_err(local_error)?);

        let mut chunk = writer.chunk(0);
        let mut total = 0u64;
        while let Some(data) = body.next().await {
            let data = data.map_err(|e| bad(format!("the upload was cut off: {e}")))?;
            total += data.len() as u64;
            // One chunk at a time: the next is read only after this one is written.
            chunk = tokio::task::spawn_blocking(move || chunk.write(&data).map(|()| chunk))
                .await
                .map_err(|e| ServiceError::Internal(e.to_string()))?
                .map_err(local_error)?;
        }
        if declared.is_some_and(|d| d != total) {
            return Err(bad("the upload was cut off"));
        }
        let finished_part = part.clone();
        let to = rel;
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            chunk.finish()?;
            writer.sync_data()?;
            local.rename(&finished_part, &to, overwrite)?;
            local.sync_parent(&to)
        })
        .await
        .map_err(|e| ServiceError::Internal(e.to_string()))?
        .map_err(local_error)?;
        self.store.audit("upload", path)?;
        Ok(total)
    }

    /// Lists a directory under the local root.
    pub async fn list_local(&self, path: &str) -> Result<Vec<Entry>> {
        let rel = parse_rel(path)?;
        let (root, hidden) = (self.local_root.clone(), self.hidden);
        let listed = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<Entry>> {
            let local = LocalRoot::open_hiding(&root, hidden)?;
            let mut out = Vec::new();
            for e in local.read_dir(&rel)? {
                let name = e.name.to_str().and_then(|n| Name::new(n).ok());
                let (size, mtime) = match &name {
                    Some(n) => match local.metadata(&rel.join(n.clone())) {
                        Ok(m) => (
                            m.len(),
                            m.modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs()),
                        ),
                        Err(_) => (0, None),
                    },
                    None => (0, None),
                };
                let kind = match e.kind {
                    EntryKind::File => Kind::File,
                    EntryKind::Dir => Kind::Dir,
                    EntryKind::Symlink => Kind::Symlink,
                    EntryKind::Other => Kind::Other,
                };
                out.push(Entry {
                    name: printable(&e.name.to_string_lossy()),
                    kind,
                    size,
                    mtime,
                    valid: name.is_some() && matches!(kind, Kind::File | Kind::Dir),
                });
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
        .await
        .map_err(|e| ServiceError::Internal(e.to_string()))?;
        listed.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ServiceError::NotFound,
            _ => bad(format!("{path}: {e}")),
        })
    }

    // Settings.

    pub fn settings(&self) -> Result<Settings> {
        let get = |k: &str| self.store.setting(k);
        Ok(Settings {
            default_existing: get("default_existing")?
                .filter(|v| parse_existing(v).is_ok())
                .unwrap_or_else(|| "skip".into()),
            default_verify: get("default_verify")?.is_some_and(|v| v == "true"),
            local_min_free: get("local_min_free")?
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        })
    }

    pub fn update_settings(&self, u: &SettingsUpdate) -> Result<Settings> {
        if let Some(e) = &u.default_existing {
            parse_existing(e)?;
        }
        if let Some(e) = &u.default_existing {
            self.store.set_setting("default_existing", e)?;
        }
        if let Some(v) = u.default_verify {
            self.store
                .set_setting("default_verify", if v { "true" } else { "false" })?;
        }
        if let Some(n) = u.local_min_free {
            self.store.set_setting("local_min_free", &n.to_string())?;
        }
        self.store.audit("settings-changed", "")?;
        self.settings()
    }

    // Jobs.

    fn check_job(&self, spec: &JobSpec) -> Result<()> {
        if parse_rel(&spec.src)?.components().is_empty() {
            return Err(bad("src must name a file or directory below the root"));
        }
        if parse_rel(&spec.dst)?.components().is_empty() {
            return Err(bad("dst must name the destination below the root"));
        }
        parse_existing(&spec.existing)?;
        if spec.lanes.is_some_and(|l| l == 0 || l > MAX_LANES) {
            return Err(bad(format!("lanes must be 1 to {MAX_LANES}")));
        }
        let server = self.store.server(spec.server)?;
        if let Some(why) = server.blocked {
            return Err(ServiceError::Conflict(format!(
                "{} is blocked: {why}; confirm its new host key first",
                server.name
            )));
        }
        Ok(())
    }

    fn normalized(spec: &JobSpec) -> Result<JobSpec> {
        Ok(JobSpec {
            src: parse_rel(&spec.src)?.to_slash_string(),
            dst: parse_rel(&spec.dst)?.to_slash_string(),
            ..spec.clone()
        })
    }

    pub fn add_job(&self, spec: &JobSpec) -> Result<Job> {
        self.check_job(spec)?;
        let job = self.store.add_job(&Self::normalized(spec)?)?;
        self.store.add_job_event(job.id, "queued")?;
        self.changed();
        self.wake.notify_one();
        Ok(job)
    }

    /// Plans a job without transferring: what would be sent, skipped or replaced.
    pub async fn dry_run(&self, spec: &JobSpec) -> Result<DryRun> {
        self.check_job(spec)?;
        let spec = Self::normalized(spec)?;
        let items = Arc::new(Mutex::new((Vec::new(), false)));
        let on_item = {
            let items = items.clone();
            Arc::new(move |path: &str, out: ItemOutcome<'_>| {
                let mut i = lock(&items);
                if i.0.len() < DRY_RUN_ITEMS {
                    i.0.push(item_of(path, out));
                } else {
                    i.1 = true;
                }
            })
        };
        let opts = TransferOptions {
            dry_run: true,
            on_item: Some(on_item),
            ..self.job_opts(&spec)?
        };
        let (files, bytes, problems) = self.transfer(&spec, &opts).await?;
        let (items, truncated) = std::mem::take(&mut *lock(&items));
        Ok(DryRun {
            files,
            bytes,
            problems,
            items,
            truncated,
        })
    }

    fn job_opts(&self, spec: &JobSpec) -> Result<TransferOptions> {
        let (lanes, max_lanes) = match spec.lanes {
            Some(n) => (n, n),
            None => (4, 16),
        };
        let min_free = match spec.direction {
            Direction::Push => self.store.server(spec.server)?.min_free,
            Direction::Pull => self
                .store
                .setting("local_min_free")?
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        };
        Ok(TransferOptions {
            lanes,
            max_lanes,
            existing: parse_existing(&spec.existing)?,
            verify: spec.verify,
            min_free,
            journal: Some(self.journal.clone()),
            ..self.base_opts()
        })
    }

    /// Runs one transfer; returns files sent, bytes and problems.
    async fn transfer(&self, spec: &JobSpec, opts: &TransferOptions) -> Result<(u64, u64, u64)> {
        let server = self.store.server(spec.server)?;
        let remote = self.remote(&server)?;
        let (root, hidden) = (self.local_root.clone(), self.hidden);
        let local = tokio::task::spawn_blocking(move || LocalRoot::open_hiding(&root, hidden))
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?
            .map_err(|e| ServiceError::Internal(format!("local root: {e}")))?;
        let (src, dst) = (parse_rel(&spec.src)?, parse_rel(&spec.dst)?);
        let file = |r: FileReport| (u64::from(!r.unchanged), r.bytes, 0);
        let tree = |r: TreeReport| (r.files, r.bytes, r.problems.len() as u64);
        Ok(match spec.direction {
            Direction::Push => {
                let meta = local.metadata(&src).map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => bad(format!("{}: not found", spec.src)),
                    _ => bad(format!("{}: {e}", spec.src)),
                })?;
                if meta.is_dir() {
                    tree(engine::push_tree(&local, &src, &remote, &dst, opts).await?)
                } else {
                    file(engine::push_file(&local, &src, &remote, &dst, opts).await?)
                }
            }
            Direction::Pull => match remote.kind(&remote.root().join(&src), opts).await? {
                Some(RemoteKind::Dir) => {
                    tree(engine::pull_tree(&remote, &src, &local, &dst, opts).await?)
                }
                Some(RemoteKind::File) => {
                    file(engine::pull_file(&remote, &src, &local, &dst, opts).await?)
                }
                Some(_) => return Err(bad(format!("{}: not a file or directory", spec.src))),
                None => return Err(bad(format!("{}: not found on the server", spec.src))),
            },
        })
    }

    /// Starts queued jobs while there is room.
    fn fill(self: &Arc<Self>) {
        while !self.queue_paused.load(Ordering::Relaxed)
            && lock(&self.running).len() < self.concurrency
        {
            let next = match self.store.next_queued() {
                Ok(Some(j)) => j,
                Ok(None) => return,
                Err(e) => {
                    eprintln!("hopsync: scheduler: {e}");
                    return;
                }
            };
            let total = match next.spec.direction {
                Direction::Push => parse_rel(&next.spec.src).ok().and_then(|rel| {
                    let m = LocalRoot::open_hiding(&self.local_root, self.hidden)
                        .ok()?
                        .metadata(&rel)
                        .ok()?;
                    m.is_file().then_some(m.len())
                }),
                Direction::Pull => None,
            };
            let running = Arc::new(Running {
                cancel: watch::channel(false).0,
                stop: Mutex::new(None),
                counters: Arc::default(),
                direction: next.spec.direction,
                total,
                last: Mutex::new((0, Instant::now(), 0.0)),
            });
            if let Err(e) = self.store.set_job_state(next.id, JobState::Running, None) {
                eprintln!("hopsync: scheduler: {e}");
                return;
            }
            lock(&self.running).insert(next.id, running.clone());
            self.changed();
            let me = self.clone();
            tokio::spawn(async move { me.run_job(next, running).await });
        }
    }

    async fn run_job(self: Arc<Self>, job: Job, running: Arc<Running>) {
        let id = job.id;
        let _ = self.store.clear_job_items(id);
        let _ = self.store.add_job_event(id, "started");
        let pending = Arc::new(Mutex::new(Vec::<JobItem>::new()));
        let result = match self.job_opts(&job.spec) {
            Err(e) => Err(e),
            Ok(base) => {
                let counters = running.counters.clone();
                let progress = {
                    let c = counters.clone();
                    Arc::new(move |n: u64| {
                        c.bytes.fetch_add(n, Ordering::Relaxed);
                    })
                };
                let on_item = {
                    let (c, pending) = (counters.clone(), pending.clone());
                    Arc::new(move |path: &str, out: ItemOutcome<'_>| {
                        match out {
                            ItemOutcome::Sent { .. } => c.files.fetch_add(1, Ordering::Relaxed),
                            ItemOutcome::Problem(_) => c.problems.fetch_add(1, Ordering::Relaxed),
                            _ => 0,
                        };
                        lock(&pending).push(item_of(path, out));
                    })
                };
                let opts = TransferOptions {
                    progress: Some(progress),
                    on_item: Some(on_item),
                    cancel: Some(running.cancel.subscribe()),
                    ..base
                };
                let transfer = self.transfer(&job.spec, &opts);
                tokio::pin!(transfer);
                let mut tick = tokio::time::interval(FLUSH_EVERY);
                loop {
                    tokio::select! {
                        r = &mut transfer => break r,
                        _ = tick.tick() => self.flush(id, &pending).await,
                    }
                }
            }
        };
        self.flush(id, &pending).await;
        let c = &running.counters;
        let _ = self.store.set_job_totals(
            id,
            c.files.load(Ordering::Relaxed),
            c.bytes.load(Ordering::Relaxed),
            c.problems.load(Ordering::Relaxed),
        );
        let stop = *lock(&running.stop);
        let (state, error) = match (&result, stop) {
            (Err(ServiceError::Transfer(TransferError::Cancelled)), Some(s)) => (s, None),
            (Ok(_), _) => (JobState::Done, None),
            (Err(e), _) => (JobState::Failed, Some(printable(&e.to_string()))),
        };
        if let Err(ServiceError::Transfer(e)) = &result
            && let Some(why) = changed_key(e)
        {
            let _ = self.store.set_server_blocked(job.spec.server, Some(&why));
            let _ = self.store.audit(
                "host-key-changed",
                &format!("server {}: {why}", job.spec.server),
            );
        }
        let _ = tokio::task::spawn_blocking(crate::pool::release_free_memory).await;
        let _ = self.store.set_job_state(id, state, error.as_deref());
        let _ = self.store.add_job_event(
            id,
            &match &error {
                Some(e) => format!("{}: {e}", state.as_str()),
                None => state.as_str().to_owned(),
            },
        );
        lock(&self.running).remove(&id);
        self.changed();
        self.wake.notify_one();
    }

    async fn flush(&self, job: i64, pending: &Arc<Mutex<Vec<JobItem>>>) {
        let items = std::mem::take(&mut *lock(pending));
        if items.is_empty() {
            return;
        }
        let store = self.store.clone();
        let written = tokio::task::spawn_blocking(move || store.add_job_items(job, &items)).await;
        if let Ok(Err(e)) = written {
            eprintln!("hopsync: job {job}: saving results: {e}");
        }
    }

    /// Stops a running job as `state` (paused or cancelled). Saved first, so that a daemon that
    /// dies before the transfer has stopped cannot run the job again after its restart.
    fn stop_running(&self, id: i64, state: JobState) -> Result<()> {
        self.store.want_job_stop(id, state)?;
        if let Some(r) = lock(&self.running).get(&id).cloned() {
            *lock(&r.stop) = Some(state);
            r.cancel.send_replace(true);
        }
        Ok(())
    }

    pub fn pause_job(&self, id: i64) -> Result<()> {
        let job = self.store.job(id)?;
        match job.state {
            JobState::Running => {
                self.stop_running(id, JobState::Paused)?;
            }
            JobState::Queued => {
                self.store.set_job_state(id, JobState::Paused, None)?;
                self.store.add_job_event(id, "paused")?;
                self.changed();
            }
            JobState::Paused => {}
            _ => return Err(ServiceError::Conflict("the job is finished".into())),
        }
        Ok(())
    }

    pub fn cancel_job(&self, id: i64) -> Result<()> {
        let job = self.store.job(id)?;
        match job.state {
            JobState::Running => {
                self.stop_running(id, JobState::Cancelled)?;
            }
            JobState::Queued | JobState::Paused => {
                self.store.set_job_state(id, JobState::Cancelled, None)?;
                self.store.add_job_event(id, "cancelled")?;
                self.changed();
            }
            _ => {}
        }
        Ok(())
    }

    /// Puts a paused, failed or cancelled job back in the queue; it resumes from the journal.
    pub fn resume_job(&self, id: i64) -> Result<()> {
        let job = self.store.job(id)?;
        match job.state {
            JobState::Paused | JobState::Failed | JobState::Cancelled => {
                // Its destination may have been taken by a newer job meanwhile.
                let clash = self.store.jobs(None)?.into_iter().any(|j| {
                    j.id != id
                        && matches!(
                            j.state,
                            JobState::Queued | JobState::Running | JobState::Paused
                        )
                        && j.spec.direction == job.spec.direction
                        && j.spec.server == job.spec.server
                        && j.spec.dst == job.spec.dst
                });
                if clash {
                    return Err(ServiceError::Conflict(
                        "another unfinished job writes the same destination".into(),
                    ));
                }
                self.store.set_job_state(id, JobState::Queued, None)?;
                self.store.add_job_event(id, "queued again")?;
                self.changed();
                self.wake.notify_one();
                Ok(())
            }
            JobState::Queued | JobState::Running => Ok(()),
            JobState::Done => Err(ServiceError::Conflict("the job is done".into())),
        }
    }

    pub fn delete_job(&self, id: i64) -> Result<()> {
        self.store.delete_job(id)?;
        self.changed();
        Ok(())
    }

    pub fn move_job(&self, id: i64, before: Option<i64>) -> Result<()> {
        self.store.move_job(id, before)?;
        self.changed();
        Ok(())
    }

    /// Stops starting new jobs, also after a restart; running ones finish.
    pub fn pause_queue(&self) -> Result<()> {
        self.store.set_setting(QUEUE_HELD, "true")?;
        self.queue_paused.store(true, Ordering::Relaxed);
        self.changed();
        Ok(())
    }

    pub fn resume_queue(&self) -> Result<()> {
        self.store.set_setting(QUEUE_HELD, "false")?;
        self.queue_paused.store(false, Ordering::Relaxed);
        self.changed();
        self.wake.notify_one();
        Ok(())
    }

    pub fn queue_paused(&self) -> bool {
        self.queue_paused.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod progress_tests {
    use super::{Direction, Progress};

    /// The UI totals upload and download speed from this field.
    #[test]
    fn progress_tells_clients_its_direction() {
        let p = Progress {
            job: 1,
            direction: Direction::Pull,
            bytes: 2,
            files: 3,
            problems: 0,
            rate: 4,
            total: None,
        };
        let json = serde_json::to_value(&p).expect("json");
        assert_eq!(json["direction"], "pull");
    }
}
