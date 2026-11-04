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
    /// 8 chunks of 8 MiB over 2 lanes, journaled.
    fn opts(&self) -> TransferOptions {
        TransferOptions {
            lanes: 2,
            plan: PlanConfig {
                lanes: 2,
                min_chunk: 8 << 20,
                max_chunk: 8 << 20,
            },
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

/// Runs the transfer until `fraction` of the bytes are through, then drops it.
async fn interrupted<F, Fut>(fraction: f64, run: F)
where
    F: FnOnce(TransferOptions) -> Fut,
    Fut: Future<Output = Result<FileReport, TransferError>>,
{
    let done = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(tokio::sync::Notify::new());
    let o = TransferOptions {
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
    tokio::select! {
        r = run(o) => panic!("transfer finished before the interruption: {r:?}"),
        () = stop.notified() => {}
    }
    // Let blocking writes that were already running land, as they would in a killed process.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

fn with_journal(base: TransferOptions, o: TransferOptions) -> TransferOptions {
    TransferOptions {
        progress: o.progress,
        ..base
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_push_resumes_missing_chunks_only() {
    let Some(e) = env() else { return };
    let data = write_source(&e.local_path("f"), 1, 1_600_000_000);
    interrupted(0.6, |o| {
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
    interrupted(0.6, |o| {
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
    interrupted(0.6, |o| {
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
    interrupted(0.6, |o| {
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
