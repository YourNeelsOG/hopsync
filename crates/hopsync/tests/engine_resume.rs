//! Resume after an interrupted transfer (Phase 1 step 11).
//!
//! A transfer is interrupted by dropping its future once enough bytes are through: that aborts
//! every lane mid-flight, as a killed process would leave the destination. The CLI test at the end
//! does the same with SIGKILL on the real binary.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use hopsync::engine::{FileReport, Remote, TransferError, TransferOptions, pull_file, push_file};
use hopsync::journal::Journal;
use hopsync::paths::{LocalRoot, RelPath, RemoteRoot};
use hopsync::plan::PlanConfig;

const MIB: usize = 1 << 20;
const SIZE: usize = 64 * MIB;

struct Env {
    sshd: support::TestSshd,
    _pins: tempfile::TempDir,
    remote: Remote,
    local_dir: tempfile::TempDir,
    local: LocalRoot,
    journal: Arc<Journal>,
}

fn env() -> Option<Env> {
    let sshd = support::TestSshd::start()?;
    let (pins, known, target) = sshd.client();
    let remote_dir = sshd.dir().join("remote");
    std::fs::create_dir(&remote_dir).expect("mkdir remote");
    let root = RemoteRoot::parse(remote_dir.to_str().expect("utf8")).expect("remote root");
    let local_dir = tempfile::tempdir().expect("tempdir");
    let local = LocalRoot::open(local_dir.path()).expect("local root");
    let journal = Arc::new(Journal::open(&pins.path().join("journal.sqlite")).expect("journal"));
    Some(Env {
        remote: Remote::new(target, known, root),
        sshd,
        _pins: pins,
        local_dir,
        local,
        journal,
    })
}

impl Env {
    fn remote_path(&self, name: &str) -> PathBuf {
        self.sshd.dir().join("remote").join(name)
    }
    fn local_path(&self, name: &str) -> PathBuf {
        self.local_dir.path().join(name)
    }
    /// 8 chunks of 8 MiB over 2 lanes, journaled, each chunk its own durability checkpoint.
    fn opts(&self) -> TransferOptions {
        TransferOptions {
            lanes: 2,
            plan: PlanConfig {
                lanes: 2,
                min_chunk: 8 << 20,
                max_chunk: 8 << 20,
            },
            checkpoint: 8 << 20,
            journal: Some(self.journal.clone()),
            ..TransferOptions::default()
        }
    }
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).expect("rel")
}

fn write_source(path: &Path, seed: u8, mtime: u64) -> Vec<u8> {
    let data: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8 ^ seed).collect();
    std::fs::write(path, &data).expect("write");
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(UNIX_EPOCH + Duration::from_secs(mtime)))
        .expect("mtime");
    data
}

/// Runs the transfer until `fraction` of the bytes are through, then drops it. With `durable_in`,
/// waits first for a checkpoint to land in that env's journal, so the run leaves something to
/// resume; a push's reader is held at `fraction` meanwhile.
async fn interrupted<F, Fut>(durable_in: Option<&Env>, fraction: f64, run: F)
where
    F: FnOnce(TransferOptions) -> Fut,
    Fut: Future<Output = Result<FileReport, TransferError>>,
{
    let done = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(tokio::sync::Notify::new());
    let db = durable_in.map(|e| e._pins.path().join("journal.sqlite"));
    let o = TransferOptions {
        on_read: db
            .clone()
            .map(|db| support::hold_reads_until_checkpoint(db, (SIZE as f64 * fraction) as u64)),
        progress: Some(Arc::new({
            let (done, stop) = (done.clone(), stop.clone());
            move |n| {
                if done.fetch_add(n, Ordering::SeqCst) + n >= (SIZE as f64 * fraction) as u64 {
                    stop.notify_one();
                }
            }
        })),
        ..TransferOptions::default()
    };
    let mut run = Box::pin(run(o));
    tokio::select! {
        r = &mut run => panic!("transfer finished before the interruption: {r:?}"),
        () = stop.notified() => {}
    }
    if let Some(db) = &db {
        tokio::select! {
            r = &mut run => panic!("transfer finished before a checkpoint: {r:?}"),
            () = support::checkpoint_landed(db) => {}
        }
    }
    drop(run);
    // Let blocking writes that were already running land, as they would in a killed process.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

fn with_journal(base: TransferOptions, o: TransferOptions) -> TransferOptions {
    TransferOptions {
        progress: o.progress,
        on_read: o.on_read,
        ..base
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_push_resumes_missing_chunks_only() {
    let Some(e) = env() else { return };
    let data = write_source(&e.local_path("f"), 1, 1_600_000_000);
    interrupted(Some(&e), 0.6, |o| {
        let (e, o) = (&e, with_journal(e.opts(), o));
        async move { push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &o).await }
    })
    .await;
    assert!(
        !e.remote_path("f").exists(),
        "nothing may be renamed into place yet"
    );
    assert!(e.remote_path("f.hopsync-part").exists());

    let report = push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &e.opts())
        .await
        .expect("resumed push");
    assert!(
        report.resumed >= 8 * MIB as u64,
        "resumed only {} bytes",
        report.resumed
    );
    assert!(report.resumed < SIZE as u64);
    assert_eq!(std::fs::read(e.remote_path("f")).expect("read"), data);

    // The journal row is gone: a fresh transfer of the same name starts from zero.
    std::fs::remove_file(e.remote_path("f")).expect("rm");
    let again = push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &e.opts())
        .await
        .expect("push again");
    assert_eq!(again.resumed, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_pull_resumes_missing_chunks_only() {
    let Some(e) = env() else { return };
    let data = write_source(&e.remote_path("f"), 2, 1_600_000_000);
    interrupted(Some(&e), 0.6, |o| {
        let (e, o) = (&e, with_journal(e.opts(), o));
        async move { pull_file(&e.remote, &rel("f"), &e.local, &rel("f"), &o).await }
    })
    .await;
    assert!(!e.local_path("f").exists());
    let report = pull_file(&e.remote, &rel("f"), &e.local, &rel("f"), &e.opts())
        .await
        .expect("resumed pull");
    assert!(
        report.resumed >= 8 * MIB as u64,
        "resumed only {} bytes",
        report.resumed
    );
    assert_eq!(std::fs::read(e.local_path("f")).expect("read"), data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_source_between_runs_restarts_the_file() {
    let Some(e) = env() else { return };
    write_source(&e.local_path("f"), 3, 1_600_000_000);
    interrupted(Some(&e), 0.6, |o| {
        let (e, o) = (&e, with_journal(e.opts(), o));
        async move { push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &o).await }
    })
    .await;
    // Same size, new content and mtime: resuming would stitch two versions together.
    let new = write_source(&e.local_path("f"), 4, 1_700_000_000);
    let report = push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &e.opts())
        .await
        .expect("push");
    assert_eq!(report.resumed, 0);
    assert_eq!(std::fs::read(e.remote_path("f")).expect("read"), new);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_part_file_is_not_trusted() {
    let Some(e) = env() else { return };
    let data = write_source(&e.remote_path("f"), 5, 1_600_000_000);
    interrupted(Some(&e), 0.6, |o| {
        let (e, o) = (&e, with_journal(e.opts(), o));
        async move { pull_file(&e.remote, &rel("f"), &e.local, &rel("f"), &o).await }
    })
    .await;
    std::fs::remove_file(e.local_path("f.hopsync-part")).expect("lose the part");
    let report = pull_file(&e.remote, &rel("f"), &e.local, &rel("f"), &e.opts())
        .await
        .expect("pull");
    assert_eq!(
        report.resumed, 0,
        "a journal without its part file must start over"
    );
    assert_eq!(std::fs::read(e.local_path("f")).expect("read"), data);
}

/// Chunks are made durable in batches of `checkpoint` bytes, not one fsync each: with the default
/// (1 GiB), nothing of a 64 MiB file is recorded before it finishes, so an interrupted run resumes
/// nothing (and paid no fsync on the way).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chunks_are_recorded_in_checkpoint_batches() {
    let Some(e) = env() else { return };
    let data = write_source(&e.local_path("f"), 4, 1_600_000_000);
    let base = TransferOptions {
        checkpoint: TransferOptions::default().checkpoint,
        ..e.opts()
    };
    assert_eq!(base.checkpoint, 1 << 30, "default checkpoint");
    interrupted(None, 0.9, |o| {
        let (e, o) = (&e, with_journal(base.clone(), o));
        async move { push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &o).await }
    })
    .await;
    let report = push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &base)
        .await
        .expect("second push");
    assert_eq!(report.resumed, 0, "no batch reached the checkpoint");
    assert_eq!(std::fs::read(e.remote_path("f")).expect("read"), data);

    // Two chunks per checkpoint: what is resumed is whole batches of 16 MiB.
    let two = TransferOptions {
        checkpoint: 16 << 20,
        ..e.opts()
    };
    let data = write_source(&e.local_path("g"), 5, 1_600_000_000);
    interrupted(Some(&e), 0.9, |o| {
        let (e, o) = (&e, with_journal(two.clone(), o));
        async move { push_file(&e.local, &rel("g"), &e.remote, &rel("g"), &o).await }
    })
    .await;
    let report = push_file(&e.local, &rel("g"), &e.remote, &rel("g"), &two)
        .await
        .expect("second push");
    assert!(
        report.resumed >= 16 << 20,
        "resumed {} bytes",
        report.resumed
    );
    assert_eq!(std::fs::read(e.remote_path("g")).expect("read"), data);
}

/// Whether this filesystem allocates space on request (tmpfs, ext4, xfs and btrfs do).
fn can_preallocate(dir: &Path) -> bool {
    let probe = dir.join("probe");
    let f = std::fs::File::create(&probe).expect("probe");
    let ok = rustix::fs::fallocate(&f, rustix::fs::FallocateFlags::KEEP_SIZE, 0, 1 << 20).is_ok();
    let _ = std::fs::remove_file(probe);
    ok
}

/// A pull reserves the whole file before writing, so a hard drive lays it out in few extents
/// whatever order the chunks land in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pull_preallocates_its_part_file() {
    use std::os::unix::fs::MetadataExt;
    let Some(e) = env() else { return };
    if !can_preallocate(e.local_dir.path()) {
        eprintln!("skipping: no fallocate on this filesystem");
        return;
    }
    write_source(&e.remote_path("f"), 6, 1_600_000_000);
    interrupted(Some(&e), 0.25, |o| {
        let (e, o) = (&e, with_journal(e.opts(), o));
        async move { pull_file(&e.remote, &rel("f"), &e.local, &rel("f"), &o).await }
    })
    .await;
    let m = std::fs::metadata(e.local_path("f.hopsync-part")).expect("part");
    assert!(
        m.blocks() * 512 >= SIZE as u64,
        "{} bytes allocated of {SIZE}",
        m.blocks() * 512
    );
}
