//! A remote that lies (Phase 1 security review): sizes, listings and names it reports must not be
//! able to exhaust the client or make it corrupt data.

mod support;

use std::path::PathBuf;
use std::time::Duration;

use hopsync::engine::{
    Problem, Remote, TransferError, TransferOptions, pull_file, pull_tree, push_tree,
};
use hopsync::paths::{LocalRoot, RelPath, RemoteRoot};
use hopsync::policy::Existing;
use hopsync::sftp::SftpOptions;

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
    fn remote_path(&self, name: &str) -> PathBuf {
        self.sshd.dir().join("remote").join(name)
    }
    fn local_path(&self, name: &str) -> PathBuf {
        self.local_dir.path().join(name)
    }
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).expect("rel")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claimed_size_of_exabytes_costs_no_memory() {
    // Address space capped at 8 GiB: planning 2^62 bytes in 64 MiB chunks up front would need
    // terabytes and abort here instead of filling the machine's memory.
    let cap = 8 << 30;
    rustix::process::setrlimit(
        rustix::process::Resource::As,
        rustix::process::Rlimit {
            current: Some(cap),
            maximum: None,
        },
    )
    .expect("setrlimit");
    let Some(e) = env() else { return };
    // Sparse: the remote "file" takes no space, but reports 4 EiB. It lives on tmpfs because ext4
    // refuses a file this large with EFBIG, and a smaller claim would not prove the point above.
    let huge_dir = tempfile::tempdir_in("/dev/shm").expect("tempdir on tmpfs");
    std::fs::File::create(huge_dir.path().join("huge"))
        .and_then(|f| f.set_len(1 << 62))
        .expect("sparse file");
    let (_huge_pins, known, target) = e.sshd.client();
    let root = RemoteRoot::parse(huge_dir.path().to_str().expect("utf8")).expect("remote root");
    let remote = Remote::new(target, known, root);
    let err = tokio::time::timeout(
        Duration::from_secs(20),
        pull_file(
            &remote,
            &rel("huge"),
            &e.local,
            &rel("huge"),
            &TransferOptions::default(),
        ),
    )
    .await
    .expect("must not hang")
    .expect_err("cannot fit");
    assert!(matches!(err, TransferError::NoSpace { .. }), "got {err}");
    assert!(!e.local_path("huge.hopsync-part").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_endless_listing_is_cut_off() {
    let Some(e) = env() else { return };
    std::fs::create_dir(e.remote_path("big")).expect("mkdir");
    for i in 0..50 {
        std::fs::write(e.remote_path(&format!("big/f{i}")), b"x").expect("write");
    }
    let o = TransferOptions {
        sftp: SftpOptions {
            max_dir_entries: 20,
            ..SftpOptions::default()
        },
        ..TransferOptions::default()
    };
    let r = pull_tree(&e.remote, &rel("big"), &e.local, &rel("big"), &o)
        .await
        .expect("tree");
    assert_eq!(r.files, 0);
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    let (path, p) = &r.problems[0];
    assert_eq!(path, ".");
    assert!(p.to_string().contains("more than 20 entries"), "{p}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leftover_part_files_in_a_tree_are_not_sent() {
    let Some(e) = env() else { return };
    // A directory that once received an interrupted transfer: `a` beside a stale part of `a`.
    std::fs::create_dir(e.local_path("t")).expect("mkdir");
    std::fs::write(e.local_path("t/a"), b"the real a").expect("write");
    std::fs::write(e.local_path("t/a.hopsync-part"), b"stale half of a").expect("write");
    let o = TransferOptions {
        existing: Existing::Overwrite,
        ..TransferOptions::default()
    };
    let r = push_tree(&e.local, &rel("t"), &e.remote, &rel("t"), &o)
        .await
        .expect("push");
    assert_eq!(r.files, 1);
    assert_eq!(
        std::fs::read(e.remote_path("t/a")).expect("read"),
        b"the real a"
    );
    assert!(!e.remote_path("t/a.hopsync-part").exists());
    assert!(
        matches!(&r.problems[..], [(p, Problem::PartFile)] if p == "a.hopsync-part"),
        "{:?}",
        r.problems
    );
}
