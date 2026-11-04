//! Single-file push and pull against a real OpenSSH server (Phase 1 step 9).
//!
//! The fixture's "remote" filesystem is this machine's, so both sides are checked on disk.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hopsync::engine::{Remote, TransferError, TransferOptions, pull_file, push_file};
use hopsync::paths::{LocalRoot, RelPath, RemoteRoot};
use hopsync::plan::PlanConfig;

const MIB: usize = 1 << 20;

struct Env {
    sshd: support::TestSshd,
    _pins: tempfile::TempDir,
    remote: Remote,
    local_dir: tempfile::TempDir,
    local: LocalRoot,
}

/// Local side in its own temp dir; remote root is a `remote/` dir inside the fixture's dir.
fn env() -> Option<Env> {
    let sshd = support::TestSshd::start()?;
    let (pins, known, target) = sshd.client();
    let remote_dir = sshd.dir().join("remote");
    std::fs::create_dir(&remote_dir).expect("mkdir remote");
    let root = RemoteRoot::parse(remote_dir.to_str().expect("utf8")).expect("remote root");
    let local_dir = tempfile::tempdir().expect("tempdir");
    let local = LocalRoot::open(local_dir.path()).expect("local root");
    Some(Env {
        remote: Remote::new(target, known, root),
        sshd,
        _pins: pins,
        local_dir,
        local,
    })
}

impl Env {
    fn remote_path(&self, name: &str) -> std::path::PathBuf {
        self.sshd.dir().join("remote").join(name)
    }
    fn local_path(&self, name: &str) -> std::path::PathBuf {
        self.local_dir.path().join(name)
    }
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).expect("rel path")
}

/// Several chunks over 4 lanes: 8 MiB chunks for a file of a few tens of MiB.
fn opts() -> TransferOptions {
    TransferOptions {
        lanes: 4,
        plan: PlanConfig {
            lanes: 4,
            min_chunk: 8 << 20,
            max_chunk: 8 << 20,
        },
        ..TransferOptions::default()
    }
}

fn pattern(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// Writes `data` with an mtime well in the past, so a preserved mtime is distinguishable from "now".
fn write_old(path: &Path, data: &[u8]) -> u64 {
    std::fs::write(path, data).expect("write source");
    let t = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(t))
        .expect("set mtime");
    1_600_000_000
}

fn mtime_secs(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))
        .expect("mtime")
}

fn assert_no_part_files(dir: &Path) {
    for e in std::fs::read_dir(dir).expect("read dir") {
        let name = e.expect("entry").file_name();
        assert!(
            !name.to_string_lossy().ends_with(".hopsync-part"),
            "leftover {name:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_is_byte_identical_and_keeps_mtime() {
    let Some(e) = env() else { return };
    let data = pattern(37 * MIB + 123, 1);
    let mtime = write_old(&e.local_path("big.bin"), &data);
    let report = push_file(
        &e.local,
        &rel("big.bin"),
        &e.remote,
        &rel("big.bin"),
        &opts(),
    )
    .await
    .expect("push");
    assert_eq!(report.bytes, data.len() as u64);
    assert_eq!(std::fs::read(e.remote_path("big.bin")).expect("read"), data);
    assert_eq!(mtime_secs(&e.remote_path("big.bin")), mtime);
    assert_no_part_files(&e.remote_path(""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_is_byte_identical_and_keeps_mtime() {
    let Some(e) = env() else { return };
    let data = pattern(37 * MIB + 123, 2);
    let mtime = write_old(&e.remote_path("big.bin"), &data);
    let report = pull_file(
        &e.remote,
        &rel("big.bin"),
        &e.local,
        &rel("big.bin"),
        &opts(),
    )
    .await
    .expect("pull");
    assert_eq!(report.bytes, data.len() as u64);
    assert_eq!(std::fs::read(e.local_path("big.bin")).expect("read"), data);
    assert_eq!(mtime_secs(&e.local_path("big.bin")), mtime);
    assert_no_part_files(e.local_dir.path());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_byte_files_are_created() {
    let Some(e) = env() else { return };
    write_old(&e.local_path("empty"), b"");
    push_file(&e.local, &rel("empty"), &e.remote, &rel("empty"), &opts())
        .await
        .expect("push");
    assert_eq!(
        std::fs::metadata(e.remote_path("empty"))
            .expect("meta")
            .len(),
        0
    );

    write_old(&e.remote_path("empty2"), b"");
    pull_file(&e.remote, &rel("empty2"), &e.local, &rel("empty2"), &opts())
        .await
        .expect("pull");
    assert_eq!(
        std::fs::metadata(e.local_path("empty2"))
            .expect("meta")
            .len(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_destination_is_refused_unless_overwrite() {
    let Some(e) = env() else { return };
    write_old(&e.local_path("f"), b"new contents");
    std::fs::write(e.remote_path("f"), b"old").expect("write");
    let err = push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &opts())
        .await
        .expect_err("exists");
    assert!(
        matches!(err, TransferError::DestinationExists(_)),
        "got {err}"
    );
    assert_eq!(std::fs::read(e.remote_path("f")).expect("read"), b"old");

    let ow = TransferOptions {
        existing: hopsync::policy::Existing::Overwrite,
        ..opts()
    };
    push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &ow)
        .await
        .expect("overwrite push");
    assert_eq!(
        std::fs::read(e.remote_path("f")).expect("read"),
        b"new contents"
    );

    std::fs::write(e.local_path("g"), b"old").expect("write");
    let err = pull_file(&e.remote, &rel("f"), &e.local, &rel("g"), &opts())
        .await
        .expect_err("exists");
    assert!(
        matches!(err, TransferError::DestinationExists(_)),
        "got {err}"
    );
    assert_eq!(std::fs::read(e.local_path("g")).expect("read"), b"old");
    pull_file(&e.remote, &rel("f"), &e.local, &rel("g"), &ow)
        .await
        .expect("overwrite pull");
    assert_eq!(
        std::fs::read(e.local_path("g")).expect("read"),
        b"new contents"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_larger_part_file_does_not_leak_into_result() {
    let Some(e) = env() else { return };
    let data = pattern(3 * MIB, 3);
    write_old(&e.local_path("f"), &data);
    std::fs::write(e.remote_path("f.hopsync-part"), vec![0xffu8; 5 * MIB]).expect("stale");
    push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &opts())
        .await
        .expect("push");
    assert_eq!(std::fs::read(e.remote_path("f")).expect("read"), data);

    write_old(&e.remote_path("r"), &data);
    std::fs::write(e.local_path("r.hopsync-part"), vec![0xffu8; 5 * MIB]).expect("stale");
    pull_file(&e.remote, &rel("r"), &e.local, &rel("r"), &opts())
        .await
        .expect("pull");
    assert_eq!(std::fs::read(e.local_path("r")).expect("read"), data);
}

/// Kills every sftp-server once a third of the bytes are through; lanes must reconnect and redo
/// their chunks.
async fn survives_killed_lanes(push: bool) {
    let Some(e) = env() else { return };
    let data = pattern(64 * MIB, 4);
    let src = if push {
        e.local_path("f")
    } else {
        e.remote_path("f")
    };
    write_old(&src, &data);
    let sshd = Arc::new(e.sshd);
    let done = Arc::new(AtomicU64::new(0));
    let killed = Arc::new(AtomicBool::new(false));
    let o = TransferOptions {
        progress: Some(Arc::new({
            let (sshd, done, killed) = (sshd.clone(), done.clone(), killed.clone());
            move |n| {
                let total = done.fetch_add(n, Ordering::SeqCst) + n;
                if total > data_len() / 3 && !killed.swap(true, Ordering::SeqCst) {
                    assert!(sshd.signal_sftp_servers("KILL") > 0, "nothing to kill");
                }
            }
        })),
        ..opts()
    };
    let report = if push {
        push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &o).await
    } else {
        pull_file(&e.remote, &rel("f"), &e.local, &rel("f"), &o).await
    }
    .expect("transfer survives killed lanes");
    assert!(killed.load(Ordering::SeqCst));
    assert!(
        report.retries >= 1,
        "expected retries, got {}",
        report.retries
    );
    let dst = if push {
        sshd.dir().join("remote/f")
    } else {
        e.local_dir.path().join("f")
    };
    assert_eq!(std::fs::read(dst).expect("read"), data);
}

fn data_len() -> u64 {
    (64 * MIB) as u64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_survives_killed_lanes() {
    survives_killed_lanes(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_survives_killed_lanes() {
    survives_killed_lanes(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_changing_mid_push_fails_and_leaves_no_destination() {
    let Some(e) = env() else { return };
    let path = e.local_path("grow");
    write_old(&path, &pattern(32 * MIB, 5));
    let touched = Arc::new(AtomicBool::new(false));
    let o = TransferOptions {
        progress: Some(Arc::new({
            let (path, touched) = (path.clone(), touched.clone());
            move |_| {
                if !touched.swap(true, Ordering::SeqCst) {
                    let f = std::fs::File::options()
                        .write(true)
                        .open(&path)
                        .expect("open");
                    f.set_modified(SystemTime::now()).expect("touch");
                }
            }
        })),
        ..opts()
    };
    let err = push_file(&e.local, &rel("grow"), &e.remote, &rel("grow"), &o)
        .await
        .expect_err("source changed");
    assert!(matches!(err, TransferError::SourceChanged(_)), "got {err}");
    assert!(!e.remote_path("grow").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_that_is_not_a_regular_file_is_refused() {
    let Some(e) = env() else { return };
    std::os::unix::fs::symlink("/etc/hostname", e.local_path("link")).expect("symlink");
    std::fs::create_dir(e.local_path("dir")).expect("mkdir");
    for name in ["link", "dir"] {
        let err = push_file(&e.local, &rel(name), &e.remote, &rel(name), &opts())
            .await
            .expect_err("not a file");
        assert!(
            matches!(err, TransferError::NotAFile(_)),
            "{name}: got {err}"
        );
    }
    std::os::unix::fs::symlink("/etc/hostname", e.remote_path("rlink")).expect("symlink");
    let err = pull_file(&e.remote, &rel("rlink"), &e.local, &rel("rlink"), &opts())
        .await
        .expect_err("not a file");
    assert!(matches!(err, TransferError::NotAFile(_)), "got {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_destination_directory_is_named_in_the_error() {
    let Some(e) = env() else { return };
    write_old(&e.local_path("f"), b"x");
    let err = push_file(&e.local, &rel("f"), &e.remote, &rel("nodir/f"), &opts())
        .await
        .expect_err("no dir");
    assert!(
        matches!(&err, TransferError::MissingDirectory(d) if d.ends_with("remote/nodir")),
        "got {err:?}"
    );
    write_old(&e.remote_path("r"), b"x");
    let err = pull_file(&e.remote, &rel("r"), &e.local, &rel("nodir/r"), &opts())
        .await
        .expect_err("no dir");
    assert!(
        matches!(&err, TransferError::MissingDirectory(d) if d == "nodir"),
        "got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lookup_before_a_transfer_reuses_its_connection() {
    let Some(e) = env() else { return };
    std::fs::create_dir(e.remote_path("dir")).expect("mkdir");
    std::fs::write(e.local_path("small"), b"one connection").expect("write");
    let dir = e.remote_path("dir").to_str().expect("utf8").to_owned();
    let kind = e.remote.kind(&dir, &opts()).await.expect("lookup");
    assert_eq!(kind, Some(hopsync::sftp::RemoteKind::Dir));
    let missing = e.remote_path("nope").to_str().expect("utf8").to_owned();
    assert_eq!(
        e.remote.kind(&missing, &opts()).await.expect("lookup"),
        None
    );

    // The CLI looks the destination up, then picks the root: the session survives the change.
    let into = e.remote.with_root(RemoteRoot::parse(&dir).expect("root"));
    push_file(&e.local, &rel("small"), &into, &rel("small"), &opts())
        .await
        .expect("push");
    assert_eq!(
        std::fs::read(e.remote_path("dir/small")).expect("read"),
        b"one connection"
    );
    let logins = e.sshd.log().matches("Accepted publickey").count();
    assert_eq!(logins, 1, "lookup and a one-chunk push need one connection");
}
