//! Directory tree push and pull against a real OpenSSH server (Phase 1 step 10).

mod support;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use hopsync::engine::{Problem, Remote, TransferError, TransferOptions, pull_tree, push_tree};
use hopsync::paths::{LocalRoot, RelPath, RemoteRoot};
use hopsync::plan::PlanConfig;
use hopsync::sftp::SftpError;

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
    fn remote_path(&self, rel: &str) -> std::path::PathBuf {
        self.sshd.dir().join("remote").join(rel)
    }
    fn local_path(&self, rel: &str) -> std::path::PathBuf {
        self.local_dir.path().join(rel)
    }
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).expect("rel")
}

fn opts() -> TransferOptions {
    TransferOptions {
        lanes: 3,
        plan: PlanConfig {
            lanes: 3,
            min_chunk: 8 << 20,
            max_chunk: 8 << 20,
        },
        ..TransferOptions::default()
    }
}

/// Unicode, a space, an empty directory, a zero-byte file, a multi-chunk file, a symlink and a name
/// with a newline (which Hopsync refuses to carry).
fn build_tree(base: &Path) {
    std::fs::create_dir_all(base.join("a/b c")).expect("mkdir");
    std::fs::create_dir_all(base.join("empty")).expect("mkdir");
    std::fs::write(base.join("a/b c/ü日本.txt"), "unicode ✓").expect("write");
    std::fs::write(base.join("a/b c/zero"), b"").expect("write");
    let big: Vec<u8> = (0..20 * 1024 * 1024 + 5).map(|i| (i % 251) as u8).collect();
    std::fs::write(base.join("big.bin"), big).expect("write");
    std::os::unix::fs::symlink("a", base.join("link")).expect("symlink");
    std::fs::write(base.join("bad\nname"), b"x").expect("write");
}

/// Every regular file (with contents) and directory under `dir`, by slash path. Symlinks and names
/// with control characters are left out: those are the entries a transfer must skip.
fn snapshot(dir: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        for e in std::fs::read_dir(dir).expect("read_dir") {
            let e = e.expect("entry");
            let name = e.file_name().to_string_lossy().into_owned();
            if name.chars().any(char::is_control) {
                continue;
            }
            let path = e.path();
            let key = path
                .strip_prefix(base)
                .expect("prefix")
                .to_string_lossy()
                .into_owned();
            let ft = e.file_type().expect("type");
            if ft.is_dir() {
                out.insert(key, None);
                walk(base, &path, out);
            } else if ft.is_file() {
                out.insert(key, Some(std::fs::read(&path).expect("read")));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn problems(report: &hopsync::engine::TreeReport) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = report
        .problems
        .iter()
        .map(|(p, pr)| (p.clone(), kind(pr).to_owned()))
        .collect();
    v.sort();
    v
}

fn kind(p: &Problem) -> &'static str {
    match p {
        Problem::Symlink => "symlink",
        Problem::Special => "special",
        Problem::InvalidName => "invalid-name",
        Problem::Exists => "exists",
        Problem::Newer => "newer",
        Problem::PartFile => "part-file",
        Problem::Failed(_) => "failed",
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_tree_copies_everything_it_can_and_reports_the_rest() {
    let Some(e) = env() else { return };
    build_tree(&e.local_path("src"));
    let report = push_tree(&e.local, &rel("src"), &e.remote, &rel("dst"), &opts())
        .await
        .expect("push tree");
    assert_eq!(
        snapshot(&e.remote_path("dst")),
        snapshot(&e.local_path("src"))
    );
    assert_eq!(
        problems(&report),
        vec![
            ("bad\nname".into(), "invalid-name".into()),
            ("link".into(), "symlink".into())
        ]
    );
    assert_eq!((report.files, report.dirs), (3, 3));
    assert!(e.remote_path("dst/empty").is_dir());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_tree_copies_everything_it_can_and_reports_the_rest() {
    let Some(e) = env() else { return };
    build_tree(&e.remote_path("src"));
    let report = pull_tree(&e.remote, &rel("src"), &e.local, &rel("dst"), &opts())
        .await
        .expect("pull tree");
    assert_eq!(
        snapshot(&e.local_path("dst")),
        snapshot(&e.remote_path("src"))
    );
    assert_eq!(
        problems(&report),
        vec![
            ("bad\nname".into(), "invalid-name".into()),
            ("link".into(), "symlink".into())
        ]
    );
    assert_eq!((report.files, report.dirs), (3, 3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreadable_directory_fails_alone() {
    let Some(e) = env() else { return };
    for base in [e.local_path("src"), e.remote_path("src")] {
        std::fs::create_dir_all(base.join("locked")).expect("mkdir");
        std::fs::write(base.join("locked/secret"), b"s").expect("write");
        std::fs::write(base.join("ok"), b"fine").expect("write");
        std::fs::set_permissions(base.join("locked"), PermissionsExt::from_mode(0o000))
            .expect("chmod");
    }
    let push = push_tree(&e.local, &rel("src"), &e.remote, &rel("pushed"), &opts()).await;
    let pull = pull_tree(&e.remote, &rel("src"), &e.local, &rel("pulled"), &opts()).await;
    for base in [e.local_path("src"), e.remote_path("src")] {
        std::fs::set_permissions(base.join("locked"), PermissionsExt::from_mode(0o755))
            .expect("chmod back");
    }
    let push = push.expect("push tree");
    assert_eq!(problems(&push), vec![("locked".into(), "failed".into())]);
    assert!(matches!(
        &push.problems[0].1,
        Problem::Failed(TransferError::Local { .. })
    ));
    assert_eq!(
        std::fs::read(e.remote_path("pushed/ok")).expect("read"),
        b"fine"
    );

    let pull = pull.expect("pull tree");
    assert_eq!(problems(&pull), vec![("locked".into(), "failed".into())]);
    assert!(matches!(
        &pull.problems[0].1,
        Problem::Failed(TransferError::Remote(SftpError::PermissionDenied(_)))
    ));
    assert_eq!(
        std::fs::read(e.local_path("pulled/ok")).expect("read"),
        b"fine"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_files_are_skipped_without_overwrite() {
    let Some(e) = env() else { return };
    build_tree(&e.local_path("src"));
    std::fs::create_dir_all(e.remote_path("dst/a/b c")).expect("mkdir");
    std::fs::write(e.remote_path("dst/a/b c/zero"), b"keep me").expect("write");
    let report = push_tree(&e.local, &rel("src"), &e.remote, &rel("dst"), &opts())
        .await
        .expect("push tree");
    assert!(problems(&report).contains(&("a/b c/zero".into(), "exists".into())));
    assert_eq!(
        std::fs::read(e.remote_path("dst/a/b c/zero")).expect("read"),
        b"keep me"
    );
    assert_eq!(
        std::fs::read(e.remote_path("dst/a/b c/ü日本.txt")).expect("read"),
        "unicode ✓".as_bytes()
    );
    assert_eq!(report.files, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ten_thousand_small_files_round_trip() {
    let Some(e) = env() else { return };
    let src = e.local_path("many");
    for d in 0..100 {
        let dir = src.join(format!("d{d:03}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        for f in 0..100 {
            std::fs::write(dir.join(format!("f{f:03}")), format!("{d}/{f} payload"))
                .expect("write");
        }
    }
    let o = TransferOptions {
        lanes: 4,
        ..TransferOptions::default()
    };
    let t = std::time::Instant::now();
    let pushed = push_tree(&e.local, &rel("many"), &e.remote, &rel("many"), &o)
        .await
        .expect("push");
    let push_time = t.elapsed();
    assert_eq!((pushed.files, pushed.dirs), (10_000, 100));
    assert!(pushed.problems.is_empty(), "{:?}", problems(&pushed));
    let t = std::time::Instant::now();
    let pulled = pull_tree(&e.remote, &rel("many"), &e.local, &rel("back"), &o)
        .await
        .expect("pull");
    let pull_time = t.elapsed();
    assert_eq!(pulled.files, 10_000);
    assert_eq!(snapshot(&e.local_path("back")), snapshot(&src));
    eprintln!("10k files over loopback: push {push_time:?}, pull {pull_time:?}");
}

/// Many multi-chunk files finishing at once across 16 workers: every file must be finalized exactly
/// once (a check-then-decrement on the chunk count let two last chunks both skip finalizing).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_last_chunks_finalize_every_file() {
    let Some(e) = env() else { return };
    let src = e.local_path("chunks");
    std::fs::create_dir(&src).expect("mkdir");
    for i in 0..40u8 {
        std::fs::write(src.join(format!("f{i:02}")), vec![i; 4 << 20]).expect("write");
    }
    let o = TransferOptions {
        lanes: 4,
        per_lane: 4,
        plan: PlanConfig {
            lanes: 4,
            min_chunk: 1 << 20,
            max_chunk: 1 << 20,
        },
        ..TransferOptions::default()
    };
    let report = push_tree(&e.local, &rel("chunks"), &e.remote, &rel("chunks"), &o)
        .await
        .expect("push");
    assert!(report.problems.is_empty(), "{:?}", problems(&report));
    assert_eq!(report.files, 40);
    assert_eq!(snapshot(&e.remote_path("chunks")), snapshot(&src));
    let parts = std::fs::read_dir(e.remote_path("chunks"))
        .expect("read")
        .filter(|d| {
            d.as_ref()
                .is_ok_and(|d| d.file_name().to_string_lossy().ends_with(".hopsync-part"))
        })
        .count();
    assert_eq!(parts, 0);
}
