# HDD throughput plan: disk-ordered streaming

Status: approved 2026-11-09 (owner: "Approve, start"); steps 1 to 7 built 2026-11-09, results below.
Evidence: `docs/benchmarks/2026-11-08-hdd/` (to be added with the first step).
Design it carries forward: plan.md §6.2 (bounded memory), §6.3 (sequential reads, checkpoint fsync),
§6.4 (peer mode later), CLAUDE.md §4 (performance budget).

## Problem

A 1.18 TB push from SH_EU (`seedhost`) to HDD5TB (`hdd5tib`) took 14.5 h: 21.6 MiB/s overall, 9 to
15 MiB/s per job with 2 jobs running. Both boxes sit in the same data centre: RTT 0.29 ms, iperf3
8.1 Gbit/s on one stream and 18.4 Gbit/s on 8. Both disks are a single shared Toshiba MG06 HDD.

## Measurements (2026-11-08, same file, same hour)

Ceilings:

| Path | Result |
|---|---|
| Network, iperf3 1 / 8 streams | 8.1 / 18.4 Gbit/s |
| One OpenSSH stream, aes128-gcm / chacha20 | 407 / 272 MiB/s |
| SH_EU disk read, 16 MiB blocks, buffered / O_DIRECT | 220 / 223 MiB/s |
| SH_EU disk write, 16 MiB + fdatasync (shared, busy) | 74 MiB/s |
| HDD5TB disk write, 16 MiB O_DIRECT / buffered + fdatasync | 160 / 105 MiB/s |
| HDD5TB disk read, 16 MiB buffered | 153 MiB/s |
| Ideal pipe: `dd iflag=direct bs=16M \| ssh \| dd bs=16M conv=fdatasync`, 32 GiB | **123.2 MiB/s** end to end (4 m 26 s), 28 % CPU; source 68 % busy, 8 % iowait; destination 75 % busy, over 90 % in 96 of 133 samples |

The ideal pipe is limited by the destination disk, as it should be: one sequential stream each side,
and HDD5TB's write-back is the slowest part. That is the number Hopsync is measured against.

32 GiB file, SH_EU to HDD5TB, page cache dropped before each run:

| | rclone 1.53 (1 stream) | Hopsync (default, ended on 6 lanes) |
|---|---|---|
| Time | 8 m 54 s | 15 m 02 s |
| Average / peak | 61.4 / 93.2 MiB/s | 36.3 / 56.5 MiB/s |
| CPU (of one core) | 44 % | 14 % |
| Peak RSS | 70 MiB | 38 MiB |
| Source disk busy / iowait | 37 % / 10 % | 72 % / 48 % |
| Source reads | 501/s × 128 KiB | 314/s × 120 KiB |
| Destination write avg, busy | 59.3 MiB/s, 37 % | 36.2 MiB/s, 20 % |

## Root cause

1. **Disk parallelism instead of disk order.** A file is cut into up to 16 chunks of 8 to 64 MiB and
   every lane reads its own chunk, 16 pieces of about 256 KiB in flight each, through one shared file
   descriptor. The kernel's readahead state is per open file, so it sees random reads and stops
   reading ahead. The disk seeks between chunks: 72 % busy for 37 MiB/s, where rclone's single
   in-order stream took 37 % busy for 62 MiB/s.
2. **Too many streams per job and across jobs.** `lanes` 4 auto-tuning to 16, `per_lane` 4: up to 64
   items in flight per job, 2 jobs at once, all on the same source disk. Up to 512 blocking threads;
   405 were seen in D state on 2026-11-08.
3. **An fsync per chunk.** Daemon jobs always journal, and `record_durable` sends `fsync@openssh.com`
   after every 8 to 64 MiB chunk. plan.md §6.3 asked for checkpoints every few hundred MiB.
4. **Drop-behind per piece.** `fadvise(DONTNEED)` runs on every 256 KiB piece, which is many syscalls
   and fights any readahead the kernel still does.
5. Not the cause: network, SSH cipher, hashing (off by default) and CPU (14 % of one core).

## How the fast tools do it

- **rsync:** one file at a time, one sequential reader (256 KiB map window), 256 KiB buffered writes,
  no fsync unless `--fsync`, `--preallocate` opt-in.
- **rclone:** sequential reader with an async read-ahead buffer (`--buffer-size`); SFTP upload keeps
  up to 64 requests in flight (`concurrency`) at increasing offsets; multi-thread only above
  `--multi-thread-cutoff` 256 MiB with 4 streams of 64 MiB chunks and preallocation on local
  destinations.
- **qBittorrent / libtorrent:** pieces arrive in random order, but writes land in RAM first: a disk
  queue (`DiskQueueSize` 1 GiB on both boxes), then the page cache. libtorrent 2.0 writes through
  mmap and lets the kernel sort dirty pages by offset; preallocation (on at SH_EU) keeps the file in
  one extent. Both boxes allow 20 % of RAM dirty (about 50 to 75 GiB), so a download can run at
  200 to 300 MiB/s for tens of GiB before write-back sets the pace.

The lesson: **writes can be absorbed and reordered by RAM; reads cannot.** A read waits for the disk
at the moment it is issued, so the reading side must be sequential and large. The writing side mainly
needs offsets that move forward so the kernel can merge them.

## Target

| Case | Target |
|---|---|
| Single 32 GiB file, SH_EU to HDD5TB | at least 85 % of the ideal pipe (105 MiB/s or more); source disk busy no more than the pipe's 68 % + 10 points |
| 1 TB tree like the real run | at least 75 % of the ideal pipe sustained (92 MiB/s), about 3 h instead of 14.5 h |
| High-RTT link (Phase 0 seedbox, 203 ms) | no regression against `docs/benchmarks/results.json` |
| Memory | default peak under 256 MiB for any tree; configurable |
| CPU | under 50 % of one core at 200 MiB/s |

## Design

### D1. One sequential reader per file (push)

- One blocking task per active file reads `read_block` (default 16 MiB) at a time, in order, into
  buffers from the pool. `fadvise(SEQUENTIAL)` on open; `DONTNEED` on what was sent, in 64 MiB steps.
- `O_DIRECT` when `io_direct = auto` and the filesystem accepts it (aligned buffers, fallback to
  buffered on `EINVAL`). The 16 MiB read test shows equal speed either way, so `auto` is about
  leaving the seedbox's page cache alone, not speed.
- A bounded queue of `read_ahead` (default 64 MiB, 4 blocks) per file. A full queue stops the reader,
  so memory stays fixed.

### D2. Lanes carry buffers; they do not pick file offsets

- Each 16 MiB block is cut into SFTP writes of the server's `max_write` (255 KiB on OpenSSH) and
  dispatched to the next lane with free pipeline room, up to `inflight` requests per lane.
- All writes in flight for a file sit inside one sliding window (the blocks in the queue), so the
  destination sees offsets that only move forward a few MiB at a time. sftp-server writes to its page
  cache, and the kernel merges these into large sequential write-back.
- The chunk plan stays for resume bookkeeping only; it no longer decides which lane reads where.
- Lane auto-tune stays (it is what a 203 ms link needs), but starts at 1 lane when RTT is under 5 ms
  (one OpenSSH stream carries about 400 MiB/s), and stops adding lanes when the source disk's read
  wait rises (D4).

### D3. File scheduler per source disk

- Files are grouped by source device (`st_dev`). Per device: at most `big_files_per_disk` (default 1)
  file of `big_file` (default 64 MiB) or more streams at a time, plus up to `small_files` (default 8)
  small files overlapping their round trips.
- The limit is daemon-wide, not per job: two queued jobs reading the same disk share it instead of
  doubling the seeks. Different disks run in parallel.
- Blocking threads capped (`max_blocking_threads` = 2 × active files + 4) so a slow disk cannot pile up
  hundreds of threads.

### D4. I/O governor

- Every 2 s, read `/proc/diskstats` for the source device (no root needed): read throughput, request
  wait (`await`), and queue depth.
- If our reads wait more than `io_wait_target` (default 40 ms) for 10 s, shrink the small-file set
  and stop adding lanes; grow back when it falls. On shared boxes this keeps Hopsync from starving the
  torrent client, and it reacts to the disk's real response time rather than "% busy", which other
  tenants also fill.
- `io_governor = off` disables it.

### D5. Durability checkpoints, not fsync per chunk

- Resume becomes "contiguous bytes done" per file instead of a chunk set: the reader is sequential, so
  progress is a prefix.
- `fsync@openssh.com` and a journal record every `checkpoint` (default 1 GiB) per file, plus the
  existing fsync before rename for files of 1 MiB and more. A crash redoes at most 1 GiB per file.
- Servers without `fsync@` keep today's rule: no records, so a resume redoes the file.

### D6. Pull side (local writes) mirrors it

- Lanes read remote windows in order; one writer thread per file writes 16 MiB blocks, keeping the
  existing `sync_file_range` pacing and drop-behind.
- `fallocate` the part file to its final size before the first write (we own the local disk), so a
  big file lands in few extents. On `EOPNOTSUPP`, carry on without.

### D7. Peer receiver (phase B, separate approval and security review)

- Over plain SFTP we cannot preallocate, pace write-back or drop pages on the remote side: OpenSSH
  sftp-server has no `fallocate` extension. When Hopsync is installed on the remote too (plan.md
  §6.4), `hopsync receive` runs as the SSH session's fixed command, confined to its root, no shell,
  and writes with D6's discipline.
- Only worth building if phase A leaves a measurable gap to the ideal pipe at the destination.

### D8. Progress (task 4 of the request)

- Total bytes and file count known up front: a push already walks the local tree; run the walk ahead
  of the transfer (listing only, bounded memory: counts and sums, not the file list), and send
  `total_bytes`, `total_files` before the first byte. For a pull, the remote walk's totals arrive as it
  lists; the UI shows "counting" until it ends.
- Per-file progress: bytes done per active file in the snapshot (bounded to active files).
- Speed: exponential moving average over 2 s samples (half-life 10 s); ETA = remaining / smoothed
  speed, hidden for the first 10 s and when speed is near zero.
- Snapshot adds `files_done/files_total`, `bytes_done/bytes_total`, `percent`, `eta_secs`.

## New options and defaults

Planned options above; this is what was built (2026-11-09). Options without a CLI flag are fields of
`TransferOptions` and use these defaults in the daemon too.

| Option | Default | Where |
|---|---|---|
| Chunk size (`PlanConfig` min and max) | 16 MiB (was 8 to 64 MiB, size / lanes) | engine |
| `read_block` | 16 MiB | engine |
| `read_ahead` | 128 MiB, in reused buffers | CLI `--read-ahead SIZE` |
| `checkpoint` | 1 GiB per file (0 = every chunk) | CLI `--checkpoint SIZE` |
| `big_file` | 64 MiB: files this big take the source disk's single turn | engine |
| `io_wait_target` | 40 ms (0 or None = governor off) | CLI `--io-wait-target MS` |
| Durability session request timeout | 30 min | engine constant |
| Blocking threads | 64 (was 512) | runtime |
| Pull part file `fallocate` | always, `KEEP_SIZE`, errors ignored | engine |

## As built: differences from the design

- D2: lanes still take whole chunks from the queue, but the producer reads each chunk before
  queueing it, in order, under the read-ahead budget. Chunks are 16 MiB, so writes in flight stay
  within about `read_ahead` of each other. Lane auto-tune is unchanged; the "1 lane under 5 ms RTT"
  start was not built (4 lanes at 0.29 ms RTT reached 173 MiB/s).
- D3: one turn per source disk for files of `big_file` or more, process-wide. No separate per-disk
  limit on small files.
- D4: the governor paces reads (a pause before each 16 MiB read) instead of shrinking file sets.
- D5: the chunk-set journal stays; chunks are recorded in batches of `checkpoint` bytes with one
  `fsync@` per batch, over a separate SFTP session with a 30 min request timeout (sftp-server is
  serial, and a multi-GiB fsync outlasted the lane's 60 s timeout). No journal migration needed.
- D1: no `O_DIRECT` (measured equal speed); reads already drop their pages after use.
- D7 (peer receiver) not started.
- Known gap: no automated test reproduces an fsync slower than the request timeout; it was found and
  checked on the real boxes.

## Edge cases

Source file growing or shrinking mid-read (existing size and mtime re-check, plus short read =
`SourceChanged`); `O_DIRECT` refused (tmpfs, some FUSE) or unaligned tail; a disk shared by two jobs;
many tiny files (round-trip bound, not seek bound: small-file overlap must stay); sparse sources (read
as data, as today); resume of a part written by the old chunk-set journal (migrate: largest contiguous
prefix, rest redone); remote without `fsync@`; full destination mid-file (existing min-free check);
very slow remote (the read queue fills and the reader waits: memory stays fixed); lane lost mid-window
(its unacknowledged writes are re-sent from buffers still held; buffers are released only after every
write in them is acknowledged); cancel while the reader is blocked on disk.

## Results (2026-11-09)

Same 32 GiB file and boxes, four runs back to back (`docs/benchmarks/2026-11-09-hdd-after/`):

| Run | MiB/s | Source busy / iowait | Destination busy | CPU | Max RSS |
|---|---|---|---|---|---|
| Ideal pipe | 123.7 | 65 % / 11 % | 62 % | 30 % | 18 MB |
| rclone 1.53 | 59.5 | 38 % / 10 % | 32 % | 46 % | 65 MB |
| Hopsync before | 35.2 | 72 % / 47 % | 22 % | 15 % | 52 MB |
| Hopsync after | 155.0 | 69 % / 8 % | 90 % | 57 % | 162 MB |

Against the targets: single file met (155 MiB/s against 105; source 69 % busy against at most 78 %);
memory met (162 MB peak, under 256 MiB); CPU missed (57 % of one core at 155 MiB/s, so about 74 % at
200, against a target under 50 %). The bottleneck moved to the destination disk. D7 (peer receiver) is not needed for this pair: the
destination already takes writes at its buffered speed. Not measured yet: a slice of the real tree
with many files, and a high-RTT link.

## Steps (each ends verified; failing test first)

1. Benchmark harness in-repo: `benches/hdd/` scripts (monitor, runner, ideal pipe) and the 2026-11-08
   results; Decision log entries for D1 to D5.
2. Sequential reader with bounded read-ahead (D1), unit tests for ordering, short reads, cancel.
3. Lane dispatch from the reader's window (D2), push path; resume journal moves to a contiguous prefix
   with checkpoints (D5); migration from chunk sets.
4. Per-disk file scheduler shared across jobs, blocking-thread cap (D3).
5. Pull side: in-order writer and `fallocate` (D6).
6. I/O governor (D4).
7. Progress totals, per-file progress, smoothed speed and ETA in API, CLI and web UI (D8).
8. Re-run the matrix: 32 GiB single file, a 100 to 200 GB slice of the real tree, loopback, high-RTT
   seedbox. Report before/after against the targets above. Decide on D7 from the numbers.

## Open questions for the owner

- qBittorrent's 200 to 300 MiB/s on these boxes is largely RAM absorbing writes (1 GiB disk queue
  plus about 50 GiB of allowed dirty pages). Hopsync's push gets the same absorption on the
  destination for free; the source read is what must change. Agree that the target is "ideal pipe",
  not "qBittorrent's peak"?
- Phase B (peer receiver) needs Hopsync on the remote box: acceptable for HDD5TB-type setups?
