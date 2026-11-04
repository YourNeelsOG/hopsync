//! Inline hashing and `--verify` (Phase 1 step 13).
//!
//! SSH already rejects bytes damaged in transit, so the corruption `--verify` exists for happens at
//! rest: a part file left by an interrupted run and damaged before it is resumed. The tests make
//! exactly that happen.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hopsync::engine::{
    FileReport, Remote, TransferError, TransferOptions, pull_file, pull_tree, push_file, push_tree,
};
use hopsync::journal::Journal;
use hopsync::paths::{LocalRoot, RelPath, RemoteRoot};
use hopsync::plan::PlanConfig;
use hopsync::policy::Existing;

const MIB: usize = 1 << 20;
const CHUNK: usize = 8 * MIB;
const SIZE: usize = 8 * CHUNK;

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
    fn opts(&self, verify: bool) -> TransferOptions {
        TransferOptions {
            lanes: 2,
            plan: PlanConfig {
                lanes: 2,
                min_chunk: CHUNK as u64,
                max_chunk: CHUNK as u64,
            },
            journal: Some(self.journal.clone()),
            verify,
            ..TransferOptions::default()
        }
    }
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).expect("rel")
}

fn write_source(path: &Path, seed: u8) -> Vec<u8> {
    let data: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8 ^ seed).collect();
    std::fs::write(path, &data).expect("write");
    data
}

/// Runs the transfer until 60 % of the bytes are through, then drops it, as a killed process would.
async fn interrupted<F, Fut>(base: TransferOptions, run: F)
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
                if done.fetch_add(n, Ordering::SeqCst) + n >= (SIZE as u64) * 6 / 10 {
                    stop.notify_one();
                }
            }
        })),
        ..base
    };
    tokio::select! {
        r = run(o) => panic!("transfer finished before the interruption: {r:?}"),
        () = stop.notified() => {}
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// Flips one byte in the middle of every chunk the part file covers, so whichever chunks the
/// journal recorded are damaged.
fn damage(part: &Path) {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::options()
        .read(true)
        .write(true)
        .open(part)
        .expect("open part");
    let len = f.metadata().expect("meta").len() as usize;
    for at in (CHUNK / 2..len).step_by(CHUNK) {
        let mut b = [0u8];
        f.read_exact_at(&mut b, at as u64).expect("read");
        f.write_all_at(&[!b[0]], at as u64).expect("write");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_checks_every_byte_of_a_push_and_a_pull() {
    let Some(e) = env() else { return };
    let data = write_source(&e.local_path("f"), 1);
    let r = push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &e.opts(true))
        .await
        .expect("push");
    assert_eq!(r.verified, SIZE as u64);
    assert_eq!(std::fs::read(e.remote_path("f")).expect("read"), data);

    let r = pull_file(&e.remote, &rel("f"), &e.local, &rel("g"), &e.opts(true))
        .await
        .expect("pull");
    assert_eq!(r.verified, SIZE as u64);
    assert_eq!(std::fs::read(e.local_path("g")).expect("read"), data);

    // Off by default: nothing is read back.
    let o = TransferOptions {
        existing: Existing::Overwrite,
        ..e.opts(false)
    };
    let r = push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &o)
        .await
        .expect("push");
    assert_eq!(r.verified, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_catches_a_damaged_part_on_push_resume() {
    let Some(e) = env() else { return };
    let data = write_source(&e.local_path("f"), 2);
    interrupted(e.opts(false), |o| {
        let e = &e;
        async move { push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &o).await }
    })
    .await;
    damage(&e.remote_path("f.hopsync-part"));

    let err = push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &e.opts(true))
        .await
        .expect_err("the damage must be caught");
    assert!(
        matches!(err, TransferError::VerifyFailed { .. }),
        "got {err}"
    );
    assert!(
        !e.remote_path("f").exists(),
        "a damaged file must never be renamed into place"
    );

    // The damaged chunks are no longer trusted: the next run, even without verify, redoes them.
    let r = push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &e.opts(false))
        .await
        .expect("push");
    assert_eq!(r.resumed, 0);
    assert_eq!(std::fs::read(e.remote_path("f")).expect("read"), data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_catches_a_damaged_part_on_pull_resume() {
    let Some(e) = env() else { return };
    let data = write_source(&e.remote_path("f"), 3);
    interrupted(e.opts(false), |o| {
        let e = &e;
        async move { pull_file(&e.remote, &rel("f"), &e.local, &rel("f"), &o).await }
    })
    .await;
    damage(&e.local_path("f.hopsync-part"));

    let err = pull_file(&e.remote, &rel("f"), &e.local, &rel("f"), &e.opts(true))
        .await
        .expect_err("the damage must be caught");
    assert!(
        matches!(err, TransferError::VerifyFailed { .. }),
        "got {err}"
    );
    assert!(!e.local_path("f").exists());

    let r = pull_file(&e.remote, &rel("f"), &e.local, &rel("f"), &e.opts(false))
        .await
        .expect("pull");
    assert_eq!(r.resumed, 0);
    assert_eq!(std::fs::read(e.local_path("f")).expect("read"), data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_covers_small_files_in_trees() {
    let Some(e) = env() else { return };
    std::fs::create_dir_all(e.local_path("t/sub")).expect("mkdir");
    std::fs::write(e.local_path("t/empty"), b"").expect("write");
    std::fs::write(e.local_path("t/sub/small"), b"hello").expect("write");
    let r = push_tree(&e.local, &rel("t"), &e.remote, &rel("t"), &e.opts(true))
        .await
        .expect("push");
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    assert_eq!((r.files, r.verified), (2, 5));
    let r = pull_tree(&e.remote, &rel("t"), &e.local, &rel("back"), &e.opts(true))
        .await
        .expect("pull");
    assert_eq!((r.files, r.verified), (2, 5));
    assert_eq!(
        std::fs::read(e.local_path("back/sub/small")).expect("read"),
        b"hello"
    );
}
