//! An idle daemon must hold almost no memory after a transfer or a login. Freed buffers otherwise
//! stay in glibc's per-thread arenas: measured on a real box, 105 MiB of anonymous memory remained
//! after a 2 GiB push, against 1.3 MiB before it.

#![cfg(all(target_os = "linux", target_env = "gnu"))]

use hopsync::pool::release_free_memory;

fn anon_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("RssAnon:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
fn freed_transfer_buffers_go_back_to_the_kernel() {
    // A worker thread, so the blocks come from a thread arena as they do under tokio. Each is freed
    // and allocated again first, which raises glibc's mmap threshold above this size, as a pool
    // reused across lanes does. The small blocks outlive them and pin the arena's pages, as a live
    // daemon's other allocations do, so freeing alone cannot shrink it.
    let (filled, _pin) = std::thread::spawn(|| {
        for _ in 0..4 {
            drop(vec![1u8; 1 << 20]);
        }
        let mut blocks = Vec::new();
        let mut pin = Vec::new();
        for _ in 0..128 {
            blocks.push(vec![1u8; 1 << 20]);
            pin.push(vec![1u8; 4096]);
        }
        let peak = anon_kib();
        drop(blocks);
        (peak, pin)
    })
    .join()
    .unwrap();
    assert!(filled >= 100 * 1024, "only {filled} KiB was allocated");

    release_free_memory();

    let after = anon_kib();
    assert!(
        after < filled / 4,
        "{after} KiB still held after freeing and releasing, {filled} KiB at the peak"
    );
}
