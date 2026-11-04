//! Destination policies and the free-space check (Phase 1 step 12).

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use hopsync::engine::{
    Problem, Remote, TransferError, TransferOptions, TreeReport, pull_tree, push_file, push_tree,
};
use hopsync::paths::{LocalRoot, RelPath, RemoteRoot};
use hopsync::policy::Existing;

struct Env {
    sshd: support::TestSshd,
    _pins: tempfile::TempDir,
    remote: Remote,
    local_dir: tempfile::TempDir,
    local: LocalRoot,
}

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
    fn remote_path(&self, rel: &str) -> PathBuf {
        self.sshd.dir().join("remote").join(rel)
    }
    fn local_path(&self, rel: &str) -> PathBuf {
        self.local_dir.path().join(rel)
    }
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).expect("rel")
}

fn put(path: &Path, data: &[u8], mtime: u64) {
    std::fs::write(path, data).expect("write");
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(UNIX_EPOCH + Duration::from_secs(mtime)))
        .expect("mtime");
}

/// Source files (mtime 1000) and a destination holding a same, an older, a newer and a partial copy.
fn scenario(src: &Path, dst: &Path) {
    std::fs::create_dir_all(src).expect("mkdir");
    std::fs::create_dir_all(dst).expect("mkdir");
    for name in [
        "same.txt",
        "older.txt",
        "newer.txt",
        "partial.bin",
        "new.txt",
    ] {
        put(&src.join(name), format!("source {name}").as_bytes(), 1000);
    }
    put(&dst.join("same.txt"), b"source same.txt", 1000);
    put(&dst.join("older.txt"), b"old version", 900);
    put(&dst.join("newer.txt"), b"edited at the destination", 1100);
    put(&dst.join("partial.bin"), b"source par", 1000);
}

fn read(path: PathBuf) -> String {
    String::from_utf8(std::fs::read(path).expect("read")).expect("utf8")
}

fn kinds(r: &TreeReport) -> Vec<(String, &'static str)> {
    let mut v: Vec<(String, &'static str)> = r
        .problems
        .iter()
        .map(|(p, pr)| {
            let k = match pr {
                Problem::Exists => "exists",
                Problem::Newer => "newer",
                Problem::Failed(_) => "failed",
                _ => "other",
            };
            (p.clone(), k)
        })
        .collect();
    v.sort();
    v
}

fn opts(existing: Existing) -> TransferOptions {
    TransferOptions {
        existing,
        ..TransferOptions::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skip_leaves_every_existing_file() {
    let Some(e) = env() else { return };
    scenario(&e.local_path("src"), &e.remote_path("dst"));
    let r = push_tree(
        &e.local,
        &rel("src"),
        &e.remote,
        &rel("dst"),
        &opts(Existing::Skip),
    )
    .await
    .expect("push");
    assert_eq!(r.files, 1);
    assert_eq!(
        kinds(&r),
        vec![
            ("newer.txt".into(), "exists"),
            ("older.txt".into(), "exists"),
            ("partial.bin".into(), "exists"),
            ("same.txt".into(), "exists"),
        ]
    );
    assert_eq!(read(e.remote_path("dst/older.txt")), "old version");
    assert_eq!(read(e.remote_path("dst/new.txt")), "source new.txt");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_replaces_older_and_partial_keeps_same_and_newer() {
    let Some(e) = env() else { return };
    scenario(&e.local_path("src"), &e.remote_path("dst"));
    let r = push_tree(
        &e.local,
        &rel("src"),
        &e.remote,
        &rel("dst"),
        &opts(Existing::Update),
    )
    .await
    .expect("push");
    assert_eq!((r.files, r.unchanged), (3, 1));
    assert_eq!(kinds(&r), vec![("newer.txt".into(), "newer")]);
    assert_eq!(read(e.remote_path("dst/older.txt")), "source older.txt");
    assert_eq!(read(e.remote_path("dst/partial.bin")), "source partial.bin");
    assert_eq!(
        read(e.remote_path("dst/newer.txt")),
        "edited at the destination"
    );
    assert_eq!(read(e.remote_path("dst/new.txt")), "source new.txt");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_works_for_pull_too() {
    let Some(e) = env() else { return };
    scenario(&e.remote_path("src"), &e.local_path("dst"));
    let r = pull_tree(
        &e.remote,
        &rel("src"),
        &e.local,
        &rel("dst"),
        &opts(Existing::Update),
    )
    .await
    .expect("pull");
    assert_eq!((r.files, r.unchanged), (3, 1));
    assert_eq!(kinds(&r), vec![("newer.txt".into(), "newer")]);
    assert_eq!(read(e.local_path("dst/partial.bin")), "source partial.bin");
    assert_eq!(
        read(e.local_path("dst/newer.txt")),
        "edited at the destination"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overwrite_replaces_everything() {
    let Some(e) = env() else { return };
    scenario(&e.local_path("src"), &e.remote_path("dst"));
    let r = push_tree(
        &e.local,
        &rel("src"),
        &e.remote,
        &rel("dst"),
        &opts(Existing::Overwrite),
    )
    .await
    .expect("push");
    assert_eq!(r.files, 5);
    assert!(r.problems.is_empty());
    for name in [
        "same.txt",
        "older.txt",
        "newer.txt",
        "partial.bin",
        "new.txt",
    ] {
        assert_eq!(
            read(e.remote_path(&format!("dst/{name}"))),
            format!("source {name}")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_keeps_existing_and_writes_beside_them() {
    let Some(e) = env() else { return };
    scenario(&e.local_path("src"), &e.remote_path("dst"));
    std::fs::write(e.remote_path("dst/older (1).txt"), b"taken").expect("write");
    let r = push_tree(
        &e.local,
        &rel("src"),
        &e.remote,
        &rel("dst"),
        &opts(Existing::Rename),
    )
    .await
    .expect("push");
    assert_eq!(r.files, 5);
    assert_eq!(
        read(e.remote_path("dst/newer.txt")),
        "edited at the destination"
    );
    assert_eq!(read(e.remote_path("dst/newer (1).txt")), "source newer.txt");
    assert_eq!(read(e.remote_path("dst/older (1).txt")), "taken");
    assert_eq!(read(e.remote_path("dst/older (2).txt")), "source older.txt");
    assert_eq!(
        read(e.remote_path("dst/partial (1).bin")),
        "source partial.bin"
    );
    assert_eq!(read(e.remote_path("dst/new.txt")), "source new.txt");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_enough_space_fails_before_any_data_moves() {
    let Some(e) = env() else { return };
    put(&e.local_path("f"), &vec![1u8; 4 << 20], 1000);
    // Ask to keep more free than any disk has: the check must refuse up front.
    let o = TransferOptions {
        min_free: u64::MAX / 2,
        ..TransferOptions::default()
    };
    let err = push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &o)
        .await
        .expect_err("no space");
    assert!(matches!(err, TransferError::NoSpace { .. }), "got {err}");
    assert!(
        !e.remote_path("f.hopsync-part").exists(),
        "no data may move"
    );

    scenario(&e.local_path("src"), &e.local_path("unused"));
    let r = push_tree(&e.local, &rel("src"), &e.remote, &rel("dst"), &o)
        .await
        .expect("tree");
    assert_eq!(r.files, 0);
    assert_eq!(r.problems.len(), 5);
    assert!(
        r.problems
            .iter()
            .all(|(_, p)| matches!(p, Problem::Failed(TransferError::NoSpace { .. })))
    );
    assert_eq!(
        std::fs::read_dir(e.remote_path("dst"))
            .expect("dir")
            .count(),
        0
    );

    put(&e.remote_path("r"), &vec![2u8; 4 << 20], 1000);
    let err = hopsync::engine::pull_file(&e.remote, &rel("r"), &e.local, &rel("r"), &o)
        .await
        .expect_err("no space");
    assert!(matches!(err, TransferError::NoSpace { .. }), "got {err}");
    assert!(!e.local_path("r.hopsync-part").exists());
}
