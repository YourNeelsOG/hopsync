//! Engine features the job scheduler needs (Phase 2 step 1): per-file outcomes, a dry run that
//! shares the planner, and cancellation that leaves a resumable transfer.

mod support;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hopsync::engine::{
    ItemFn, ItemOutcome, Remote, TransferError, TransferOptions, push_file, push_tree,
};
use hopsync::journal::Journal;
use hopsync::paths::{LocalRoot, RelPath, RemoteRoot};
use hopsync::plan::PlanConfig;

struct Env {
    sshd: support::TestSshd,
    pins: tempfile::TempDir,
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
        pins,
        local_dir,
        local,
    })
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).expect("rel")
}

/// What each file ended as, by source path: "sent <bytes>", "plan <bytes>[ replace]",
/// "unchanged", or the problem text.
type Seen = Arc<Mutex<BTreeMap<String, String>>>;

fn recorder() -> (Seen, ItemFn) {
    let seen: Seen = Arc::default();
    let s = seen.clone();
    let f: ItemFn = Arc::new(move |path: &str, out: ItemOutcome<'_>| {
        let text = match out {
            ItemOutcome::Sent { bytes, .. } => format!("sent {bytes}"),
            ItemOutcome::Planned { bytes, replace, .. } => {
                format!("plan {bytes}{}", if replace { " replace" } else { "" })
            }
            ItemOutcome::Unchanged => "unchanged".to_owned(),
            ItemOutcome::Problem(p) => p.to_string(),
        };
        s.lock().expect("lock").insert(path.to_owned(), text);
    });
    (seen, f)
}

fn build_tree(base: &std::path::Path) {
    std::fs::create_dir_all(base.join("t/sub/deeper")).expect("mkdir");
    std::fs::write(base.join("t/a.txt"), b"alpha").expect("write");
    std::fs::write(base.join("t/sub/b.bin"), vec![7u8; 3000]).expect("write");
    std::fs::write(base.join("t/sub/deeper/zero"), b"").expect("write");
    std::fs::write(base.join("t/keep.txt"), b"new").expect("write");
    std::os::unix::fs::symlink("a.txt", base.join("t/link")).expect("symlink");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dry_run_reports_what_the_real_run_then_does_and_moves_nothing() {
    let Some(e) = env() else { return };
    build_tree(e.local_dir.path());
    // One destination file exists already, so the default policy (skip) keeps it.
    let dst = e.sshd.dir().join("remote/t");
    std::fs::create_dir(&dst).expect("mkdir");
    std::fs::write(dst.join("keep.txt"), b"old").expect("write");

    let (planned, on_item) = recorder();
    let dry = TransferOptions {
        dry_run: true,
        on_item: Some(on_item),
        ..TransferOptions::default()
    };
    let report = push_tree(&e.local, &rel("t"), &e.remote, &rel("t"), &dry)
        .await
        .expect("dry run");
    assert_eq!(report.files, 3, "files that would be sent");
    assert_eq!(report.bytes, 5 + 3000, "bytes that would be sent");
    assert!(
        !e.sshd.dir().join("remote/t/sub").exists(),
        "a dry run creates no directory"
    );
    assert_eq!(
        std::fs::read_dir(&dst).expect("list").count(),
        1,
        "a dry run writes no file"
    );
    let planned = planned.lock().expect("lock").clone();
    assert_eq!(
        planned,
        BTreeMap::from([
            ("a.txt".to_owned(), "plan 5".to_owned()),
            (
                "keep.txt".to_owned(),
                "skipped: destination exists".to_owned()
            ),
            ("link".to_owned(), "skipped: symlink".to_owned()),
            ("sub/b.bin".to_owned(), "plan 3000".to_owned()),
            ("sub/deeper/zero".to_owned(), "plan 0".to_owned()),
        ])
    );

    let (done, on_item) = recorder();
    let real = TransferOptions {
        on_item: Some(on_item),
        ..TransferOptions::default()
    };
    push_tree(&e.local, &rel("t"), &e.remote, &rel("t"), &real)
        .await
        .expect("real run");
    let done = done.lock().expect("lock").clone();
    let as_planned: BTreeMap<String, String> = done
        .iter()
        .map(|(k, v)| (k.clone(), v.replacen("sent", "plan", 1)))
        .collect();
    assert_eq!(as_planned, planned, "the run did what the dry run said");
    assert_eq!(
        std::fs::read(dst.join("sub/b.bin")).expect("read"),
        vec![7u8; 3000]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dry_run_of_one_file_names_the_replace() {
    let Some(e) = env() else { return };
    std::fs::write(e.local_dir.path().join("f"), b"0123456789").expect("write");
    std::fs::write(e.sshd.dir().join("remote/f"), b"old").expect("write");
    let (seen, on_item) = recorder();
    let opts = TransferOptions {
        dry_run: true,
        existing: hopsync::policy::Existing::Overwrite,
        on_item: Some(on_item),
        ..TransferOptions::default()
    };
    let report = push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &opts)
        .await
        .expect("dry run");
    assert_eq!(report.bytes, 10);
    assert_eq!(
        std::fs::read(e.sshd.dir().join("remote/f")).expect("read"),
        b"old"
    );
    assert_eq!(
        seen.lock().expect("lock").get("f").map(String::as_str),
        Some("plan 10 replace")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_stops_quickly_and_the_next_run_resumes() {
    let Some(e) = env() else { return };
    let size = 96usize << 20;
    let data: Vec<u8> = (0..size).map(|i| (i % 253) as u8).collect();
    std::fs::write(e.local_dir.path().join("big"), &data).expect("write");
    let journal = Arc::new(Journal::open(&e.pins.path().join("journal.sqlite")).expect("journal"));
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let cancel_tx = Arc::new(cancel_tx);
    let sent = Arc::new(AtomicU64::new(0));
    let cancelled_at = Arc::new(Mutex::new(None::<Instant>));
    let progress = {
        let (sent, cancel_tx, at) = (sent.clone(), cancel_tx.clone(), cancelled_at.clone());
        Arc::new(move |n: u64| {
            if sent.fetch_add(n, Ordering::Relaxed) + n > 40 << 20 && !*cancel_tx.borrow() {
                *at.lock().expect("lock") = Some(Instant::now());
                cancel_tx.send_replace(true);
            }
        })
    };
    let opts = TransferOptions {
        lanes: 2,
        max_lanes: 2,
        plan: PlanConfig {
            lanes: 2,
            min_chunk: 8 << 20,
            max_chunk: 8 << 20,
        },
        journal: Some(journal.clone()),
        cancel: Some(cancel_rx),
        progress: Some(progress),
        ..TransferOptions::default()
    };
    let err = push_file(&e.local, &rel("big"), &e.remote, &rel("big"), &opts)
        .await
        .expect_err("cancelled");
    let stopped = cancelled_at
        .lock()
        .expect("lock")
        .expect("cancel was sent")
        .elapsed();
    assert!(matches!(err, TransferError::Cancelled), "got {err}");
    assert!(stopped < Duration::from_secs(2), "took {stopped:?} to stop");
    assert!(
        !e.sshd.dir().join("remote/big").exists(),
        "nothing renamed into place"
    );
    assert!(
        e.sshd.dir().join("remote/big.hopsync-part").exists(),
        "the part file stays for the resume"
    );

    let again = TransferOptions {
        cancel: None,
        progress: None,
        ..opts
    };
    let report = push_file(&e.local, &rel("big"), &e.remote, &rel("big"), &again)
        .await
        .expect("resumed run");
    assert!(report.resumed > 0, "the second run resumed durable chunks");
    assert!(std::fs::read(e.sshd.dir().join("remote/big")).expect("read") == data);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_before_the_start_sends_nothing() {
    let Some(e) = env() else { return };
    build_tree(e.local_dir.path());
    let (_tx, rx) = tokio::sync::watch::channel(true);
    let opts = TransferOptions {
        cancel: Some(rx),
        ..TransferOptions::default()
    };
    let err = push_tree(&e.local, &rel("t"), &e.remote, &rel("t"), &opts)
        .await
        .expect_err("cancelled");
    assert!(matches!(err, TransferError::Cancelled), "got {err}");
    assert!(!e.sshd.dir().join("remote/t/a.txt").exists());
}
