//! I/O governor (docs/plans/2026-11-08-hdd-throughput-plan.md, D4): slows a push's source reads
//! while the disk is contended, so a transfer does not starve the seedbox's torrent client.
//!
//! The signal is the disk's average read wait (`await`), from `/sys/dev/block/MAJ:MIN/stat`, which
//! needs no root. `%util` is useless on a shared box: other tenants fill it. A sequential stream
//! sees 1 to 2 ms on an idle hard drive (measured 2026-11-09); a disk thrashed by competing
//! readers sees tens of ms. Pacing starts only after the wait stays above the target for
//! `SUSTAIN` samples in a row, doubles while it stays high, and halves away once it falls.

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Samples are taken at most this often.
const INTERVAL: Duration = Duration::from_secs(2);
/// Consecutive slow samples before pacing starts: one busy moment is not contention.
const SUSTAIN: u32 = 3;
const FIRST_PAUSE: Duration = Duration::from_millis(50);
const MAX_PAUSE: Duration = Duration::from_secs(1);

/// The pacing decision, separate from where the numbers come from.
#[derive(Debug, Default)]
pub struct Pacer {
    slow: u32,
    pause: Duration,
}

impl Pacer {
    /// Takes one sample's average read wait; returns the pause to put before each read until the
    /// next sample.
    pub fn observe(&mut self, wait: Duration, target: Duration) -> Duration {
        if wait > target {
            self.slow += 1;
            if self.slow >= SUSTAIN {
                self.pause = (self.pause * 2).clamp(FIRST_PAUSE, MAX_PAUSE);
            }
        } else {
            self.slow = 0;
            if wait <= target / 2 {
                self.pause /= 2;
                if self.pause < FIRST_PAUSE / 4 {
                    self.pause = Duration::ZERO;
                }
            }
        }
        self.pause
    }
}

/// Completed reads and milliseconds spent on them, from a block device's `stat` file.
pub fn parse_stat(text: &str) -> Option<(u64, u64)> {
    let f: Vec<u64> = text
        .split_ascii_whitespace()
        .take(4)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    // Fields: reads completed, reads merged, sectors read, ms spent reading.
    (f.len() == 4).then(|| (f[0], f[3]))
}

/// Watches the disk a source file lives on.
pub struct Governor {
    stat: PathBuf,
    target: Duration,
    last: Option<(Instant, u64, u64)>,
    pacer: Pacer,
    pause: Duration,
}

impl Governor {
    /// None when the device has no block `stat` file (network filesystems, some FUSE and ZFS
    /// setups): no pacing then.
    pub fn for_device(dev: u64, target: Duration) -> Option<Self> {
        let stat = PathBuf::from(format!(
            "/sys/dev/block/{}:{}/stat",
            rustix::fs::major(dev),
            rustix::fs::minor(dev)
        ));
        std::fs::read_to_string(&stat)
            .ok()
            .as_deref()
            .and_then(parse_stat)?;
        Some(Self {
            stat,
            target,
            last: None,
            pacer: Pacer::default(),
            pause: Duration::ZERO,
        })
    }

    /// The pause to take before the next read. Samples the disk when the last sample is old.
    pub fn pause(&mut self) -> Duration {
        let now = Instant::now();
        if self.last.is_some_and(|(t, ..)| now - t < INTERVAL) {
            return self.pause;
        }
        let Some((reads, ms)) = std::fs::read_to_string(&self.stat)
            .ok()
            .as_deref()
            .and_then(parse_stat)
        else {
            return self.pause;
        };
        if let Some((_, r0, m0)) = self.last
            && reads > r0
        {
            let wait = Duration::from_millis(ms.saturating_sub(m0) / (reads - r0));
            self.pause = self.pacer.observe(wait, self.target);
        }
        self.last = Some((now, reads, ms));
        self.pause
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: Duration = Duration::from_millis(40);
    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_streaming_disk_is_never_paced() {
        let mut p = Pacer::default();
        for _ in 0..100 {
            assert_eq!(p.observe(2 * MS, TARGET), Duration::ZERO);
        }
    }

    #[test]
    fn pacing_starts_only_after_sustained_contention() {
        let mut p = Pacer::default();
        assert_eq!(p.observe(90 * MS, TARGET), Duration::ZERO);
        assert_eq!(p.observe(90 * MS, TARGET), Duration::ZERO);
        // A quiet sample resets the count.
        assert_eq!(p.observe(10 * MS, TARGET), Duration::ZERO);
        assert_eq!(p.observe(90 * MS, TARGET), Duration::ZERO);
        assert_eq!(p.observe(90 * MS, TARGET), Duration::ZERO);
        assert_eq!(p.observe(90 * MS, TARGET), 50 * MS);
        assert_eq!(p.observe(90 * MS, TARGET), 100 * MS);
    }

    #[test]
    fn pacing_is_capped_and_backs_off_once_the_disk_calms() {
        let mut p = Pacer::default();
        for _ in 0..20 {
            p.observe(200 * MS, TARGET);
        }
        assert_eq!(p.observe(200 * MS, TARGET), Duration::from_secs(1));
        // Between half the target and the target: hold.
        assert_eq!(p.observe(30 * MS, TARGET), Duration::from_secs(1));
        let mut last = Duration::from_secs(1);
        for _ in 0..10 {
            let now = p.observe(5 * MS, TARGET);
            assert!(now < last || now.is_zero());
            last = now;
        }
        assert_eq!(last, Duration::ZERO);
    }

    #[test]
    fn stat_lines_parse_reads_and_read_time() {
        let line = "  168431     1203 47280562   977456  1063612   238004 ...";
        assert_eq!(parse_stat(line), Some((168_431, 977_456)));
        assert_eq!(parse_stat("1 2 3"), None);
        assert_eq!(parse_stat("a b c d"), None);
    }
}
