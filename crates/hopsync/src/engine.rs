//! Transfers over parallel lanes: single files and directory trees (plan.md §6.1, Phase 1 steps 9
//! and 10).
//!
//! - A lane is one SSH connection with one SFTP session. `lanes × per_lane` workers share the lanes
//!   and pull work items (one chunk of one file, from `plan`) off a bounded channel, so many small
//!   files overlap their round trips while a big file is split across every lane.
//! - A producer feeds the channel: one file, or a streamed tree walk (list a directory, create it on
//!   the other side, queue its files, descend). Back-pressure from the channel keeps the walk from
//!   running ahead of the transfer.
//! - Within a chunk a worker keeps up to `inflight` pieces in flight as an ordered pipeline, so pulls
//!   can hand pieces to the sequential `ChunkWriter`. All buffers come from one pool sized
//!   `lanes × inflight`; a worker that cannot get one drains its own pipeline instead of waiting, so
//!   workers sharing the pool cannot deadlock.
//! - Data goes to `<name>.hopsync-part`. After the last chunk the part is cut to the source size
//!   (a stale longer part must not leak through), given the source mtime, and renamed into place.
//!   Files of 1 MiB and more are fsynced first; a pull ends with one `syncfs` on the local root
//!   instead of an fsync per small file.
//! - Lanes start at `lanes`. With `max_lanes` above that, `tune` adds one at a time while each
//!   raises throughput. At most two lanes handshake at once (sshd's `MaxStartups` drops excess
//!   unauthenticated connections), and a lane that never connected and is refused while another is
//!   up is retired without spending retries: its workers move to a live lane.
//! - Errors are sorted three ways. Lost connections and timeouts: the lane reconnects with backoff
//!   and the chunk is redone (positional writes make that idempotent), within a shared retry budget.
//!   SSH-level failures (auth, host key) and an exhausted budget: the whole transfer stops. Anything
//!   else (permission denied, source changed, odd names): only that file or directory fails, and
//!   the report lists it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{OnceCell, Semaphore, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use twox_hash::XxHash3_128;

use crate::hostkeys::KnownHosts;
use crate::journal::Journal;
use crate::localio::{CachePolicy, ChunkWriter, LocalReader, LocalWriter};
use crate::paths::{EntryKind, LocalRoot, Name, OpenMode, RelPath, RemoteRoot};
use crate::plan::{self, PlanConfig, Range};
use crate::policy::{self, Decision, Existing, Found};
use crate::pool::BufferPool;
use crate::sftp::{RemoteFile, RemoteKind, RemoteOpenMode, Sftp, SftpError, SftpOptions};
use crate::ssh::{SshError, SshOptions, SshTarget, connect};
use crate::tune::Tuner;

const PART_SUFFIX: &str = ".hopsync-part";
/// Page size multiple for pieces, so dropping a piece's pages never leaves a straddling page behind.
const PAGE: usize = 4096;
/// Files at least this big are fsynced before the rename; smaller ones rely on the end-of-transfer
/// sync (local) or the server's own write-back (remote). See the plan.md Decision log.
const FSYNC_MIN: u64 = 1 << 20;

/// Called with the byte count of each piece as it completes. Retried pieces are reported again.
pub type ProgressFn = Arc<dyn Fn(u64) + Send + Sync>;

/// Called once per source file with its path relative to the source top and how it ended.
pub type ItemFn = Arc<dyn Fn(&str, ItemOutcome<'_>) + Send + Sync>;

/// How one source file ended, for `TransferOptions::on_item`.
#[derive(Debug, Clone, Copy)]
pub enum ItemOutcome<'a> {
    /// Transferred to `dst` (remote path for a push, path under the local root for a pull).
    Sent { bytes: u64, dst: &'a str },
    /// Dry run: would be transferred to `dst`, over an existing file when `replace`.
    Planned {
        bytes: u64,
        dst: &'a str,
        replace: bool,
    },
    /// Policy `Update` found the destination identical.
    Unchanged,
    /// Skipped or failed; the problem says which and why.
    Problem(&'a Problem),
}

#[derive(Clone)]
pub struct TransferOptions {
    /// SSH connections at the start. A single file uses at most one per chunk.
    pub lanes: u32,
    /// Auto-tune ceiling. Above `lanes`, a lane is added every few seconds while each one raises
    /// throughput by 10 % (see `tune`); at or below it, the count stays at `lanes`.
    pub max_lanes: u32,
    /// Files in flight per lane in a tree transfer, to overlap small-file round trips.
    pub per_lane: u32,
    pub plan: PlanConfig,
    pub cache: CachePolicy,
    /// What to do with destination files that already exist.
    pub existing: Existing,
    /// Bytes to leave free on the destination filesystem. Files that would cut into it fail before
    /// any of their data moves.
    pub min_free: u64,
    /// Lost connections and timeouts tolerated per transfer, across all lanes.
    pub retries: u32,
    pub ssh: SshOptions,
    pub sftp: SftpOptions,
    pub progress: Option<ProgressFn>,
    /// Records durable chunks of multi-chunk files so an interrupted transfer resumes. None: every
    /// run starts files from zero.
    pub journal: Option<Arc<Journal>>,
    /// Read every chunk back from the destination and compare its xxh3-128 hash with the source's,
    /// before the file is renamed into place. Chunks resumed from an earlier run are checked too.
    pub verify: bool,
    /// Walk and apply the policy, reporting each file as `Planned`, but create and write nothing.
    pub dry_run: bool,
    pub on_item: Option<ItemFn>,
    /// Set to true to stop: the transfer ends with `Cancelled` as soon as in-flight requests are
    /// dropped. Part files and the journal stay, so a later run resumes.
    pub cancel: Option<watch::Receiver<bool>>,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            lanes: 4,
            max_lanes: 4,
            per_lane: 4,
            plan: PlanConfig::default(),
            cache: CachePolicy::default(),
            existing: Existing::default(),
            min_free: 0,
            retries: 8,
            ssh: SshOptions::default(),
            sftp: SftpOptions::default(),
            progress: None,
            journal: None,
            verify: false,
            dry_run: false,
            on_item: None,
            cancel: None,
        }
    }
}

impl TransferOptions {
    fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|c| *c.borrow())
    }

    fn item(&self, path: &str, outcome: ItemOutcome<'_>) {
        if let Some(f) = &self.on_item {
            f(path, outcome);
        }
    }
}

/// Where and how to reach the remote side.
#[derive(Clone)]
pub struct Remote {
    target: SshTarget,
    known: Arc<KnownHosts>,
    root: RemoteRoot,
    /// A session left by `kind`, used as the next transfer's first lane instead of a new
    /// connection. Shared by clones (`with_root`).
    warm: Arc<Mutex<Option<Sftp>>>,
}

impl Remote {
    pub fn new(target: SshTarget, known: KnownHosts, root: RemoteRoot) -> Self {
        Self {
            target,
            known: Arc::new(known),
            root,
            warm: Arc::new(Mutex::new(None)),
        }
    }

    /// The same server and account under another root, sharing any warm session.
    pub fn with_root(&self, root: RemoteRoot) -> Self {
        Self {
            root,
            ..self.clone()
        }
    }

    /// What is at `path` (absolute, or relative to the login directory), without following a
    /// symlink; None if nothing. The connection stays open for the next transfer through this
    /// `Remote` or a `with_root` of it, so looking before transferring costs no extra handshake.
    pub async fn kind(
        &self,
        path: &str,
        opts: &TransferOptions,
    ) -> Result<Option<RemoteKind>, TransferError> {
        let sftp = connect_session(self, opts).await?;
        let kind = match sftp.lstat(path).await {
            Ok(a) => Some(a.kind),
            Err(SftpError::NotFound(_)) => None,
            Err(e) => return Err(e.into()),
        };
        if let Ok(mut warm) = self.warm.lock() {
            *warm = Some(sftp);
        }
        Ok(kind)
    }

    /// The entries of directory `rel` under the root, unfiltered (callers validate names). Keeps
    /// the session warm like `kind`, so browsing does not pay a handshake per directory; a warm
    /// session the server has dropped meanwhile is replaced once.
    pub async fn list(
        &self,
        rel: &RelPath,
        opts: &TransferOptions,
    ) -> Result<Vec<(String, crate::sftp::RemoteAttrs)>, TransferError> {
        let path = self.root.join(rel);
        let mut fresh_tried = false;
        loop {
            let was_warm = self.warm.lock().is_ok_and(|w| w.is_some());
            let sftp = connect_session(self, opts).await?;
            match sftp.read_dir(&path).await {
                Ok(entries) => {
                    if let Ok(mut warm) = self.warm.lock() {
                        *warm = Some(sftp);
                    }
                    return Ok(entries);
                }
                Err(e) => {
                    let e = TransferError::from(e);
                    if was_warm && !fresh_tried && is_transient(&e) {
                        fresh_tried = true;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    pub fn root(&self) -> &RemoteRoot {
        &self.root
    }

    /// The same remote, authenticating with `key` instead; never shares a warm session.
    pub fn with_auth(&self, key: crate::ssh::KeySource) -> Self {
        let mut target = self.target.clone();
        target.key = key;
        Self {
            target,
            warm: Arc::new(Mutex::new(None)),
            ..self.clone()
        }
    }

    /// A fresh SFTP session (or the warm one), for operations outside a transfer.
    pub async fn session(&self, opts: &TransferOptions) -> Result<Sftp, TransferError> {
        connect_session(self, opts).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReport {
    pub bytes: u64,
    /// Lane reconnects needed.
    pub retries: u32,
    /// Bytes an earlier interrupted run had already made durable, not sent again.
    pub resumed: u64,
    /// Bytes read back from the destination and found identical to the source (`verify`).
    pub verified: u64,
    /// Lanes that connected (fewer than asked when the server refused some).
    pub lanes: u32,
    /// Policy `Update` found the destination identical; nothing was sent.
    pub unchanged: bool,
    /// Where the file went: the destination asked for, or a free name beside it (policy `Rename`).
    pub dst: String,
}

#[derive(Debug, Default)]
pub struct TreeReport {
    /// Files transferred.
    pub files: u64,
    /// Files already identical at the destination (policy `Update`).
    pub unchanged: u64,
    /// Directories created or reused below the top.
    pub dirs: u64,
    pub bytes: u64,
    pub retries: u32,
    /// Bytes skipped because an earlier run had already made them durable.
    pub resumed: u64,
    /// Bytes of transferred files read back and found identical (`verify`).
    pub verified: u64,
    /// Lanes that connected (fewer than asked when the server refused some).
    pub lanes: u32,
    /// Entries not transferred, by path relative to the source top (slash-separated; names that are
    /// not valid UTF-8 appear lossily).
    pub problems: Vec<(String, Problem)>,
}

#[derive(Debug)]
pub enum Problem {
    /// Symlinks are skipped, never followed (CLAUDE.md §3: paths stay inside their roots).
    Symlink,
    /// Sockets, FIFOs and devices.
    Special,
    /// Not UTF-8, or has control characters (a newline, for example): refused rather than mangled.
    InvalidName,
    /// A `.hopsync-part` file: an unfinished earlier transfer, never data to send. Sending it
    /// would also collide with the part file of its complete sibling.
    PartFile,
    /// The destination exists and the policy is `Skip`.
    Exists,
    /// The destination is newer than the source (policy `Update`): it may hold edits.
    Newer,
    Failed(TransferError),
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Symlink => f.write_str("skipped: symlink"),
            Self::Special => f.write_str("skipped: not a regular file or directory"),
            Self::InvalidName => {
                f.write_str("skipped: name is not UTF-8 or has control characters")
            }
            Self::PartFile => f.write_str("skipped: unfinished Hopsync part file"),
            Self::Exists => f.write_str("skipped: destination exists"),
            Self::Newer => f.write_str("skipped: destination is newer than the source"),
            Self::Failed(e) => write!(f, "failed: {e}"),
        }
    }
}

#[derive(Debug)]
pub enum TransferError {
    Local {
        path: String,
        source: io::Error,
    },
    Remote(SftpError),
    NotAFile(String),
    DestinationExists(String),
    /// The destination's parent directory does not exist (trees create directories; single files
    /// do not).
    MissingDirectory(String),
    SourceChanged(String),
    DestinationNewer(String),
    /// Transferring would leave less than `min_free` on the destination.
    NoSpace {
        path: String,
        need: u64,
        available: u64,
    },
    /// The destination does not hold what was sent: the chunk at `offset` hashes differently from
    /// the source. The file is not renamed into place, and none of its chunks is resumed later.
    VerifyFailed {
        path: String,
        offset: u64,
    },
    /// The retry budget ran out; carries the last transient error.
    RetriesExhausted(SftpError),
    /// `TransferOptions::cancel` was set.
    Cancelled,
}

impl fmt::Display for TransferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local { path, source } => write!(f, "{path}: {source}"),
            Self::Remote(e) => write!(f, "{e}"),
            Self::NotAFile(p) => write!(f, "{p}: not a regular file"),
            Self::DestinationExists(p) => write!(f, "{p}: destination exists"),
            Self::MissingDirectory(p) => write!(f, "{p}: directory does not exist"),
            Self::SourceChanged(p) => write!(f, "{p}: source changed during transfer"),
            Self::DestinationNewer(p) => write!(f, "{p}: destination is newer than the source"),
            Self::NoSpace {
                path,
                need,
                available,
            } => write!(
                f,
                "{path}: not enough space at the destination ({need} bytes needed, {available} available above the reserve)"
            ),
            Self::VerifyFailed { path, offset } => write!(
                f,
                "{path}: verify failed: the destination differs from the source in the chunk at byte {offset}"
            ),
            Self::RetriesExhausted(e) => write!(f, "giving up after repeated failures: {e}"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for TransferError {}

impl From<SftpError> for TransferError {
    fn from(e: SftpError) -> Self {
        Self::Remote(e)
    }
}

fn local_err(path: &str) -> impl FnOnce(io::Error) -> TransferError + '_ {
    move |source| TransferError::Local {
        path: path.to_owned(),
        source,
    }
}

/// Failures a fresh connection may fix.
fn is_transient(e: &TransferError) -> bool {
    matches!(
        e,
        TransferError::Remote(
            SftpError::ConnectionLost(_)
                | SftpError::Timeout
                | SftpError::Ssh(SshError::Timeout { .. } | SshError::Connect { .. })
        )
    )
}

/// Failures no other file would escape: stop the whole transfer.
fn is_fatal(e: &TransferError) -> bool {
    matches!(
        e,
        TransferError::RetriesExhausted(_) | TransferError::Remote(SftpError::Ssh(_))
    )
}

/// `dst`'s sibling that receives data. Names too long for the suffix are shortened and tagged with a
/// hash of the full name, so distinct long names keep distinct part files.
pub fn part_path(dst: &RelPath) -> Result<RelPath, TransferError> {
    let (last, parents) = dst
        .components()
        .split_last()
        .ok_or_else(|| TransferError::NotAFile("<root>".into()))?;
    let name = last.as_str();
    let mut part = format!("{name}{PART_SUFFIX}");
    if part.len() > 255 {
        let tag = format!("~{:016x}{PART_SUFFIX}", fnv1a(name.as_bytes()));
        let mut keep = 255 - tag.len();
        while !name.is_char_boundary(keep) {
            keep -= 1;
        }
        part = format!("{}{tag}", &name[..keep]);
    }
    let part = Name::new(&part).map_err(|e| TransferError::Local {
        path: dst.to_slash_string(),
        source: io::Error::new(io::ErrorKind::InvalidInput, e.to_string()),
    })?;
    Ok(rel_of(parents).join(part))
}

fn rel_of(names: &[Name]) -> RelPath {
    names
        .iter()
        .fold(RelPath::default(), |p, n| p.join(n.clone()))
}

fn parent_label(rel: &RelPath) -> String {
    let parents = rel.components().split_last().map_or(&[][..], |(_, p)| p);
    rel_of(parents).to_slash_string()
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn join_label(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

// Entry points.

/// Copies local file `src` to remote `dst` (both relative to their roots).
pub async fn push_file(
    local: &LocalRoot,
    src: &RelPath,
    remote: &Remote,
    dst: &RelPath,
    opts: &TransferOptions,
) -> Result<FileReport, TransferError> {
    let label = src.to_slash_string();
    let meta = local.metadata(src).map_err(local_err(&label))?;
    if !meta.is_file() {
        return Err(TransferError::NotAFile(label));
    }
    check_cancel(opts)?;
    let first = connect_session(remote, opts).await?;
    let found = |rel: RelPath| {
        let (sftp, path) = (first.clone(), remote.root.join(&rel));
        async move {
            match sftp.lstat(&path).await {
                Ok(a) => Ok(Some(Found {
                    size: a.size,
                    mtime: a.mtime,
                    is_file: a.kind == RemoteKind::File,
                })),
                Err(SftpError::NotFound(_)) => Ok(None),
                Err(e) => Err(TransferError::from(e)),
            }
        }
    };
    let src_mtime = meta.modified().ok().map(unix_secs);
    let existing = found(dst.clone()).await?;
    let decision = policy::decide(opts.existing, meta.len(), src_mtime, existing);
    let Some((dst, replace)) = single_target(decision, dst, found).await? else {
        return Ok(unchanged_report(opts, &label, remote.root.join(dst)));
    };
    let dst_path = remote.root.join(&dst);
    if opts.dry_run {
        return Ok(planned_report(opts, &label, meta.len(), dst_path, replace));
    }
    let mut file = FileState::push(remote, src.clone(), &dst, label, &meta)?;
    file.replace = replace;
    let lanes = single_file_lanes(file.size, opts);
    let report = run(
        Direction::Push,
        remote,
        local,
        opts,
        lanes,
        1,
        first,
        |shared, tx| async move {
            shared.enqueue(&tx, file).await;
            Ok(())
        },
    )
    .await?;
    single_result(report, dst_path)
}

/// Copies remote file `src` to local `dst` (both relative to their roots).
pub async fn pull_file(
    remote: &Remote,
    src: &RelPath,
    local: &LocalRoot,
    dst: &RelPath,
    opts: &TransferOptions,
) -> Result<FileReport, TransferError> {
    check_cancel(opts)?;
    let first = connect_session(remote, opts).await?;
    let src_path = remote.root.join(src);
    let attrs = first.lstat(&src_path).await?;
    if attrs.kind != RemoteKind::File {
        return Err(TransferError::NotAFile(src_path));
    }
    let found = |rel: RelPath| async move { local_found(local, &rel) };
    let existing = local_found(local, dst)?;
    let decision = policy::decide(opts.existing, attrs.size, attrs.mtime, existing);
    let label = src.to_slash_string();
    let Some((dst, replace)) = single_target(decision, dst, found).await? else {
        return Ok(unchanged_report(opts, &label, dst.to_slash_string()));
    };
    if opts.dry_run {
        let to = dst.to_slash_string();
        return Ok(planned_report(opts, &label, attrs.size, to, replace));
    }
    let mut file = FileState::pull(
        &src_path,
        dst.clone(),
        src_path.clone(),
        attrs.size,
        attrs.mtime,
    )?;
    file.replace = replace;
    let lanes = single_file_lanes(file.size, opts);
    let report = run(
        Direction::Pull,
        remote,
        local,
        opts,
        lanes,
        1,
        first,
        |shared, tx| async move {
            shared.enqueue(&tx, file).await;
            Ok(())
        },
    )
    .await?;
    single_result(report, dst.to_slash_string())
}

/// What is at a local destination, without following a symlink there.
fn local_found(local: &LocalRoot, rel: &RelPath) -> Result<Option<Found>, TransferError> {
    match local.metadata(rel) {
        Ok(m) => Ok(Some(Found {
            size: m.len(),
            mtime: m.modified().ok().map(unix_secs),
            is_file: m.is_file(),
        })),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(local_err(&rel.to_slash_string())(e)),
    }
}

/// Where a single file goes and whether it replaces what is there, from the policy decision.
/// None: unchanged, nothing to do. `found` looks up a destination candidate.
async fn single_target<F, Fut>(
    decision: Decision,
    dst: &RelPath,
    found: F,
) -> Result<Option<(RelPath, bool)>, TransferError>
where
    F: Fn(RelPath) -> Fut,
    Fut: Future<Output = Result<Option<Found>, TransferError>>,
{
    let label = dst.to_slash_string();
    match decision {
        Decision::Create => Ok(Some((dst.clone(), false))),
        Decision::Replace => Ok(Some((dst.clone(), true))),
        Decision::Unchanged => Ok(None),
        Decision::KeepExisting => Err(TransferError::DestinationExists(label)),
        Decision::KeepNewer => Err(TransferError::DestinationNewer(label)),
        Decision::NotAFile => Err(TransferError::NotAFile(label)),
        Decision::WriteBeside => {
            let (last, parents) = dst
                .components()
                .split_last()
                .ok_or_else(|| TransferError::NotAFile(label.clone()))?;
            let mut taken = HashSet::from([last.as_str().to_owned()]);
            loop {
                let name = policy::free_name(last.as_str(), &taken)
                    .and_then(|n| Name::new(&n).ok())
                    .ok_or_else(|| TransferError::DestinationExists(label.clone()))?;
                let candidate = rel_of(parents).join(name.clone());
                if found(candidate.clone()).await?.is_none() {
                    return Ok(Some((candidate, false)));
                }
                taken.insert(name.as_str().to_owned());
            }
        }
    }
}

fn unchanged_report(opts: &TransferOptions, label: &str, dst: String) -> FileReport {
    opts.item(label, ItemOutcome::Unchanged);
    FileReport {
        bytes: 0,
        retries: 0,
        resumed: 0,
        verified: 0,
        lanes: 1,
        unchanged: true,
        dst,
    }
}

/// A dry run's answer for one file: `bytes` would go to `dst`.
fn planned_report(
    opts: &TransferOptions,
    label: &str,
    bytes: u64,
    dst: String,
    replace: bool,
) -> FileReport {
    let planned = ItemOutcome::Planned {
        bytes,
        dst: &dst,
        replace,
    };
    opts.item(label, planned);
    FileReport {
        bytes,
        retries: 0,
        resumed: 0,
        verified: 0,
        lanes: 0,
        unchanged: false,
        dst,
    }
}

fn check_cancel(opts: &TransferOptions) -> Result<(), TransferError> {
    if opts.cancelled() {
        return Err(TransferError::Cancelled);
    }
    Ok(())
}

/// Copies the local directory `src` to remote `dst`, creating `dst` if needed (its parent must exist).
pub async fn push_tree(
    local: &LocalRoot,
    src: &RelPath,
    remote: &Remote,
    dst: &RelPath,
    opts: &TransferOptions,
) -> Result<TreeReport, TransferError> {
    check_cancel(opts)?;
    let first = connect_session(remote, opts).await?;
    let top = remote.root.join(dst);
    if !opts.dry_run {
        first.mkdir(&top).await.map_err(|e| match e {
            SftpError::NotFound(_) => {
                TransferError::MissingDirectory(top.rsplit_once('/').map_or(".", |(p, _)| p).into())
            }
            e => e.into(),
        })?;
    }
    let (src, dst) = (src.clone(), dst.clone());
    let lanes = tree_lanes(opts);
    let per_lane = opts.per_lane.max(1) as usize;
    run(
        Direction::Push,
        remote,
        local,
        opts,
        lanes,
        per_lane,
        first,
        move |shared, tx| walk_local(shared, tx, src, dst),
    )
    .await
}

/// Copies the remote directory `src` to local `dst`, creating `dst` if needed (its parent must exist).
pub async fn pull_tree(
    remote: &Remote,
    src: &RelPath,
    local: &LocalRoot,
    dst: &RelPath,
    opts: &TransferOptions,
) -> Result<TreeReport, TransferError> {
    check_cancel(opts)?;
    let first = connect_session(remote, opts).await?;
    let src_path = remote.root.join(src);
    if first.lstat(&src_path).await?.kind != RemoteKind::Dir {
        return Err(TransferError::NotAFile(src_path));
    }
    if !dst.components().is_empty() && !opts.dry_run {
        local.create_dir(dst).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => TransferError::MissingDirectory(parent_label(dst)),
            _ => local_err(&dst.to_slash_string())(e),
        })?;
    }
    let (src, dst) = (src.clone(), dst.clone());
    let lanes = tree_lanes(opts);
    let per_lane = opts.per_lane.max(1) as usize;
    run(
        Direction::Pull,
        remote,
        local,
        opts,
        lanes,
        per_lane,
        first,
        move |shared, tx| walk_remote(shared, tx, src, dst),
    )
    .await
}

/// Lanes at the start and the auto-tune ceiling: a single file never uses more than its chunks.
fn single_file_lanes(size: u64, opts: &TransferOptions) -> (usize, usize) {
    let chunks = plan::chunk_count(size, &plan_for(opts));
    let cap = |n: u32| usize::try_from(chunks.min(u64::from(n.max(1)))).unwrap_or(1);
    (cap(opts.lanes), cap(lane_ceiling(opts)))
}

fn tree_lanes(opts: &TransferOptions) -> (usize, usize) {
    (opts.lanes.max(1) as usize, lane_ceiling(opts) as usize)
}

fn lane_ceiling(opts: &TransferOptions) -> u32 {
    opts.max_lanes.max(opts.lanes).max(1)
}

fn single_result(mut report: TreeReport, dst: String) -> Result<FileReport, TransferError> {
    match report.problems.pop() {
        None => Ok(FileReport {
            bytes: report.bytes,
            retries: report.retries,
            resumed: report.resumed,
            verified: report.verified,
            lanes: report.lanes,
            unchanged: false,
            dst,
        }),
        Some((_, Problem::Failed(e))) => Err(e),
        Some((_, Problem::Exists)) => Err(TransferError::DestinationExists(dst)),
        Some((_, Problem::Newer)) => Err(TransferError::DestinationNewer(dst)),
        Some((path, _)) => Err(TransferError::NotAFile(path)),
    }
}

/// Chunks are planned for the lane ceiling, so lanes added by auto-tune find chunks to take.
fn plan_for(opts: &TransferOptions) -> PlanConfig {
    PlanConfig {
        lanes: lane_ceiling(opts),
        ..opts.plan
    }
}

async fn connect_session(remote: &Remote, opts: &TransferOptions) -> Result<Sftp, TransferError> {
    if let Some(s) = remote.warm.lock().ok().and_then(|mut w| w.take()) {
        return Ok(s);
    }
    let conn = connect(&remote.target, &remote.known, &opts.ssh)
        .await
        .map_err(SftpError::from)?;
    // The session keeps the connection alive; the handle itself can go.
    Ok(Sftp::start(&conn, opts.sftp.clone()).await?)
}

// Shared transfer state.

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Push,
    Pull,
}

/// One file in a transfer. Shared by the work items of its chunks.
struct FileState {
    /// Path relative to the source top, for the report.
    label: String,
    size: u64,
    ends: FileEnds,
    /// Chunks not yet finished; whoever finishes the last one finalizes the file.
    remaining: AtomicU64,
    /// Set by `enqueue`: the plan's chunk size, or the one a resumed file was first planned with.
    chunk_size: u64,
    /// Journal row, for files with more than one chunk while a journal is in use.
    journal_id: Option<i64>,
    /// Rename over an existing destination (the policy decided to replace it).
    replace: bool,
    failed: AtomicBool,
    /// A push's part file has been created; later opens must not create it again.
    part_created: AtomicBool,
    /// Verify found a damaged chunk: once the file's last item is done, the journal forgets its
    /// chunks so a later run cannot resume the damage.
    distrust: AtomicBool,
    /// A pull's part file opened for reading, for verify.
    check: OnceCell<Arc<LocalReader>>,
    /// Local side, opened by the first worker to need it and closed with the last reference.
    local: OnceCell<LocalSide>,
}

enum FileEnds {
    Push {
        src: RelPath,
        /// Source identity at listing time; a change by the end fails the file.
        len: u64,
        mtime: Option<SystemTime>,
        part: String,
        dst: String,
    },
    Pull {
        src: String,
        mtime: Option<u64>,
        part: RelPath,
        dst: RelPath,
    },
}

enum LocalSide {
    Reader(Arc<LocalReader>),
    Writer(Arc<LocalWriter>),
}

impl FileState {
    fn push(
        remote: &Remote,
        src: RelPath,
        dst: &RelPath,
        label: String,
        meta: &std::fs::Metadata,
    ) -> Result<Self, TransferError> {
        Ok(Self::new(
            label,
            meta.len(),
            FileEnds::Push {
                src,
                len: meta.len(),
                mtime: meta.modified().ok(),
                part: remote.root.join(&part_path(dst)?),
                dst: remote.root.join(dst),
            },
        ))
    }

    fn pull(
        label: &str,
        dst: RelPath,
        src: String,
        size: u64,
        mtime: Option<u64>,
    ) -> Result<Self, TransferError> {
        Ok(Self::new(
            label.to_owned(),
            size,
            FileEnds::Pull {
                src,
                mtime,
                part: part_path(&dst)?,
                dst,
            },
        ))
    }

    fn new(label: String, size: u64, ends: FileEnds) -> Self {
        Self {
            label,
            size,
            ends,
            remaining: AtomicU64::new(0),
            chunk_size: 0,
            journal_id: None,
            replace: false,
            failed: AtomicBool::new(false),
            part_created: AtomicBool::new(false),
            distrust: AtomicBool::new(false),
            check: OnceCell::new(),
            local: OnceCell::new(),
        }
    }

    /// Journal key and source identity: the destination path and the source mtime (nanoseconds
    /// for a local source, seconds for a remote one).
    fn journal_identity(&self) -> (String, Option<i64>) {
        match &self.ends {
            FileEnds::Push { dst, mtime, .. } => (
                dst.clone(),
                mtime
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .and_then(|d| i64::try_from(d.as_nanos()).ok()),
            ),
            FileEnds::Pull { dst, mtime, .. } => (
                dst.to_slash_string(),
                mtime.and_then(|m| i64::try_from(m).ok()),
            ),
        }
    }

    /// The remote path every worker opens: the part file (push) or the source (pull).
    fn remote_path(&self) -> &str {
        match &self.ends {
            FileEnds::Push { part, .. } => part,
            FileEnds::Pull { src, .. } => src,
        }
    }
}

struct Item {
    file: Arc<FileState>,
    /// Chunk index for the journal. An empty range only finalizes (every chunk was resumed).
    index: u64,
    range: Range,
    /// Made durable by an earlier run: only verified, not sent again.
    resumed: bool,
}

/// One lane: its current session, and whether it ever connected.
struct Slot {
    state: tokio::sync::Mutex<SlotState>,
    /// Connected at least once in this transfer. Only a lane that never did may be retired.
    up: AtomicBool,
}

struct SlotState {
    /// Unique across lanes: tells a worker whether the session it saw fail was already replaced,
    /// and keeps cached handles from being used on another lane's session.
    generation: u64,
    sftp: Option<Sftp>,
    /// The server refused this lane while others worked: its workers use other lanes.
    retired: bool,
}

/// SSH handshakes in flight at once per transfer. sshd drops unauthenticated connections past
/// `MaxStartups` (default 10, often lower on shared boxes).
const CONNECTING: usize = 2;

/// How often auto-tune measures throughput.
const TUNE_INTERVAL: Duration = Duration::from_secs(2);

/// Errors after which a never-connected lane is retired instead of failing the transfer, as long as
/// another lane is up. Host key and key file problems stay fatal: a second connection seeing a
/// different host key than the first is exactly the attack pinning exists to stop.
fn may_retire(e: &TransferError) -> bool {
    !matches!(
        e,
        TransferError::Remote(SftpError::Ssh(
            SshError::HostKeyMismatch { .. }
                | SshError::UnknownHost { .. }
                | SshError::UnsupportedHostKey { .. }
                | SshError::KeyFile { .. }
        ))
    )
}

struct Shared {
    dir: Direction,
    /// Journal job key: direction, account, and both roots.
    job: String,
    remote: Remote,
    local: LocalRoot,
    opts: TransferOptions,
    pool: BufferPool,
    piece: usize,
    slots: Vec<Slot>,
    /// Lanes that have connected at least once.
    lanes_up: AtomicUsize,
    generations: AtomicU64,
    connecting: Semaphore,
    /// Bytes moved, for auto-tune.
    moved: AtomicU64,
    /// Items queued and not yet finished.
    pending: AtomicU64,
    producer_done: AtomicBool,
    /// Set once the producer is done and every item is finished (or the transfer aborted): lanes
    /// still in their handshake are no longer needed.
    finished: watch::Sender<bool>,
    retries_left: AtomicU32,
    retries: AtomicU32,
    abort: AtomicBool,
    report: Mutex<TreeReport>,
    /// Destination bytes still free above `min_free`, less what queued files will use. None: the
    /// destination cannot tell (no `statvfs@`), so no check.
    space: Mutex<Option<u64>>,
}

impl Shared {
    fn depth(&self) -> usize {
        self.opts.sftp.inflight.max(1)
    }

    fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }

    /// A session for the worker on lane `slot`. When that lane is retired, the worker moves to a
    /// live one and `slot` is updated.
    async fn lane(&self, slot: &mut usize) -> Result<(u64, Sftp), TransferError> {
        loop {
            match self.session(*slot).await? {
                Some(s) => return Ok(s),
                None => *slot = self.live_slot(*slot),
            }
        }
    }

    /// The next lane after `from` that has connected. Lane 0 always has.
    fn live_slot(&self, from: usize) -> usize {
        let n = self.slots.len();
        (1..=n)
            .map(|i| (from + i) % n)
            .find(|&i| self.slots[i].up.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// The lane's session, connecting it if needed. Transient connect failures spend retries,
    /// except on a lane that never connected while another is up: that lane is retired (None).
    async fn session(&self, slot: usize) -> Result<Option<(u64, Sftp)>, TransferError> {
        let lane = &self.slots[slot];
        let mut st = lane.state.lock().await;
        if st.retired {
            return Ok(None);
        }
        if let Some(s) = &st.sftp {
            return Ok(Some((st.generation, s.clone())));
        }
        let mut failures = 0;
        loop {
            let connected = {
                // The semaphore lives as long as `self` and is never closed.
                let _permit = self.connecting.acquire().await.ok();
                connect_session(&self.remote, &self.opts).await
            };
            match connected {
                Ok(s) => {
                    st.generation = self.generations.fetch_add(1, Ordering::Relaxed) + 1;
                    st.sftp = Some(s.clone());
                    if !lane.up.swap(true, Ordering::AcqRel) {
                        self.lanes_up.fetch_add(1, Ordering::Relaxed);
                    }
                    return Ok(Some((st.generation, s)));
                }
                Err(e)
                    if !lane.up.load(Ordering::Acquire)
                        && self.lanes_up.load(Ordering::Relaxed) > 0
                        && may_retire(&e) =>
                {
                    st.retired = true;
                    return Ok(None);
                }
                Err(e) if is_transient(&e) => {
                    self.spend_retry(e)?;
                    failures += 1;
                    tokio::time::sleep(backoff(failures)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Drops the lane's session if it is still the one that failed.
    async fn invalidate(&self, slot: usize, generation: u64) {
        let mut st = self.slots[slot].state.lock().await;
        if st.generation == generation {
            st.sftp = None;
        }
    }

    /// Runs a metadata operation on lane 0, reconnecting on transient failures.
    async fn remote_op<T, F, Fut>(&self, f: F) -> Result<T, TransferError>
    where
        F: Fn(Sftp) -> Fut,
        Fut: Future<Output = Result<T, SftpError>>,
    {
        let mut failures = 0;
        loop {
            let mut slot = 0;
            let (generation, sftp) = self.lane(&mut slot).await?;
            match f(sftp).await.map_err(TransferError::from) {
                Err(e) if is_transient(&e) => {
                    self.invalidate(slot, generation).await;
                    self.spend_retry(e)?;
                    failures += 1;
                    tokio::time::sleep(backoff(failures)).await;
                }
                other => return other,
            }
        }
    }

    /// Spends one retry, or fails with `last` when none are left.
    fn spend_retry(&self, last: TransferError) -> Result<(), TransferError> {
        let ok = self
            .retries_left
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok();
        if !ok {
            self.abort.store(true, Ordering::Relaxed);
            return Err(match last {
                TransferError::Remote(e) => TransferError::RetriesExhausted(e),
                other => other,
            });
        }
        self.retries.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// One item is finished. Both this and `producer_finished` check the other's flag after setting
    /// their own (SeqCst), so whichever comes last sets `finished`.
    fn item_done(&self) {
        if self.pending.fetch_sub(1, Ordering::SeqCst) == 1
            && self.producer_done.load(Ordering::SeqCst)
        {
            self.finished.send_replace(true);
        }
    }

    fn producer_finished(&self) {
        self.producer_done.store(true, Ordering::SeqCst);
        if self.pending.load(Ordering::SeqCst) == 0 {
            self.finished.send_replace(true);
        }
    }

    fn progress(&self, n: u64) {
        self.moved.fetch_add(n, Ordering::Relaxed);
        if let Some(p) = &self.opts.progress {
            p(n);
        }
    }

    fn problem(&self, path: String, p: Problem) {
        self.opts.item(&path, ItemOutcome::Problem(&p));
        if let Ok(mut r) = self.report.lock() {
            r.problems.push((path, p));
        }
    }

    fn count_dir(&self) {
        if let Ok(mut r) = self.report.lock() {
            r.dirs += 1;
        }
    }

    /// Marks the file failed (once) and reports why.
    fn fail_file(&self, file: &FileState, e: TransferError) {
        if !file.failed.swap(true, Ordering::Relaxed) {
            let p = match e {
                TransferError::DestinationExists(_) => Problem::Exists,
                e => Problem::Failed(e),
            };
            self.problem(file.label.clone(), p);
        }
    }

    fn file_done(&self, file: &FileState) {
        let dst = match &file.ends {
            FileEnds::Push { dst, .. } => dst.clone(),
            FileEnds::Pull { dst, .. } => dst.to_slash_string(),
        };
        let sent = ItemOutcome::Sent {
            bytes: file.size,
            dst: &dst,
        };
        self.opts.item(&file.label, sent);
        if let Ok(mut r) = self.report.lock() {
            r.files += 1;
            r.bytes += file.size;
            if self.opts.verify {
                r.verified += file.size;
            }
        }
    }

    /// Queues the chunks of `file` that are not already durable from an earlier run. Returns false
    /// once the workers are gone.
    async fn enqueue(&self, tx: &mpsc::Sender<Item>, mut file: FileState) -> bool {
        let plan = plan_for(&self.opts);
        file.chunk_size = plan::chunk_size(file.size, &plan);
        let mut done = HashSet::new();
        if let Some(j) = &self.opts.journal
            && plan::chunk_count(file.size, &plan) > 1
        {
            let (key, mtime) = file.journal_identity();
            // A journal that cannot be read or written only costs resumability, not the transfer.
            if let Ok(r) = j.begin(&self.job, &key, file.size, mtime, file.chunk_size) {
                file.journal_id = Some(r.id);
                file.chunk_size = r.chunk_size;
                done = r.done;
                if !done.is_empty() && !self.part_holds(&file, &done).await {
                    let _ = j.reset(r.id);
                    done.clear();
                }
            }
        }
        // Chunks are counted and produced one at a time, never collected: the size may come from
        // a remote that claims exabytes, and the space check below must get to refuse it.
        let count = plan::count_with(file.size, file.chunk_size);
        done.retain(|&i| i < count);
        let resumed: u64 = done
            .iter()
            .map(|&i| plan::chunk_with(file.size, file.chunk_size, i).len)
            .sum();
        if let Ok(mut space) = self.space.lock()
            && let Some(left) = space.as_mut()
        {
            let need = file.size - resumed;
            if need > *left {
                let e = TransferError::NoSpace {
                    path: file.label.clone(),
                    need,
                    available: *left,
                };
                drop(space);
                self.problem(file.label.clone(), Problem::Failed(e));
                return true;
            }
            *left -= need;
        }
        if resumed > 0 {
            // The part already exists and holds those chunks: never recreate it.
            file.part_created.store(true, Ordering::Relaxed);
            if let Ok(mut r) = self.report.lock() {
                r.resumed += resumed;
            }
        }
        // With verify, resumed chunks are queued too, to be read back and checked.
        let verify = self.opts.verify;
        let queued = if verify {
            count
        } else {
            count - done.len() as u64
        };
        file.remaining.store(queued.max(1), Ordering::Relaxed);
        let file = Arc::new(file);
        let chunks = (0u64..)
            .zip(plan::chunks_with(file.size, file.chunk_size))
            .map(|(i, r)| (i, r, done.contains(&i)))
            .filter(|&(_, _, resumed)| verify || !resumed);
        // Every chunk resumed and nothing to verify: one empty item still finalizes the file.
        let finalize_only = (queued == 0).then_some((
            u64::MAX,
            Range {
                offset: file.size,
                len: 0,
            },
            false,
        ));
        for (index, range, resumed) in chunks.chain(finalize_only) {
            let item = Item {
                file: file.clone(),
                index,
                range,
                resumed,
            };
            self.pending.fetch_add(1, Ordering::SeqCst);
            // `finished` before the producer is done means the transfer aborted: the workers are
            // gone, and a full channel would block this send forever.
            let mut finished = self.finished.subscribe();
            let sent = tokio::select! {
                r = tx.send(item) => r.is_ok(),
                _ = finished.wait_for(|f| *f) => false,
            };
            if !sent {
                return false;
            }
        }
        true
    }

    /// Whether the part file left by an earlier run still covers the recorded chunks. A part that
    /// was deleted or truncated since must not be trusted.
    async fn part_holds(&self, file: &FileState, done: &HashSet<u64>) -> bool {
        let need = done
            .iter()
            .map(|&i| plan::chunk_with(file.size, file.chunk_size, i).end())
            .max()
            .unwrap_or(0);
        let have = match &file.ends {
            FileEnds::Push { part, .. } => self
                .remote_op(|s| {
                    let p = part.clone();
                    async move { s.lstat(&p).await }
                })
                .await
                .ok()
                .filter(|a| a.kind == RemoteKind::File)
                .map(|a| a.size),
            FileEnds::Pull { part, .. } => self
                .local
                .metadata(part)
                .ok()
                .filter(std::fs::Metadata::is_file)
                .map(|m| m.len()),
        };
        have.is_some_and(|h| h >= need)
    }

    /// Opens (once) the local side of `file`.
    async fn local_side<'f>(&self, file: &'f FileState) -> Result<&'f LocalSide, TransferError> {
        // Each FileState belongs to one Shared, so the cell is always filled by this method.
        file.local
            .get_or_try_init(|| async {
                match &file.ends {
                    FileEnds::Push { src, len, .. } => {
                        let f = self
                            .local
                            .open_file(src, OpenMode::Read)
                            .map_err(local_err(&file.label))?;
                        // The path could have been swapped since it was listed.
                        let m = f.metadata().map_err(local_err(&file.label))?;
                        if !m.is_file() {
                            return Err(TransferError::NotAFile(file.label.clone()));
                        }
                        if m.len() != *len {
                            return Err(TransferError::SourceChanged(file.label.clone()));
                        }
                        Ok(LocalSide::Reader(Arc::new(LocalReader::new(
                            f,
                            self.opts.cache,
                        ))))
                    }
                    FileEnds::Pull { part, dst, .. } => {
                        let f = self
                            .local
                            .open_file(part, OpenMode::WriteCreate)
                            .map_err(|e| {
                                if e.kind() == io::ErrorKind::NotFound {
                                    TransferError::MissingDirectory(parent_label(dst))
                                } else {
                                    local_err(&file.label)(e)
                                }
                            })?;
                        Ok(LocalSide::Writer(Arc::new(LocalWriter::new(
                            f,
                            self.opts.cache,
                        ))))
                    }
                }
            })
            .await
    }
}

/// Runs a transfer: connects lanes lazily (lane 0 reuses `first`), starts the workers and the
/// producer, and collects the report.
#[allow(clippy::too_many_arguments)]
async fn run<P, Fut>(
    dir: Direction,
    remote: &Remote,
    local: &LocalRoot,
    opts: &TransferOptions,
    (lanes, max_lanes): (usize, usize),
    per_lane: usize,
    first: Sftp,
    produce: P,
) -> Result<TreeReport, TransferError>
where
    P: FnOnce(Arc<Shared>, mpsc::Sender<Item>) -> Fut,
    Fut: Future<Output = Result<(), TransferError>> + Send + 'static,
{
    let lanes = lanes.max(1);
    let max_lanes = max_lanes.max(lanes);
    let piece = piece_len(match dir {
        Direction::Push => first.max_write(),
        Direction::Pull => first.max_read(),
    });
    let depth = opts.sftp.inflight.max(1);
    let slots = (0..max_lanes)
        .map(|i| Slot {
            state: tokio::sync::Mutex::new(SlotState {
                generation: if i == 0 { 1 } else { 0 },
                sftp: (i == 0).then(|| first.clone()),
                retired: false,
            }),
            up: AtomicBool::new(i == 0),
        })
        .collect();
    let space = match dir {
        Direction::Push => first.available_bytes(remote.root.as_str()).await?,
        Direction::Pull => Some(
            local
                .available_bytes()
                .map_err(local_err(&local.path().display().to_string()))?,
        ),
    };
    let local_root =
        std::fs::canonicalize(local.path()).unwrap_or_else(|_| local.path().to_owned());
    let job = format!(
        "{}|{}@{}|{}|{}",
        match dir {
            Direction::Push => "push",
            Direction::Pull => "pull",
        },
        remote.target.user,
        remote.target.host,
        remote.root.as_str(),
        local_root.display()
    );
    let shared = Arc::new(Shared {
        dir,
        job,
        remote: remote.clone(),
        local: local
            .try_clone()
            .map_err(local_err(&local.path().display().to_string()))?,
        opts: opts.clone(),
        // Sized for the ceiling; buffers are allocated on first use.
        pool: BufferPool::new(max_lanes * depth, piece),
        piece,
        slots,
        lanes_up: AtomicUsize::new(1),
        generations: AtomicU64::new(1),
        connecting: Semaphore::new(CONNECTING),
        moved: AtomicU64::new(0),
        pending: AtomicU64::new(0),
        producer_done: AtomicBool::new(false),
        finished: watch::channel(false).0,
        retries_left: AtomicU32::new(opts.retries),
        retries: AtomicU32::new(0),
        abort: AtomicBool::new(false),
        report: Mutex::new(TreeReport::default()),
        space: Mutex::new(space.map(|a| a.saturating_sub(opts.min_free))),
    });
    drop(first);

    let (tx, rx) = mpsc::channel(max_lanes * per_lane * 2);
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let mut set = JoinSet::new();
    for w in 0..lanes * per_lane {
        set.spawn(worker(shared.clone(), w % lanes, rx.clone()));
    }
    {
        let (s, producing) = (shared.clone(), produce(shared.clone(), tx));
        set.spawn(async move {
            let r = producing.await;
            s.producer_finished();
            r
        });
    }

    // Auto-tune: one sample per interval; the interval after a lane is added only lets it connect.
    let mut tuner = Tuner::new(lanes, max_lanes);
    let mut tick = tokio::time::interval(TUNE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    let (mut since, mut moved_then, mut warming) = (Instant::now(), 0u64, true);
    let mut first_err = None;
    let mut cancel = opts.cancel.clone();
    loop {
        let joined = tokio::select! {
            joined = set.join_next() => match joined {
                Some(j) => j,
                None => break,
            },
            () = cancelled(&mut cancel), if first_err.is_none() => {
                // Dropping the tasks drops their requests mid-flight. That is safe: data only
                // reaches the part file, and the journal records a chunk only once it is durable.
                shared.abort.store(true, Ordering::Relaxed);
                shared.finished.send_replace(true);
                set.abort_all();
                first_err = Some(TransferError::Cancelled);
                continue;
            }
            _ = tick.tick(), if !tuner.settled() && first_err.is_none() => {
                let moved = shared.moved.load(Ordering::Relaxed);
                let rate = (moved - moved_then) as f64 / since.elapsed().as_secs_f64();
                (since, moved_then) = (Instant::now(), moved);
                if std::mem::take(&mut warming) {
                    continue;
                }
                if tuner.observe(rate) {
                    let slot = tuner.active() - 1;
                    for _ in 0..per_lane {
                        set.spawn(worker(shared.clone(), slot, rx.clone()));
                    }
                    warming = true;
                }
                continue;
            }
        };
        let result = joined.unwrap_or_else(|e| {
            Err(TransferError::Local {
                path: local.path().display().to_string(),
                source: io::Error::other(format!("transfer task failed: {e}")),
            })
        });
        if let Err(e) = result
            && first_err.is_none()
        {
            shared.abort.store(true, Ordering::Relaxed);
            shared.finished.send_replace(true);
            first_err = Some(e);
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    let mut report = shared
        .report
        .lock()
        .map(|mut r| std::mem::take(&mut *r))
        .unwrap_or_default();
    report.retries = shared.retries.load(Ordering::Relaxed);
    report.lanes = u32::try_from(shared.lanes_up.load(Ordering::Relaxed)).unwrap_or(u32::MAX);
    if dir == Direction::Pull && report.files > 0 && !opts.dry_run {
        let root = local
            .try_clone()
            .map_err(local_err(&local.path().display().to_string()))?;
        let path = local.path().display().to_string();
        blocking(move || root.sync_fs())
            .await
            .map_err(local_err(&path))?;
    }
    Ok(report)
}

/// Resolves once `cancel` is set; never, when there is no cancel signal or its sender is gone.
async fn cancelled(cancel: &mut Option<watch::Receiver<bool>>) {
    if let Some(rx) = cancel
        && rx.wait_for(|c| *c).await.is_ok()
    {
        return;
    }
    std::future::pending().await
}

// Tree walks (producers).

/// A destination directory's files before the transfer, for the policy. Empty for a directory the
/// walk just created.
type Listing = HashMap<String, Found>;

/// Applies the policy to one source file headed for `dst_dir/name` and queues it. `taken` holds
/// every name already used in that directory (existing entries and source names), for `Rename`.
/// Returns false once the workers are gone.
#[allow(clippy::too_many_arguments)]
async fn place_file(
    shared: &Shared,
    tx: &mpsc::Sender<Item>,
    label: String,
    dst_dir: &RelPath,
    name: &Name,
    src: (u64, Option<u64>),
    existing: &Listing,
    taken: &mut HashSet<String>,
    build: impl FnOnce(RelPath) -> Result<FileState, TransferError>,
) -> bool {
    let found = existing.get(name.as_str()).copied();
    let (dst, replace) = match policy::decide(shared.opts.existing, src.0, src.1, found) {
        Decision::Create => (dst_dir.join(name.clone()), false),
        Decision::Replace => (dst_dir.join(name.clone()), true),
        Decision::Unchanged => {
            shared.opts.item(&label, ItemOutcome::Unchanged);
            if let Ok(mut r) = shared.report.lock() {
                r.unchanged += 1;
            }
            return true;
        }
        Decision::KeepExisting => {
            shared.problem(label, Problem::Exists);
            return true;
        }
        Decision::KeepNewer => {
            shared.problem(label, Problem::Newer);
            return true;
        }
        Decision::NotAFile => {
            shared.problem(
                label.clone(),
                Problem::Failed(TransferError::NotAFile(label)),
            );
            return true;
        }
        Decision::WriteBeside => {
            match policy::free_name(name.as_str(), taken).and_then(|n| Name::new(&n).ok()) {
                Some(n) => {
                    taken.insert(n.as_str().to_owned());
                    (dst_dir.join(n), false)
                }
                None => {
                    shared.problem(label, Problem::Exists);
                    return true;
                }
            }
        }
    };
    if shared.opts.dry_run {
        let to = match shared.dir {
            Direction::Push => shared.remote.root.join(&dst),
            Direction::Pull => dst.to_slash_string(),
        };
        let planned = ItemOutcome::Planned {
            bytes: src.0,
            dst: &to,
            replace,
        };
        shared.opts.item(&label, planned);
        if let Ok(mut r) = shared.report.lock() {
            r.files += 1;
            r.bytes += src.0;
        }
        return true;
    }
    match build(dst) {
        Ok(mut f) => {
            f.replace = replace;
            shared.enqueue(tx, f).await
        }
        Err(e) => {
            shared.problem(label, Problem::Failed(e));
            true
        }
    }
}

async fn walk_local(
    shared: Arc<Shared>,
    tx: mpsc::Sender<Item>,
    src: RelPath,
    dst: RelPath,
) -> Result<(), TransferError> {
    let mut stack = vec![(src, dst, String::new(), true)];
    while let Some((s, d, label, top)) = stack.pop() {
        if shared.aborted() {
            break;
        }
        let entries = {
            let (shared, s) = (shared.clone(), s.clone());
            blocking(move || shared.local.read_dir(&s)).await
        };
        let mut entries = match entries {
            Ok(e) => e,
            Err(e) => {
                let label = if top { ".".to_owned() } else { label };
                shared.problem(label.clone(), Problem::Failed(local_err(&label)(e)));
                continue;
            }
        };
        let remote_dir = shared.remote.root.join(&d);
        let created = if top {
            false
        } else if shared.opts.dry_run {
            shared.count_dir();
            false
        } else {
            let made = shared.remote_op(|s| {
                let p = remote_dir.clone();
                async move { s.mkdir(&p).await }
            });
            match made.await {
                Ok(created) => {
                    shared.count_dir();
                    created
                }
                Err(e) if is_fatal(&e) => return Err(e),
                Err(e) => {
                    shared.problem(label, Problem::Failed(e));
                    continue;
                }
            }
        };
        let existing = if created {
            Listing::new()
        } else {
            match remote_listing(&shared, &remote_dir).await {
                // A dry run does not create directories, so they may not exist yet.
                Err(TransferError::Remote(SftpError::NotFound(_))) if shared.opts.dry_run => {
                    Listing::new()
                }
                other => other?,
            }
        };
        let mut taken: HashSet<String> = existing.keys().cloned().collect();
        taken.extend(
            entries
                .iter()
                .filter_map(|e| e.name.to_str().map(str::to_owned)),
        );
        entries.sort_by(|a, b| b.name.cmp(&a.name));
        let mut subdirs = Vec::new();
        for entry in entries {
            let raw = entry.name.to_string_lossy().into_owned();
            let entry_label = join_label(&label, &raw);
            let Some(name) = entry.name.to_str().and_then(|n| Name::new(n).ok()) else {
                shared.problem(entry_label, Problem::InvalidName);
                continue;
            };
            let s = s.join(name.clone());
            match entry.kind {
                EntryKind::Symlink => shared.problem(entry_label, Problem::Symlink),
                EntryKind::Other => shared.problem(entry_label, Problem::Special),
                EntryKind::Dir => subdirs.push((s, d.join(name), entry_label, false)),
                EntryKind::File if name.as_str().ends_with(PART_SUFFIX) => {
                    shared.problem(entry_label, Problem::PartFile);
                }
                EntryKind::File => {
                    let meta = {
                        let (shared, s) = (shared.clone(), s.clone());
                        blocking(move || shared.local.metadata(&s)).await
                    };
                    let meta = match meta {
                        Ok(m) => m,
                        Err(e) => {
                            shared.problem(
                                entry_label.clone(),
                                Problem::Failed(local_err(&entry_label)(e)),
                            );
                            continue;
                        }
                    };
                    let src_id = (meta.len(), meta.modified().ok().map(unix_secs));
                    let build = |dst: RelPath| {
                        FileState::push(&shared.remote, s, &dst, entry_label.clone(), &meta)
                    };
                    let label = entry_label.clone();
                    if !place_file(
                        &shared, &tx, label, &d, &name, src_id, &existing, &mut taken, build,
                    )
                    .await
                    {
                        return Ok(());
                    }
                }
            }
        }
        stack.extend(subdirs);
    }
    Ok(())
}

/// A remote directory's entries as policy input.
async fn remote_listing(shared: &Shared, dir: &str) -> Result<Listing, TransferError> {
    let listed = shared
        .remote_op(|s| {
            let p = dir.to_owned();
            async move { s.read_dir(&p).await }
        })
        .await?;
    Ok(listed
        .into_iter()
        .map(|(n, a)| {
            let found = Found {
                size: a.size,
                mtime: a.mtime,
                is_file: a.kind == RemoteKind::File,
            };
            (n, found)
        })
        .collect())
}

/// A local directory's entries as policy input (one `lstat` per entry, symlinks not followed).
fn local_listing(local: &LocalRoot, dir: &RelPath) -> io::Result<Listing> {
    let mut out = Listing::new();
    for entry in local.read_dir(dir)? {
        let Some(name) = entry.name.to_str().and_then(|n| Name::new(n).ok()) else {
            continue;
        };
        let m = local.metadata(&dir.join(name.clone()))?;
        out.insert(
            name.as_str().to_owned(),
            Found {
                size: m.len(),
                mtime: m.modified().ok().map(unix_secs),
                is_file: m.is_file(),
            },
        );
    }
    Ok(out)
}

async fn walk_remote(
    shared: Arc<Shared>,
    tx: mpsc::Sender<Item>,
    src: RelPath,
    dst: RelPath,
) -> Result<(), TransferError> {
    let mut stack = vec![(src, dst, String::new(), true)];
    while let Some((s, d, label, top)) = stack.pop() {
        if shared.aborted() {
            break;
        }
        let remote_dir = shared.remote.root.join(&s);
        let listed = shared.remote_op(|sftp| {
            let p = remote_dir.clone();
            async move { sftp.read_dir(&p).await }
        });
        let mut entries = match listed.await {
            Ok(e) => e,
            Err(e) if is_fatal(&e) => return Err(e),
            Err(e) => {
                shared.problem(if top { ".".to_owned() } else { label }, Problem::Failed(e));
                continue;
            }
        };
        let created = if top || d.components().is_empty() {
            false
        } else if shared.opts.dry_run {
            shared.count_dir();
            false
        } else {
            let (shared2, d2) = (shared.clone(), d.clone());
            match blocking(move || shared2.local.create_dir(&d2)).await {
                Ok(created) => {
                    shared.count_dir();
                    created
                }
                Err(e) => {
                    shared.problem(label.clone(), Problem::Failed(local_err(&label)(e)));
                    continue;
                }
            }
        };
        let existing = if created {
            Listing::new()
        } else {
            let (shared2, d2) = (shared.clone(), d.clone());
            match blocking(move || local_listing(&shared2.local, &d2)).await {
                Ok(l) => l,
                // A dry run does not create directories, so they may not exist yet.
                Err(e) if e.kind() == io::ErrorKind::NotFound && shared.opts.dry_run => {
                    Listing::new()
                }
                Err(e) => {
                    shared.problem(label.clone(), Problem::Failed(local_err(&label)(e)));
                    continue;
                }
            }
        };
        let mut taken: HashSet<String> = existing.keys().cloned().collect();
        taken.extend(entries.iter().map(|(n, _)| n.clone()));
        entries.sort_by(|a, b| b.0.cmp(&a.0));
        let mut subdirs = Vec::new();
        for (raw, attrs) in entries {
            let entry_label = join_label(&label, &raw);
            let Ok(name) = Name::new(&raw) else {
                shared.problem(entry_label, Problem::InvalidName);
                continue;
            };
            let s = s.join(name.clone());
            match attrs.kind {
                RemoteKind::Symlink => shared.problem(entry_label, Problem::Symlink),
                RemoteKind::Other => shared.problem(entry_label, Problem::Special),
                RemoteKind::Dir => subdirs.push((s, d.join(name), entry_label, false)),
                RemoteKind::File if name.as_str().ends_with(PART_SUFFIX) => {
                    shared.problem(entry_label, Problem::PartFile);
                }
                RemoteKind::File => {
                    let src_path = shared.remote.root.join(&s);
                    let build = |dst: RelPath| {
                        FileState::pull(&entry_label, dst, src_path, attrs.size, attrs.mtime)
                    };
                    let src_id = (attrs.size, attrs.mtime);
                    let label = entry_label.clone();
                    if !place_file(
                        &shared, &tx, label, &d, &name, src_id, &existing, &mut taken, build,
                    )
                    .await
                    {
                        return Ok(());
                    }
                }
            }
        }
        stack.extend(subdirs);
    }
    Ok(())
}

// Workers.

/// The worker's open handle on the file it last worked on.
struct Cached {
    file: Arc<FileState>,
    generation: u64,
    handle: Arc<RemoteFile>,
}

impl Cached {
    async fn close(self) {
        // Close errors after the data is in are not actionable; the session ends with the lane.
        if let Ok(f) = Arc::try_unwrap(self.handle) {
            let _ = f.close().await;
        }
    }
}

async fn worker(
    shared: Arc<Shared>,
    mut slot: usize,
    rx: Arc<tokio::sync::Mutex<mpsc::Receiver<Item>>>,
) -> Result<(), TransferError> {
    let mut cache: Option<Cached> = None;
    let mut failures = 0u32;
    let result = loop {
        if shared.aborted() {
            break Ok(());
        }
        // Connect this worker's lane before taking work, so no item waits on a handshake, and give
        // the handshake up once all the work is done by other lanes. Errors other than fatal ones
        // are met again, and reported, by the item that needs the lane.
        let mut finished = shared.finished.subscribe();
        let connected = tokio::select! {
            biased;
            _ = finished.wait_for(|f| *f) => break Ok(()),
            r = shared.lane(&mut slot) => r,
        };
        if let Err(e) = connected
            && is_fatal(&e)
        {
            break Err(e);
        }
        let Some(item) = rx.lock().await.recv().await else {
            break Ok(());
        };
        let file = &item.file;
        if !file.failed.load(Ordering::Relaxed) {
            match transfer_with_retry(&shared, &mut slot, &item, &mut cache, &mut failures).await {
                Ok(()) => {}
                Err(e) if is_fatal(&e) => break Err(e),
                Err(e) => shared.fail_file(file, e),
            }
        }
        // Exactly one decrement per item, after its data is written: whoever takes the count to
        // zero holds the file's last chunk and finalizes it.
        if file.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            if !file.failed.load(Ordering::Relaxed) {
                let result =
                    finalize_with_retry(&shared, &mut slot, file, &mut cache, &mut failures).await;
                if let (Some(j), Some(id)) = (&shared.opts.journal, file.journal_id)
                    && matches!(result, Ok(()) | Err(TransferError::SourceChanged(_)))
                {
                    let _ = j.forget(id);
                }
                match result {
                    Ok(()) => shared.file_done(file),
                    Err(e) if is_fatal(&e) => break Err(e),
                    Err(e) => shared.fail_file(file, e),
                }
            } else if file.distrust.load(Ordering::Relaxed)
                && let (Some(j), Some(id)) = (&shared.opts.journal, file.journal_id)
            {
                // Every item of the file is done, so no chunk can be recorded after this.
                let _ = j.reset(id);
            }
            if let Some(c) = cache.take_if(|c| Arc::ptr_eq(&c.file, file)) {
                c.close().await;
            }
        }
        shared.item_done();
    };
    if result.is_err() {
        shared.abort.store(true, Ordering::Relaxed);
    }
    if let Some(c) = cache {
        c.close().await;
    }
    result
}

/// Transfers one chunk, reconnecting and redoing it on transient failures.
async fn transfer_with_retry(
    shared: &Arc<Shared>,
    slot: &mut usize,
    item: &Item,
    cache: &mut Option<Cached>,
    failures: &mut u32,
) -> Result<(), TransferError> {
    let side = shared.local_side(&item.file).await?;
    loop {
        let (generation, sftp) = shared.lane(slot).await?;
        let attempt = async {
            let handle = handle_for(shared, cache, &item.file, generation, &sftp).await?;
            if item.resumed {
                return verify_chunk(shared, side, &handle, &item.file, item.range, None).await;
            }
            let hash = transfer_chunk(shared, side, &handle, &item.file, item.range).await?;
            if shared.opts.verify {
                verify_chunk(shared, side, &handle, &item.file, item.range, hash).await?;
            }
            record_durable(shared, item, side, &handle).await
        };
        match attempt.await {
            Err(e) if is_transient(&e) => {
                *cache = None;
                shared.invalidate(*slot, generation).await;
                shared.spend_retry(e)?;
                *failures += 1;
                tokio::time::sleep(backoff(*failures)).await;
            }
            other => return other,
        }
    }
}

/// Makes the chunk durable at the destination and records it in the journal. A server without
/// `fsync@` gets no records: its chunks are never claimed durable, so a resume redoes them.
async fn record_durable(
    shared: &Shared,
    item: &Item,
    side: &LocalSide,
    handle: &RemoteFile,
) -> Result<(), TransferError> {
    let (Some(journal), Some(id)) = (&shared.opts.journal, item.file.journal_id) else {
        return Ok(());
    };
    if item.range.len == 0 {
        return Ok(());
    }
    match side {
        LocalSide::Reader(_) => match handle.fsync().await {
            Ok(()) => {}
            Err(SftpError::Unsupported(_)) => return Ok(()),
            Err(e) => return Err(e.into()),
        },
        LocalSide::Writer(w) => {
            let w = w.clone();
            blocking(move || w.sync_data())
                .await
                .map_err(local_err(&item.file.label))?;
        }
    }
    // Losing this record costs a redone chunk on resume, nothing more.
    let _ = journal.mark_done(id, item.index);
    Ok(())
}

/// Finalizes a file, retrying transient failures. A retry after a push rename whose reply was lost
/// finds the part gone and the destination complete, and stops there: reopening the part would
/// recreate it empty and rename zeros over the good file.
async fn finalize_with_retry(
    shared: &Arc<Shared>,
    slot: &mut usize,
    file: &Arc<FileState>,
    cache: &mut Option<Cached>,
    failures: &mut u32,
) -> Result<(), TransferError> {
    let side = shared.local_side(file).await?;
    let mut retried = false;
    loop {
        let (generation, sftp) = shared.lane(slot).await?;
        let attempt = async {
            if retried
                && let FileEnds::Push { part, dst, .. } = &file.ends
                && matches!(sftp.lstat(part).await, Err(SftpError::NotFound(_)))
            {
                return match sftp.lstat(dst).await {
                    Ok(a) if a.size == file.size => Ok(()),
                    Ok(_) | Err(SftpError::NotFound(_)) => {
                        Err(TransferError::Remote(SftpError::NotFound(part.clone())))
                    }
                    Err(e) => Err(e.into()),
                };
            }
            let handle = handle_for(shared, cache, file, generation, &sftp).await?;
            finalize(shared, file, &sftp, &handle, side).await
        };
        match attempt.await {
            Err(e) if is_transient(&e) => {
                *cache = None;
                shared.invalidate(*slot, generation).await;
                shared.spend_retry(e)?;
                *failures += 1;
                retried = true;
                tokio::time::sleep(backoff(*failures)).await;
            }
            other => return other,
        }
    }
}

/// The worker's handle on `file` for this session, opening it if the cache holds another. Only the
/// first open of a push's part file may create it: a part that disappears mid-transfer must fail
/// the file, not be recreated with holes where earlier chunks were.
async fn handle_for(
    shared: &Shared,
    cache: &mut Option<Cached>,
    file: &Arc<FileState>,
    generation: u64,
    sftp: &Sftp,
) -> Result<Arc<RemoteFile>, TransferError> {
    if let Some(c) = cache
        && Arc::ptr_eq(&c.file, file)
        && c.generation == generation
    {
        return Ok(c.handle.clone());
    }
    if let Some(old) = cache.take() {
        old.close().await;
    }
    let created = file.part_created.load(Ordering::Acquire);
    // Only verify reads a push's part: a write-only drop directory still works without it.
    let mode = match (shared.dir, created, shared.opts.verify) {
        (Direction::Push, true, false) => RemoteOpenMode::Write,
        (Direction::Push, false, false) => RemoteOpenMode::WriteCreate,
        (Direction::Push, true, true) => RemoteOpenMode::ReadWrite,
        (Direction::Push, false, true) => RemoteOpenMode::ReadWriteCreate,
        (Direction::Pull, ..) => RemoteOpenMode::Read,
    };
    let path = file.remote_path();
    let handle = match sftp.open(path, mode).await {
        Err(SftpError::NotFound(_)) if shared.dir == Direction::Push && !created => {
            let parent = path.rsplit_once('/').map_or(".", |(p, _)| p);
            return Err(TransferError::MissingDirectory(parent.to_owned()));
        }
        other => Arc::new(other?),
    };
    file.part_created.store(true, Ordering::Release);
    *cache = Some(Cached {
        file: file.clone(),
        generation,
        handle: handle.clone(),
    });
    Ok(handle)
}

/// Size, sync, mtime, source re-check, rename.
async fn finalize(
    shared: &Shared,
    file: &FileState,
    sftp: &Sftp,
    handle: &RemoteFile,
    side: &LocalSide,
) -> Result<(), TransferError> {
    let overwrite = file.replace;
    match (&file.ends, side) {
        (
            FileEnds::Push {
                src,
                len,
                mtime,
                part,
                dst,
            },
            _,
        ) => {
            handle.set_len(file.size).await?;
            if file.size >= FSYNC_MIN {
                match handle.fsync().await {
                    Ok(()) | Err(SftpError::Unsupported(_)) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            let now = shared.local.metadata(src).map_err(local_err(&file.label))?;
            if (now.len(), now.modified().ok()) != (*len, *mtime) {
                return Err(TransferError::SourceChanged(file.label.clone()));
            }
            if let Some(m) = mtime {
                sftp.set_mtime(part, unix_secs(*m)).await?;
            }
            sftp.rename(part, dst, overwrite)
                .await
                .map_err(|e| match e {
                    SftpError::Status { .. } if !overwrite => {
                        TransferError::DestinationExists(dst.clone())
                    }
                    e => e.into(),
                })
        }
        (
            FileEnds::Pull {
                src,
                mtime,
                part,
                dst,
            },
            LocalSide::Writer(writer),
        ) => {
            let now = sftp.lstat(src).await?;
            if (now.size, now.mtime) != (file.size, *mtime) {
                return Err(TransferError::SourceChanged(file.label.clone()));
            }
            let (writer, size, mtime) = (writer.clone(), file.size, *mtime);
            blocking(move || {
                let f = writer.file();
                f.set_len(size)?;
                if let Some(m) = mtime {
                    f.set_modified(UNIX_EPOCH + Duration::from_secs(m))?;
                }
                if size >= FSYNC_MIN {
                    f.sync_all()?;
                }
                Ok(())
            })
            .await
            .map_err(local_err(&file.label))?;
            shared
                .local
                .rename(part, dst, overwrite)
                .map_err(|e| match e.kind() {
                    io::ErrorKind::AlreadyExists => {
                        TransferError::DestinationExists(dst.to_slash_string())
                    }
                    _ => local_err(&file.label)(e),
                })
        }
        (FileEnds::Pull { .. }, LocalSide::Reader(_)) => {
            unreachable!("a pull's local side is always a writer")
        }
    }
}

fn piece_len(max: u32) -> usize {
    let max = max as usize;
    if max >= PAGE { max / PAGE * PAGE } else { max }
}

fn backoff(failures: u32) -> Duration {
    Duration::from_millis(200 << failures.min(5)).min(Duration::from_secs(10))
}

/// Aborts every task still in flight when dropped, so an error or a cancelled worker leaves nothing
/// running against a connection that is being torn down.
struct Pipeline<T>(VecDeque<JoinHandle<T>>);

impl<T> Drop for Pipeline<T> {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}

async fn join<T>(h: JoinHandle<Result<T, TransferError>>, what: &str) -> Result<T, TransferError> {
    h.await.unwrap_or_else(|e| {
        Err(TransferError::Local {
            path: what.to_owned(),
            source: io::Error::other(format!("task failed: {e}")),
        })
    })
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(io::Error::other(format!("blocking task failed: {e}"))))
}

/// Moves one chunk. With verify on, returns the xxh3-128 hash of the source bytes, computed as they
/// stream, so checking the destination needs no second read of the source.
async fn transfer_chunk(
    shared: &Arc<Shared>,
    side: &LocalSide,
    handle: &Arc<RemoteFile>,
    file: &FileState,
    range: Range,
) -> Result<Option<u128>, TransferError> {
    match side {
        LocalSide::Reader(reader) => push_chunk(shared, reader, handle, file, range).await,
        LocalSide::Writer(writer) => {
            pull_chunk(shared, writer.chunk(range.offset), handle, file, range).await
        }
    }
}

/// A buffer for the next piece, or None when the pool is empty and the pipeline should drain
/// first (waiting while holding buffers could deadlock workers that share the pool).
async fn next_buffer(shared: &Shared, pipeline_empty: bool) -> Option<crate::pool::Buffer> {
    match shared.pool.try_acquire() {
        Some(b) => Some(b),
        None if pipeline_empty => Some(shared.pool.acquire().await),
        None => None,
    }
}

async fn push_chunk(
    shared: &Arc<Shared>,
    reader: &Arc<LocalReader>,
    handle: &Arc<RemoteFile>,
    file: &FileState,
    range: Range,
) -> Result<Option<u128>, TransferError> {
    let mut hasher = shared.opts.verify.then(XxHash3_128::new);
    let mut pipe = Pipeline(VecDeque::new());
    let mut off = range.offset;
    while off < range.end() || !pipe.0.is_empty() {
        let room = off < range.end() && pipe.0.len() < shared.depth();
        let buf = if room {
            next_buffer(shared, pipe.0.is_empty()).await
        } else {
            None
        };
        if let Some(mut buf) = buf {
            let len = shared
                .piece
                .min(usize::try_from(range.end() - off).unwrap_or(usize::MAX));
            let (reader, handle, at, label) =
                (reader.clone(), handle.clone(), off, file.label.clone());
            pipe.0.push_back(tokio::spawn(async move {
                let read = blocking(move || {
                    let n = reader.read_at(at, &mut buf[..len])?;
                    Ok((buf, n))
                })
                .await;
                let (buf, n) = read.map_err(local_err(&label))?;
                if n != len {
                    return Err(TransferError::SourceChanged(label));
                }
                handle.write_at(at, &buf[..len]).await?;
                Ok((buf, len))
            }));
            off += len as u64;
        } else if let Some(h) = pipe.0.pop_front() {
            // Joined in order, so the hash sees the chunk's bytes in order.
            let (buf, len) = join(h, &file.label).await?;
            if let Some(h) = &mut hasher {
                h.write(&buf[..len]);
            }
            shared.progress(len as u64);
        }
    }
    Ok(hasher.map(|h| h.finish_128()))
}

async fn pull_chunk(
    shared: &Arc<Shared>,
    writer: ChunkWriter,
    handle: &Arc<RemoteFile>,
    file: &FileState,
    range: Range,
) -> Result<Option<u128>, TransferError> {
    let mut hasher = shared.opts.verify.then(XxHash3_128::new);
    let mut pipe = Pipeline(VecDeque::new());
    let mut writer = Some(writer);
    let mut off = range.offset;
    while off < range.end() || !pipe.0.is_empty() {
        let room = off < range.end() && pipe.0.len() < shared.depth();
        let buf = if room {
            next_buffer(shared, pipe.0.is_empty()).await
        } else {
            None
        };
        if let Some(mut buf) = buf {
            let len = shared
                .piece
                .min(usize::try_from(range.end() - off).unwrap_or(usize::MAX));
            let (handle, at, label) = (handle.clone(), off, file.label.clone());
            pipe.0.push_back(tokio::spawn(async move {
                let n = handle.read_at(at, &mut buf[..len]).await?;
                if n != len {
                    return Err(TransferError::SourceChanged(label));
                }
                Ok((buf, len))
            }));
            off += len as u64;
        } else if let Some(h) = pipe.0.pop_front() {
            let (buf, len) = join(h, &file.label).await?;
            if let Some(h) = &mut hasher {
                h.write(&buf[..len]);
            }
            let Some(mut w) = writer.take() else {
                unreachable!("the writer is put back after every piece");
            };
            let w = blocking(move || {
                w.write(&buf[..len])?;
                Ok(w)
            })
            .await
            .map_err(local_err(&file.label))?;
            writer = Some(w);
            shared.progress(len as u64);
        }
    }
    if let Some(w) = writer {
        blocking(move || w.finish())
            .await
            .map_err(local_err(&file.label))?;
    }
    Ok(hasher.map(|h| h.finish_128()))
}

/// Where `hash_range` reads from.
enum ReadFrom<'a> {
    Local(&'a Arc<LocalReader>),
    Remote(&'a Arc<RemoteFile>),
}

/// The xxh3-128 hash of `range`, read with the same pipelining and buffer pool as a transfer.
async fn hash_range(
    shared: &Shared,
    from: ReadFrom<'_>,
    file: &FileState,
    range: Range,
) -> Result<u128, TransferError> {
    let mut hasher = XxHash3_128::new();
    let mut pipe = Pipeline(VecDeque::new());
    let mut off = range.offset;
    while off < range.end() || !pipe.0.is_empty() {
        let room = off < range.end() && pipe.0.len() < shared.depth();
        let buf = if room {
            next_buffer(shared, pipe.0.is_empty()).await
        } else {
            None
        };
        if let Some(mut buf) = buf {
            let len = shared
                .piece
                .min(usize::try_from(range.end() - off).unwrap_or(usize::MAX));
            let (at, label) = (off, file.label.clone());
            let task: JoinHandle<Result<_, TransferError>> = match &from {
                ReadFrom::Local(r) => {
                    let r = Arc::clone(r);
                    tokio::spawn(async move {
                        let read = blocking(move || {
                            let n = r.read_at(at, &mut buf[..len])?;
                            Ok((buf, n))
                        });
                        read.await.map_err(local_err(&label))
                    })
                }
                ReadFrom::Remote(h) => {
                    let h = Arc::clone(h);
                    tokio::spawn(async move {
                        let n = h.read_at(at, &mut buf[..len]).await?;
                        Ok((buf, n))
                    })
                }
            };
            pipe.0.push_back(task);
            off += len as u64;
        } else if let Some(h) = pipe.0.pop_front() {
            let (buf, n) = join(h, &file.label).await?;
            // A short read is a difference too: the bytes that are missing.
            hasher.write(&buf[..n]);
        }
    }
    Ok(hasher.finish_128())
}

/// Reads `range` back from the destination and compares it with the source. `source` is the hash
/// computed while sending; None (a resumed chunk) reads the source again.
async fn verify_chunk(
    shared: &Shared,
    side: &LocalSide,
    handle: &Arc<RemoteFile>,
    file: &FileState,
    range: Range,
    source: Option<u128>,
) -> Result<(), TransferError> {
    if range.len == 0 {
        return Ok(());
    }
    let (src, dst) = match (side, &file.ends) {
        (LocalSide::Reader(r), _) => (ReadFrom::Local(r), ReadFrom::Remote(handle)),
        (LocalSide::Writer(_), FileEnds::Pull { part, .. }) => {
            let check = file
                .check
                .get_or_try_init(|| async {
                    let f = shared
                        .local
                        .open_file(part, OpenMode::Read)
                        .map_err(local_err(&file.label))?;
                    Ok::<_, TransferError>(Arc::new(LocalReader::new(f, shared.opts.cache)))
                })
                .await?;
            (ReadFrom::Remote(handle), ReadFrom::Local(check))
        }
        (LocalSide::Writer(_), FileEnds::Push { .. }) => {
            unreachable!("a push's local side is always a reader")
        }
    };
    let want = match source {
        Some(h) => h,
        None => hash_range(shared, src, file, range).await?,
    };
    if hash_range(shared, dst, file, range).await? != want {
        file.distrust.store(true, Ordering::Relaxed);
        return Err(TransferError::VerifyFailed {
            path: file.label.clone(),
            offset: range.offset,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).expect("rel")
    }

    #[test]
    fn part_path_is_a_sibling_with_suffix() {
        assert_eq!(
            part_path(&rel("a/b/E01.mkv")).expect("part"),
            rel("a/b/E01.mkv.hopsync-part")
        );
        assert!(part_path(&RelPath::default()).is_err());
    }

    #[test]
    fn long_names_get_short_distinct_part_names() {
        // 4-byte characters so the cut has to land on a char boundary.
        let base = "𝄞".repeat(60);
        let (a, b) = (format!("{base}aaaa-one"), format!("{base}aaaa-two"));
        assert_eq!(a.len(), 248);
        let pa = part_path(&rel(&a)).expect("part a");
        let pb = part_path(&rel(&b)).expect("part b");
        let (na, nb) = (pa.to_slash_string(), pb.to_slash_string());
        assert!(na.len() <= 255 && nb.len() <= 255);
        assert!(na.ends_with(PART_SUFFIX) && nb.ends_with(PART_SUFFIX));
        assert_ne!(na, nb, "distinct sources need distinct part files");
        assert_eq!(
            part_path(&rel(&a)).expect("again"),
            pa,
            "deterministic for resume"
        );
    }
}
