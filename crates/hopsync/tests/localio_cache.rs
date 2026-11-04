//! Page-cache discipline of local I/O (plan.md §6.3), measured with util-linux `fincore`.
//!
//! Runs in `CARGO_TARGET_TMPDIR` because the page cache only behaves like this on a disk-backed
//! filesystem; on tmpfs the pages are the storage and cannot be dropped, so the tests skip there.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use hopsync::localio::{CachePolicy, LocalReader, LocalWriter, WINDOW};

const MIB: u64 = 1 << 20;
const FILE_SIZE: u64 = 128 * MIB;
const TMPFS_MAGIC: i64 = 0x0102_1994;

fn scratch() -> Option<tempfile::TempDir> {
    if Command::new("fincore").arg("--version").output().is_err() {
        eprintln!("skipping: util-linux fincore not installed");
        return None;
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
    let fs = rustix::fs::statfs(dir.path()).expect("statfs");
    if fs.f_type as i64 == TMPFS_MAGIC {
        eprintln!("skipping: {} is tmpfs", dir.path().display());
        return None;
    }
    Some(dir)
}

/// Bytes of `path` resident in the page cache.
fn resident(path: &Path) -> u64 {
    let out = Command::new("fincore")
        .args(["--bytes", "--noheadings", "--output", "RES"])
        .arg(path)
        .output()
        .expect("run fincore");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("fincore RES")
}

/// A synced file whose pages are cached, or None (after saying why) when memory pressure keeps
/// evicting it: then a drop cannot be told apart from an eviction. Seen after heavy disk writes
/// (the engine tests) left write-back congested.
fn make_cached_file(dir: &Path) -> Option<PathBuf> {
    let path = dir.join("src.bin");
    let mut f = std::fs::File::create(&path).expect("create");
    let block = vec![0x5au8; MIB as usize];
    for _ in 0..FILE_SIZE / MIB {
        f.write_all(&block).expect("write");
    }
    f.sync_all().expect("sync");
    // Pages can be reclaimed during write-back under pressure; a plain read brings them back.
    std::io::copy(
        &mut std::fs::File::open(&path).expect("open"),
        &mut std::io::sink(),
    )
    .expect("warm cache");
    if resident(&path) <= FILE_SIZE / 2 {
        eprintln!("skipping: memory pressure keeps evicting the test file");
        return None;
    }
    Some(path)
}

/// Reads the whole file, calling `check` every 16 MiB.
fn read_all(path: &Path, cache: CachePolicy, mut check: impl FnMut(u64)) {
    let r = LocalReader::new(std::fs::File::open(path).expect("open"), cache);
    let mut buf = vec![0u8; MIB as usize];
    let mut off = 0;
    loop {
        let n = r.read_at(off, &mut buf).expect("read");
        if n == 0 {
            break;
        }
        assert!(
            buf[..n].iter().all(|&b| b == 0x5a),
            "data mismatch at {off}"
        );
        off += n as u64;
        if off % (16 * MIB) == 0 {
            check(off);
        }
    }
    assert_eq!(off, FILE_SIZE);
}

#[test]
fn reading_with_drop_policy_leaves_little_cached() {
    let Some(dir) = scratch() else { return };
    let Some(path) = make_cached_file(dir.path()) else {
        return;
    };
    read_all(&path, CachePolicy::Drop, |_| {});
    let res = resident(&path);
    assert!(
        res <= 4 * MIB,
        "{} MiB still cached after a drop-policy read",
        res / MIB
    );
}

#[test]
fn reading_with_keep_policy_keeps_cache() {
    let Some(dir) = scratch() else { return };
    let Some(path) = make_cached_file(dir.path()) else {
        return;
    };
    // Measured during the read: freshly read pages sit on the inactive list, and a busy machine can
    // reclaim them right after, which says nothing about the policy. Drop leaves at most a few MiB
    // behind the cursor at any time.
    let mut best = 0;
    read_all(&path, CachePolicy::Keep, |_| {
        best = best.max(resident(&path))
    });
    assert!(
        best > 4 * WINDOW,
        "only {} MiB cached during a keep-policy read",
        best / MIB
    );
}

fn write_file(path: &Path, cache: CachePolicy, mut check: impl FnMut(u64)) {
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .expect("open");
    let w = LocalWriter::new(f, cache);
    let block = vec![0xa5u8; MIB as usize];
    let mut chunk = w.chunk(0);
    for i in 1..=FILE_SIZE / MIB {
        chunk.write(&block).expect("write");
        if i % 16 == 0 {
            check(i * MIB);
        }
    }
    chunk.finish().expect("finish");
}

#[test]
fn writing_with_drop_policy_bounds_cache_during_and_after() {
    let Some(dir) = scratch() else { return };
    let path = dir.path().join("dst.bin");
    let mut worst = 0;
    write_file(&path, CachePolicy::Drop, |_| {
        worst = worst.max(resident(&path))
    });
    assert!(
        worst <= 3 * WINDOW,
        "{} MiB resident mid-write; pacing should keep it near 2 windows ({} MiB)",
        worst / MIB,
        2 * WINDOW / MIB
    );
    let after = resident(&path);
    assert!(
        after <= MIB,
        "{} MiB still cached after finish",
        after / MIB
    );
    let data = std::fs::read(&path).expect("read back");
    assert_eq!(data.len() as u64, FILE_SIZE);
    assert!(data.iter().all(|&b| b == 0xa5));
}

#[test]
fn writing_with_keep_policy_keeps_cache_but_finishes_write_back() {
    let Some(dir) = scratch() else { return };
    let path = dir.path().join("dst.bin");
    // Measured during the write, for the same reason as the read test: drop never lets more than
    // 3 windows stay resident, keep goes well past that.
    let mut best = 0;
    write_file(&path, CachePolicy::Keep, |_| {
        best = best.max(resident(&path))
    });
    assert!(
        best > 4 * WINDOW,
        "only {} MiB cached during a keep-policy write: keep must not drop pages",
        best / MIB
    );
}

#[test]
fn chunks_at_offsets_compose_into_one_file() {
    let Some(dir) = scratch() else { return };
    let path = dir.path().join("pieces.bin");
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("open");
    let w = LocalWriter::new(f, CachePolicy::Drop);
    // Two chunks written out of order, like two lanes would.
    let mut second = w.chunk(10 * MIB);
    second
        .write(&vec![2u8; (10 * MIB) as usize])
        .expect("write");
    second.finish().expect("finish");
    let mut first = w.chunk(0);
    first.write(&vec![1u8; (10 * MIB) as usize]).expect("write");
    first.finish().expect("finish");
    w.sync_data().expect("fdatasync");
    let data = std::fs::read(&path).expect("read");
    assert_eq!(data.len() as u64, 20 * MIB);
    assert!(data[..(10 * MIB) as usize].iter().all(|&b| b == 1));
    assert!(data[(10 * MIB) as usize..].iter().all(|&b| b == 2));
}

#[test]
fn short_read_at_end_of_file() {
    let Some(dir) = scratch() else { return };
    let path = dir.path().join("small.bin");
    std::fs::write(&path, b"0123456789").expect("write");
    let r = LocalReader::new(std::fs::File::open(&path).expect("open"), CachePolicy::Drop);
    let mut buf = [0u8; 64];
    assert_eq!(r.read_at(4, &mut buf).expect("read"), 6);
    assert_eq!(r.read_at(10, &mut buf).expect("read"), 0);
    assert_eq!(r.size().expect("size"), 10);
}
