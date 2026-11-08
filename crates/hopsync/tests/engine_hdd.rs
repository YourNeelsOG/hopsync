//! Disk-ordered pushes (docs/plans/2026-11-08-hdd-throughput-plan.md, D1 and D2): the source is read
//! once, in order, in large blocks, however many lanes carry the data.
//!
//! On 2026-11-08 a push read its source in 256 KiB pieces at scattered offsets, one stream per lane,
//! which left a hard drive seeking: 72 % busy for 37 MiB/s where an in-order reader got 138 MiB/s.

mod support;

use std::sync::{Arc, Mutex};

use hopsync::engine::{Remote, TransferOptions, push_file, push_tree};
use hopsync::paths::{LocalRoot, RelPath, RemoteRoot};

const MIB: usize = 1 << 20;

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

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).expect("rel")
}

fn pattern(len: usize, seed: u32) -> Vec<u8> {
    (0..len)
        .map(|i| {
            (i as u32)
                .wrapping_mul(2_654_435_761)
                .wrapping_add(seed)
                .to_le_bytes()[1]
        })
        .collect()
}

type Reads = Arc<Mutex<Vec<(String, u64, usize)>>>;

/// Options that record every source read.
fn recording(lanes: u32) -> (TransferOptions, Reads) {
    let reads: Reads = Arc::default();
    let o = TransferOptions {
        lanes,
        max_lanes: lanes,
        on_read: Some(Arc::new({
            let reads = reads.clone();
            move |path: &str, offset, len| {
                reads
                    .lock()
                    .expect("reads")
                    .push((path.to_owned(), offset, len));
            }
        })),
        ..TransferOptions::default()
    };
    (o, reads)
}

/// The reads of one file must cover it exactly once, front to back, in blocks of `block` bytes
/// (the last may be shorter).
fn assert_in_order(reads: &[(u64, usize)], size: u64, block: usize) {
    let mut next = 0u64;
    for (i, &(offset, len)) in reads.iter().enumerate() {
        assert_eq!(
            offset, next,
            "read {i} at {offset}, expected {next}: {reads:?}"
        );
        let last = offset + len as u64 == size;
        assert!(
            len == block || last,
            "read {i} is {len} bytes, not a {block}-byte block: {reads:?}"
        );
        next += len as u64;
    }
    assert_eq!(next, size, "reads stop at {next} of {size}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_reads_the_source_once_in_order_in_big_blocks() {
    let Some(e) = env() else { return };
    let size = 40 * MIB + 123;
    let data = pattern(size, 7);
    std::fs::write(e.local_dir.path().join("big"), &data).expect("write");
    let (o, reads) = recording(4);
    let block = o.read_block;
    assert_eq!(block, 16 * MIB, "default read block");

    push_file(&e.local, &rel("big"), &e.remote, &rel("big"), &o)
        .await
        .expect("push");

    assert_eq!(
        std::fs::read(e.sshd.dir().join("remote/big")).expect("read"),
        data
    );
    let reads: Vec<_> = reads
        .lock()
        .expect("reads")
        .iter()
        .map(|r| (r.1, r.2))
        .collect();
    assert_in_order(&reads, size as u64, block);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tree_is_read_one_file_after_another() {
    let Some(e) = env() else { return };
    let dir = e.local_dir.path().join("t");
    std::fs::create_dir(&dir).expect("mkdir");
    let sizes = [20 * MIB + 5, 3 * MIB, 0, 17 * MIB];
    for (i, s) in sizes.iter().enumerate() {
        std::fs::write(dir.join(format!("f{i}")), pattern(*s, i as u32)).expect("write");
    }
    let (o, reads) = recording(4);
    let block = o.read_block;

    push_tree(&e.local, &rel("t"), &e.remote, &rel("t"), &o)
        .await
        .expect("push tree");

    let reads = reads.lock().expect("reads").clone();
    // Files never interleave: once a file's reads start, they finish before the next file's.
    let mut order: Vec<&str> = Vec::new();
    for (p, ..) in &reads {
        if order.last() != Some(&p.as_str()) {
            assert!(
                !order.contains(&p.as_str()),
                "{p} read again after another file: {reads:?}"
            );
            order.push(p);
        }
    }
    for (i, s) in sizes.iter().enumerate() {
        let name = format!("f{i}");
        let own: Vec<_> = reads
            .iter()
            .filter(|r| r.0 == name)
            .map(|r| (r.1, r.2))
            .collect();
        assert_in_order(&own, *s as u64, block);
        assert_eq!(
            std::fs::read(e.sshd.dir().join("remote/t").join(&name)).expect("read"),
            pattern(*s, i as u32)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_ahead_budget_below_one_chunk_still_completes() {
    let Some(e) = env() else { return };
    let size = 33 * MIB;
    let data = pattern(size, 3);
    std::fs::write(e.local_dir.path().join("f"), &data).expect("write");
    let o = TransferOptions {
        lanes: 4,
        max_lanes: 4,
        read_ahead: 4 * MIB as u64,
        ..TransferOptions::default()
    };
    push_file(&e.local, &rel("f"), &e.remote, &rel("f"), &o)
        .await
        .expect("push");
    assert_eq!(
        std::fs::read(e.sshd.dir().join("remote/f")).expect("read"),
        data
    );
}

/// Two transfers reading big files from the same disk take turns, so the disk streams one file
/// instead of seeking between two (the daemon runs two jobs at once by default).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_transfers_take_turns_on_one_disk() {
    let Some(e) = env() else { return };
    let size = 20 * MIB;
    for name in ["a", "b"] {
        std::fs::write(
            e.local_dir.path().join(name),
            pattern(size, name.len() as u32),
        )
        .expect("write");
    }
    let (base, reads) = recording(2);
    let o = TransferOptions {
        big_file: MIB as u64,
        ..base
    };
    let (a, b) = (rel("a"), rel("b"));
    let (ra, rb) = tokio::join!(
        push_file(&e.local, &a, &e.remote, &a, &o),
        push_file(&e.local, &b, &e.remote, &b, &o),
    );
    ra.expect("push a");
    rb.expect("push b");
    let reads = reads.lock().expect("reads").clone();
    let switches = reads.windows(2).filter(|w| w[0].0 != w[1].0).count();
    assert_eq!(switches, 1, "the files' reads interleave: {reads:?}");
}

/// Per-file progress: each file's reported bytes climb to its size (never past it), so a UI can
/// show a bar per active file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_file_progress_reaches_each_file_size() {
    let Some(e) = env() else { return };
    let dir = e.local_dir.path().join("p");
    std::fs::create_dir(&dir).expect("mkdir");
    let sizes = [18 * MIB + 7, 2 * MIB, 1];
    for (i, s) in sizes.iter().enumerate() {
        std::fs::write(dir.join(format!("f{i}")), pattern(*s, i as u32)).expect("write");
    }
    let seen: Arc<Mutex<std::collections::HashMap<String, (u64, u64)>>> = Arc::default();
    let o = TransferOptions {
        on_file: Some(Arc::new({
            let seen = seen.clone();
            move |path: &str, done, size| {
                let mut s = seen.lock().expect("seen");
                let e = s.entry(path.to_owned()).or_insert((0, size));
                assert!(
                    done >= e.0,
                    "{path}: progress went back from {} to {done}",
                    e.0
                );
                assert!(done <= size, "{path}: {done} past its size {size}");
                *e = (done, size);
            }
        })),
        ..TransferOptions::default()
    };
    push_tree(&e.local, &rel("p"), &e.remote, &rel("p"), &o)
        .await
        .expect("push tree");
    let seen = seen.lock().expect("seen");
    for (i, s) in sizes.iter().enumerate() {
        assert_eq!(
            seen.get(&format!("f{i}")),
            Some(&(*s as u64, *s as u64)),
            "{seen:?}"
        );
    }
}
