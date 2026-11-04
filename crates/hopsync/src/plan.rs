//! Chunk planning (plan.md §6.1): how a file is split into ranges that lanes transfer independently.
//!
//! Pure and streaming: `file_chunks` describes one file at a time, so the engine can plan a 100k-file
//! tree as it lists it instead of holding every chunk in memory.
//!
//! Chunk size per file is `size / lanes`, clamped to `[min_chunk, max_chunk]` and rounded up to
//! `ALIGN`, so a medium file still keeps every lane busy while a huge file uses big chunks (fewer
//! journal entries and fsync checkpoints). Offsets are multiples of `ALIGN` for optional `O_DIRECT`.

/// Chunk offsets and sizes (except a file's last chunk) are multiples of this.
pub const ALIGN: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanConfig {
    pub lanes: u32,
    pub min_chunk: u64,
    pub max_chunk: u64,
}

impl Default for PlanConfig {
    fn default() -> Self {
        Self {
            lanes: 4,
            min_chunk: 8 << 20,
            max_chunk: 64 << 20,
        }
    }
}

/// A byte range of one file. `len == 0` only for a zero-byte file, which still has to be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub offset: u64,
    pub len: u64,
}

impl Range {
    pub fn end(&self) -> u64 {
        self.offset + self.len
    }
}

/// Rounds up to a multiple of `ALIGN`, saturating at the largest multiple below `u64::MAX`.
pub(crate) fn align_up(x: u64) -> u64 {
    x.checked_next_multiple_of(ALIGN)
        .unwrap_or(u64::MAX / ALIGN * ALIGN)
}

/// The chunk size used for a file of `size` bytes. Always a non-zero multiple of `ALIGN`.
pub fn chunk_size(size: u64, cfg: &PlanConfig) -> u64 {
    let max = align_up(cfg.max_chunk.max(ALIGN));
    let min = align_up(cfg.min_chunk.max(ALIGN)).min(max);
    let lanes = u64::from(cfg.lanes.max(1));
    align_up(size.div_ceil(lanes)).clamp(min, max)
}

/// Number of chunks of a file of `size` bytes (1 for a zero-byte file).
pub fn chunk_count(size: u64, cfg: &PlanConfig) -> u64 {
    if size == 0 {
        1
    } else {
        size.div_ceil(chunk_size(size, cfg))
    }
}

/// Chunk `index` of a file of `size` bytes, or None past the last chunk. Chunks are addressable by
/// index so a resume journal can record them compactly.
pub fn chunk_at(size: u64, cfg: &PlanConfig, index: u64) -> Option<Range> {
    if index >= chunk_count(size, cfg) {
        return None;
    }
    if size == 0 {
        return Some(Range { offset: 0, len: 0 });
    }
    let cs = chunk_size(size, cfg);
    let offset = index.checked_mul(cs)?;
    Some(Range {
        offset,
        len: cs.min(size - offset),
    })
}

/// The ranges of a file of `size` bytes, in offset order, covering it exactly once.
pub fn file_chunks(size: u64, cfg: &PlanConfig) -> impl Iterator<Item = Range> {
    let cfg = *cfg;
    (0..chunk_count(size, &cfg)).map_while(move |i| chunk_at(size, &cfg, i))
}

/// The ranges of a file of `size` bytes at a fixed `chunk` size, for a resumed file that keeps the
/// chunk size it was first planned with. A zero `chunk` is treated as 1.
pub fn chunks_with(size: u64, chunk: u64) -> impl Iterator<Item = Range> {
    (0..count_with(size, chunk)).map(move |i| chunk_with(size, chunk, i))
}

/// How many chunks `chunks_with` yields, without producing them.
pub fn count_with(size: u64, chunk: u64) -> u64 {
    if size == 0 {
        1
    } else {
        size.div_ceil(chunk.max(1))
    }
}

/// Chunk `i` of `chunks_with`, computed directly. Past the end it is empty, at `size`.
pub fn chunk_with(size: u64, chunk: u64, i: u64) -> Range {
    let chunk = chunk.max(1);
    let offset = i.saturating_mul(chunk).min(size);
    Range {
        offset,
        len: chunk.min(size - offset),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    fn chunks(size: u64, cfg: &PlanConfig) -> Vec<Range> {
        file_chunks(size, cfg).collect()
    }

    /// The invariants every plan must hold, for any size and config.
    fn assert_valid(size: u64, cfg: &PlanConfig) {
        let cs = chunks(size, cfg);
        let ctx = format!("size={size} cfg={cfg:?}");
        if size == 0 {
            assert_eq!(cs, vec![Range { offset: 0, len: 0 }], "{ctx}");
            return;
        }
        assert!(!cs.is_empty(), "{ctx}");
        assert_eq!(cs[0].offset, 0, "{ctx}");
        for w in cs.windows(2) {
            assert_eq!(w[0].end(), w[1].offset, "contiguous, no overlap: {ctx}");
        }
        assert_eq!(
            cs.last().map(Range::end),
            Some(size),
            "covers the whole file: {ctx}"
        );
        let max = align_up(cfg.max_chunk.max(ALIGN));
        for c in &cs {
            assert!(
                c.len > 0 && c.len <= max,
                "chunk len {} out of range: {ctx}",
                c.len
            );
            assert_eq!(c.offset % ALIGN, 0, "aligned offset: {ctx}");
        }
        for c in &cs[..cs.len() - 1] {
            assert_eq!(
                c.len % ALIGN,
                0,
                "only the last chunk may be unaligned: {ctx}"
            );
        }
    }

    #[test]
    fn zero_byte_file_is_one_empty_chunk() {
        assert_eq!(
            chunks(0, &PlanConfig::default()),
            vec![Range { offset: 0, len: 0 }]
        );
    }

    #[test]
    fn small_file_is_one_chunk() {
        assert_eq!(
            chunks(1, &PlanConfig::default()),
            vec![Range { offset: 0, len: 1 }]
        );
        assert_eq!(
            chunks(5 * MIB, &PlanConfig::default()),
            vec![Range {
                offset: 0,
                len: 5 * MIB
            }]
        );
    }

    #[test]
    fn medium_file_is_split_across_lanes() {
        let cfg = PlanConfig {
            lanes: 8,
            ..PlanConfig::default()
        };
        // 100 MiB / 8 lanes = 12.5 MiB, rounded up to 13 MiB: 8 chunks, every lane busy.
        assert_eq!(chunk_size(100 * MIB, &cfg), 13 * MIB);
        assert_eq!(chunks(100 * MIB, &cfg).len(), 8);
    }

    #[test]
    fn huge_file_uses_max_chunk() {
        let cfg = PlanConfig::default();
        assert_eq!(chunk_size(50 * GIB, &cfg), 64 * MIB);
        assert_eq!(chunks(50 * GIB, &cfg).len(), 800);
    }

    #[test]
    fn exact_multiple_has_no_empty_tail() {
        let cfg = PlanConfig {
            lanes: 1,
            min_chunk: 8 * MIB,
            max_chunk: 8 * MIB,
        };
        let cs = chunks(24 * MIB, &cfg);
        assert_eq!(cs.len(), 3);
        assert!(cs.iter().all(|c| c.len == 8 * MIB));
    }

    #[test]
    fn odd_configs_are_sanitized() {
        // Zero lanes, inverted or unaligned bounds must still give a valid plan.
        for cfg in [
            PlanConfig {
                lanes: 0,
                min_chunk: 8 * MIB,
                max_chunk: 64 * MIB,
            },
            PlanConfig {
                lanes: 4,
                min_chunk: 64 * MIB,
                max_chunk: 8 * MIB,
            },
            PlanConfig {
                lanes: 4,
                min_chunk: 0,
                max_chunk: 0,
            },
            PlanConfig {
                lanes: 3,
                min_chunk: 1000,
                max_chunk: 3 * MIB + 7,
            },
            PlanConfig {
                lanes: u32::MAX,
                min_chunk: 1,
                max_chunk: u64::MAX,
            },
        ] {
            for size in [
                0,
                1,
                ALIGN - 1,
                ALIGN,
                ALIGN + 1,
                100 * MIB + 3,
                10 * GIB + 1,
            ] {
                assert_valid(size, &cfg);
            }
        }
    }

    #[test]
    fn invariants_hold_for_many_random_sizes_and_configs() {
        // Deterministic xorshift so failures reproduce; no property-test dependency needed.
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..2000 {
            let size = match next() % 4 {
                0 => next() % (4 * MIB),
                1 => next() % (2 * GIB),
                2 => next() % (200 * GIB),
                _ => (next() % 64) * ALIGN,
            };
            let cfg = PlanConfig {
                lanes: (next() % 33) as u32,
                min_chunk: next() % (128 * MIB),
                max_chunk: next() % (512 * MIB),
            };
            assert_valid(size, &cfg);
        }
    }

    #[test]
    fn largest_possible_file_does_not_overflow() {
        for cfg in [
            PlanConfig::default(),
            PlanConfig {
                lanes: 1,
                min_chunk: u64::MAX,
                max_chunk: u64::MAX,
            },
        ] {
            let size = u64::MAX - 5;
            let n = chunk_count(size, &cfg);
            let last = chunk_at(size, &cfg, n - 1).expect("last chunk");
            assert_eq!(last.end(), size, "{cfg:?}");
            assert_eq!(chunk_at(size, &cfg, n), None);
            assert_eq!(chunk_at(size, &cfg, 0).map(|c| c.offset), Some(0));
        }
    }

    #[test]
    fn chunk_at_matches_iteration() {
        let cfg = PlanConfig {
            lanes: 8,
            ..PlanConfig::default()
        };
        for size in [0, 1, 100 * MIB, 3 * GIB + 17] {
            let by_index: Vec<Range> = (0..chunk_count(size, &cfg))
                .filter_map(|i| chunk_at(size, &cfg, i))
                .collect();
            assert_eq!(by_index, chunks(size, &cfg), "size {size}");
        }
    }

    #[test]
    fn chunks_with_matches_the_planner_and_handles_edges() {
        let cfg = PlanConfig::default();
        for size in [0, 1, 100 * MIB, 3 * GIB + 17] {
            let fixed: Vec<Range> = chunks_with(size, chunk_size(size, &cfg)).collect();
            assert_eq!(fixed, chunks(size, &cfg), "size {size}");
        }
        assert_eq!(
            chunks_with(0, 0).collect::<Vec<_>>(),
            vec![Range { offset: 0, len: 0 }]
        );
        assert_eq!(chunks_with(5, 0).count(), 5);
        let size = 1024 * GIB + 5;
        let ranges: Vec<Range> = chunks_with(size, 64 * MIB).collect();
        assert_eq!(ranges.len(), 16_385);
        assert_eq!(
            ranges.last().map(|r| (r.offset, r.end())),
            Some((1024 * GIB, size))
        );
    }
}
