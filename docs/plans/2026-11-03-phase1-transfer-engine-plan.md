# Phase 1 plan: transfer engine (CLI only)

Status: approved by owner 2026-11-03; steps 1 to 15 done (Phase 1 complete).
Design: `plan.md` §5 to §8. Phase 0 evidence: `docs/phase0/`.
Scope (plan.md §9, Phase 1): SSH pool, lanes, segmented chunks, pipelining, resume journal, cache and
I/O discipline, verification. `hopsync push` and `hopsync pull` work standalone; benchmarks meet targets.
Out of scope: daemon, SQLite store, secret vault, API, web UI, password auth (all Phase 2+).

## Decisions this plan depends on

Each is recorded in the plan.md Decision log before code that uses it.

| # | Need | Choice | Evidence |
|---|---|---|---|
| D1 | SSH + SFTP | `russh` + `russh-sftp` | `docs/phase0/loopback-results.md`, `docs/phase0/wan-results.md` |
| D2 | Async runtime | tokio, multi-thread runtime with an explicit small worker count (default 2, configurable) | loopback: 16 default workers cost 2 to 3× the CPU of 1 to 4 |
| D3 | Local I/O syscalls | `rustix` for `fadvise` and `openat2`; `libc::sync_file_range` behind one documented `unsafe` wrapper | rustix 1.1.6 and nix 0.31.3 lack `sync_file_range`; rustix already in the tree via tokio |
| D4 | CLI parsing | `clap` 4 (derive) | plan.md Decision log |
| D5 | Resume journal | SQLite via `rusqlite` (`bundled`) | plan.md Decision log |
| D6 | Inline hash | xxh3-128 via `twox-hash` | 0.017 vs 0.18 CPU-s/GB for blake3 |

## Module layout

One crate, `crates/hopsync`, split into `lib.rs` (engine, testable) and `main.rs` (CLI). Split into more
crates only when Phase 2 needs it.

| Module | Owns |
|---|---|
| `paths` | `RemotePath` (validated: no `..`, NUL, control chars, absolute escape), `LocalRoot` (confined opens via `openat2(RESOLVE_BENEATH \| RESOLVE_NO_SYMLINKS)`) |
| `hostkeys` | Hopsync's own known_hosts file; fingerprint display; mismatch is a hard error |
| `ssh` | connect, key auth, cipher preference (AES-GCM if the CPU has AES, else ChaCha20), keepalive, timeouts |
| `sftp` | session wrapper: honours `limits@`, bounded in-flight requests, read/write at offset, `fsync@`, `statvfs@`, `posix-rename@` |
| `plan` | pure: file list to chunks per lane; big files segmented, small files batched |
| `pool` | fixed-size buffer pool with backpressure |
| `localio` | positional read/write, `fadvise(DONTNEED)` behind the cursor, windowed `sync_file_range` write-back, optional `O_DIRECT` |
| `journal` | completed-chunk ranges per job, written only after data is durable |
| `engine` | lanes, scheduling, retries with backoff, progress events |
| `cli` | `push`, `pull`, `hosts trust`, progress output |

## Steps

Each step leaves the tree green (build, clippy `-D warnings`, fmt, tests, `cargo deny`). Production code
starts with a failing test. Riskiest first.

### 1. Close open decisions D4 to D6
- **Change:** research notes in `docs/phase1/`; Decision log rows in plan.md.
- **Test:** for D6, a throwaway benchmark of hash CPU per GB on this machine and the seedbox.
- **Done when:** D4 to D6 are in the Decision log with reasons.

### 2. Test harness: throwaway sshd fixture
- **Change:** `crates/hopsync/tests/support/`: start a user-mode `sshd` on a free port with generated keys
  (the Phase 0 setup), return connection details, kill on drop.
- **Test:** a smoke test connects with the system `ssh` client.
- **Done when:** integration tests can each get an isolated server; skipped with a clear message if
  `sshd` is missing.

### 3. Path validation (`paths`)
- **Change:** `RemotePath` parser and `LocalRoot`.
- **Test:** rejects `..`, absolute paths outside the root, NUL, newline and control characters, and
  symlink escapes (a symlink inside the root pointing out). Accepts unicode, spaces and long names.
- **Done when:** every path the engine touches goes through these types (enforced by type signatures).

### 4. Host-key pinning (`hostkeys`)
- **Change:** load and store Hopsync's own known_hosts; `hopsync hosts trust <host>` shows the fingerprint
  and asks for confirmation.
- **Test:** unknown key refused; pinned key accepted; changed key is a hard error naming both
  fingerprints; nothing is ever added without explicit confirmation.
- **Done when:** the `ssh` module cannot connect without a pinned key.
- **Done 2026-11-03:** `hostkeys` (`KnownHosts`, `trust`, `offered_key`), 14 unit tests and 3 tests
  against the sshd fixture. The `hopsync hosts trust` prompt is wired to `trust` in step 9, when the CLI
  exists. Step 5 must restrict host-key negotiation to `KnownHosts::pinned_algorithms` and refuse
  `Unknown` as well as `Mismatch`. Not handled: concurrent writers to the file (last rename wins);
  Phase 2's daemon is the single writer.

### 5. SSH connect and SFTP session (`ssh`, `sftp`)
- **Change:** connect with key auth, start the sftp subsystem, read `limits@`, cap request sizes.
- **Test:** against the fixture: an oversized request is split, never sent; a dropped connection surfaces
  as a typed error; timeouts fire on a stalled server.
- **Done when:** one lane can read and write at arbitrary offsets with N requests in flight.
- **Done 2026-11-03:** `ssh::connect` (pinned host keys only, negotiation limited to pinned
  algorithms, key file permission check, connect timeout, keepalive) and `sftp::{Sftp, RemoteFile}`
  (limits-based splitting, shared in-flight semaphore, EOF handling, `fsync@`, typed `Timeout` and
  `ConnectionLost`). 11 tests against the sshd fixture, stable over 6 runs. Not yet: `statvfs@` and
  `posix-rename@` (needed in steps 9 and 12), directory listing (step 10).

### 6. Chunk planner (`plan`)
- **Change:** pure function from file list and lane count to work items.
- **Test:** zero-byte files, a file smaller than one chunk, a file of exactly N chunks, 100k small
  files, total bytes conserved, no overlapping ranges.
- **Done when:** property tests pass.

### 7. Buffer pool (`pool`)
- **Change:** fixed number of fixed-size buffers; acquire waits when empty.
- **Test:** never allocates past the cap under concurrent load; release wakes a waiter.
- **Done when:** the engine can only get buffers from the pool.

### 8. Local I/O (`localio`)
- **Change:** positional reader and writer with cache discipline (plan.md §6.3).
- **Test:** after streaming a 1 GiB file, its pages are not resident (`mincore`) in `cache=drop` mode,
  and are in `cache=keep`; dirty pages stay below the write-back window.
- **Done when:** both modes pass on xfs and ext4 (tmpfs is skipped: no page cache to drop).
- **Done 2026-11-03:** `localio::{LocalReader, LocalWriter, ChunkWriter, CachePolicy}`. Reads drop the
  pages they read; `ChunkWriter` starts write-back every 8 MiB `WINDOW`, waits for the window before it
  and drops it, so mid-write residency stayed under 3 windows on a 128 MiB file (was 128 MiB without
  pacing). `finish` means written back, not durable: the journal calls `sync_data` at checkpoints.
  6 tests using util-linux `fincore`, run on xfs. Not verified on ext4 (no ext4 volume here); not done:
  `O_DIRECT` (optional, later). Fixed along the way: the sshd fixture's readiness probe could
  self-connect (ephemeral range 1024-65535) and report a server that had not bound yet; it now requires
  an `SSH-` banner from a peer that is not itself (regression test in `fixture_smoke.rs`).

### 9. Single-file push and pull (`engine`)
- **Change:** lanes over one file, `.hopsync-part` then `posix-rename@`, mtime preserved.
- **Test:** against the fixture: byte-identical result (hash both sides in the test), zero-byte file,
  destination exists, a lane dropped mid-transfer is retried.
- **Done when:** `hopsync push` and `hopsync pull` move one file correctly.
- **Done 2026-11-03:** `engine::{push_file, pull_file}`: lanes are separate SSH connections taking chunk
  indexes off a shared counter; each lane pipelines up to `inflight` pieces (`max_write`/`max_read`
  rounded down to 4 KiB so page drops never strand a boundary page). Lost connections and timeouts
  reconnect the lane and redo its chunk within a shared retry budget (default 8) with backoff; other
  errors abort. Finish: cut the part to the source size (a stale longer part cannot leak), fsync,
  mtime, rename (`posix-rename@` to overwrite, plain rename or `renameat2(NOREPLACE)` otherwise).
  Source size/mtime re-checked at the end. CLI: `push`, `pull`, `hosts trust|list|remove`
  (`--fingerprint` for non-interactive pinning; no TTY means no pin). 10 engine tests against the
  fixture (kills every sftp-server mid-transfer; a mutation that disables retries fails both kill
  tests), plus `sftp` metadata ops and `LocalRoot::{metadata, rename, sync_parent}` tests.
  Real runs: loopback 1 GiB push/pull byte-identical, RSS 32 to 34 MiB (local disk does 61.5 MB/s,
  which bounds these numbers); the seedbox: 256 MiB push/pull sha256-identical with mtime kept,
  1 GiB push at 8 lanes in 36.6 s (about 32 MB/s once data flows, the box's cap; about 3 s is two
  sequential connection setups at 203 ms RTT). Known limits: the remote's page cache fills with
  what sftp-server writes (SFTP has no fadvise); the CLI's directory probe costs one extra
  connection (fold into lane reuse, step 14); non-UTF-8 local names are refused for now.

### 10. Directory trees
- **Change:** streamed listing, directory creation, symlinks skipped by default and reported.
- **Test:** odd names (unicode, spaces, newlines), empty directories, permission denied mid-tree (partial
  success reported), a 10k-file tree.
- **Done when:** trees move with a per-file success/failure report.
- **Done 2026-11-03:** the engine became one machinery for files and trees: `lanes` reconnectable
  sessions, `lanes x per_lane` workers (default 4 per lane) pulling (file, chunk) items from a bounded
  channel fed by a streamed walk. `push_tree`/`pull_tree` return a `TreeReport` (counts plus
  per-entry problems: symlink, special, invalid name, exists, failed). Per-file errors fail only that
  file; SSH-level errors and an exhausted retry budget stop the transfer. Existing destination
  directories are listed once instead of one stat per file. Two bugs found and fixed before they
  shipped: (1) "am I the last chunk" checked before decrementing let two concurrent last chunks both
  skip finalizing (regression test `concurrent_last_chunks_finalize_every_file`; a mutation that
  reintroduces it fails 3 of 3 runs); (2) a finalize retry after a rename whose reply was lost would
  have reopened the part with CREATE and renamed zeros over the good file: now only the first open
  may create a part, and that retry checks the destination instead. 6 tree tests (unicode, space,
  newline, empty dir, zero-byte, multi-chunk, symlink, unreadable dir on both sides, existing files,
  10k files: push 3.5 s, pull 2.7 s on loopback). CLI: a directory source becomes a tree transfer;
  problems print with control characters escaped (an ESC in a name cannot reach the terminal); exit 1
  if any entry failed. Not done: directory mtimes are not preserved; mkdir is one round trip per
  directory (a deep tree of many small directories is latency-bound); non-UTF-8 remote names make
  russh-sftp fail that directory's listing.

### 11. Resume journal (`journal`)
- **Change:** record chunks only after durability (local `sync_file_range` WAIT; remote `fsync@`
  at checkpoints); on restart, re-stat the source and resume only missing ranges.
- **Test:** kill the process mid-transfer, restart, result identical; source changed between runs
  restarts the file instead of stitching.
- **Done when:** resume works for push and pull.
- **Done 2026-11-03:** `journal::Journal` (SQLite WAL, file 0600 in a 0700 dir, default
  `$XDG_STATE_HOME/hopsync/journal.sqlite`). Multi-chunk files get a row keyed by job (direction,
  account, both roots) and destination, with source size, mtime and chunk size. A chunk is recorded
  only after `fsync@` on the remote part (push) or `fdatasync` on the local part (pull); a server
  without `fsync@` gets no records. On the next run a matching identity resumes only missing chunks,
  using the recorded chunk size; a changed source, or a part file shorter than the recorded chunks,
  restarts the file. The row is removed after the rename. CLI: `--journal FILE`, `--no-resume`, and
  "resumed N MiB" in the summary. Tests: 5 journal unit tests; 4 engine tests (interrupted push and
  pull resume, changed source restarts, lost part file restarts; two mutations, "never resume" and
  "trust a lost part", each fail their tests); and `cli_resume`, which SIGKILLs the real binary
  once a chunk is journaled and checks the rerun resumes and is byte-identical. Not done: a
  journaled file whose source is gone stays in the journal until the same destination is
  transferred again (cleanup command: Phase 2 daemon).

### 12. Destination policy and free-space check
- **Change:** skip, overwrite, resume, rename policies; `statvfs` check before start.
- **Test:** each policy with existing same, older, newer and partial files; not enough space fails before
  any data moves.
- **Done 2026-11-04:** `policy::{Existing, decide, free_name}`, pure and unit tested. Policies:
  `skip` (default; a single file errors), `update` (replace when size or mtime differ, keep a
  destination newer than the source, report identical files as unchanged), `overwrite`, and
  `rename` (write `name (n).ext` beside the existing file, within 255 bytes). "Resume" is not a
  policy: the step 11 journal resumes any interrupted file, and `update` completes a short copy left
  by another tool. Trees list each pre-existing destination directory once instead of one `lstat`
  per file. Free space: `statvfs@openssh.com` (push) or `fstatvfs` (pull) minus `--min-free`,
  reduced by each queued file (resumed bytes excluded); a file that does not fit fails with
  `NoSpace` before any of its data moves. CLI: `--existing`, `--overwrite`, `--min-free 10G`.
  Tests: 5 policy unit tests, 6 engine tests (every policy against same, older, newer and partial
  files for push, `update` for pull, no space for file and tree in both directions) and a CLI run of
  each policy. Not done: no space check against a server without `statvfs@` (no portable way to ask);
  space used by other writers during the transfer is not tracked.

### 13. Inline hashing and optional verify
- **Change:** hash while reading the source; `--verify` re-reads the destination.
- **Test:** a corrupted destination byte is caught by `--verify`.
- **Done 2026-11-04:** with `verify` on, each chunk's xxh3-128 hash (`twox-hash`) is computed from
  the source bytes as they stream (pieces are joined in order, so no second source read), and right
  after the chunk is written it is read back from the destination and hashed again, before it is
  journaled and before the file is renamed. Checking per chunk spreads the read-back over every lane
  instead of one serial re-read at the end. Chunks resumed from an earlier run are queued as
  verify-only items, which re-read their source. A mismatch fails the file with `VerifyFailed`;
  once its last item is done, the journal forgets its chunks, so the next run redoes them. Push
  opens the part read-write only when verifying, so write-only upload directories still work
  without it. CLI: `--verify`; summaries say what was verified. Tests: 4 engine tests (push and
  pull verified; a part file damaged between an interrupted run and its resume is caught for push
  and pull and never renamed into place, and the following run starts over; empty and small files
  in trees). Four mutations (never compare, skip resumed chunks, keep the damaged journal rows, hash
  nothing inline) each fail the tests. Cost on loopback, release build, 1 GiB push: client CPU
  from 1.0 to 2.0 user-seconds (the read-back is decrypted), wall time unchanged at disk speed.
  Not done: hashes are not kept after the transfer (no manifest), and the read-back may be served
  from the destination's page cache, so it proves what the destination returns, not what is on the
  platter.

### 14. Lane auto-tune and server limits
- **Change:** start at a default lane count, add lanes while throughput rises, back off on refused
  connections (`MaxStartups`) and errors.
- **Test:** fixture with `MaxStartups 2` and `MaxSessions 2`: the engine backs off and still finishes.
- **Done 2026-11-04:** reproduced first: 16 lanes against `MaxStartups 2` failed every run with
  `RetriesExhausted`, because each refused lane start spent the transfer-wide retry budget. Now:
  at most 2 SSH handshakes run at once per transfer; a lane that never connected and is refused
  while another lane is up is retired without spending retries, and its workers move to a live
  lane (host key and key file errors stay fatal on every lane); a lane that was up and drops keeps
  the old reconnect-with-retries path, so a network blip does not shrink the transfer for good.
  Auto-tune (`tune::Tuner`, pure): lanes start at `lanes` and, with `max_lanes` above that, one is
  added at a time while each raises throughput by 10 %. A lane count is judged only at its plateau
  (two 2-second samples that no longer rise by 10 %), because the first seconds after a start or a
  new lane are TCP and window ramp-up: the first version, which judged single samples, credited
  that ramp-up to every new lane. Chunks are planned for the ceiling so added lanes find work.
  CLI: default starts at 4 and tunes up to `--max-lanes` (16); `-l N` fixes the count; summaries
  name the lanes used. Tests: 6 tuner unit tests (ramp-up, plateau, throttle, ceiling, idle,
  small gain) and 4 engine tests: `MaxStartups 2` plus `MaxSessions 1` with 16 lanes connects all
  16 with no retries (fails without the handshake limit: 3 lanes); a relay that closes connections
  past 3 retires lanes with a retry budget of 0 (fails without retirement); a tree under the same
  limits; and a blip that cuts every lane and refuses reconnects for 0.5 s keeps all 4 lanes
  (retiring lanes that were up hangs it).
  The blip test exposed a russh-sftp 3.0.1 race: a request sent between its read loop ending and its
  write loop noticing is never answered and waits out the 60 s request timeout (2 in 12 runs). The
  SFTP channel stream is now wrapped so end of stream, I/O errors and drop set a flag, and every
  request is raced against it: 0 in 24 runs with the guard, 2 in 24 with it disabled.
  On the seedbox (1 GiB push): 4 lanes plateaued near 27 MB/s, 5 near 31, 6 near 35, and
  7 peaked near 50 MB/s before the tail; the tuner stopped at 7. Not done: lanes are never removed
  once added, and a server-side cap that applies only after a burst can leave one more lane than
  needed.

### 15. Benchmarks and Phase 1 security review
- **Change:** `benches/` harness built from the Phase 0 spike; README numbers vs `scp`/`sftp`.
- **Test:** targets from plan.md §6.2 (idle RSS under 30 MiB, under 150 MiB at 1 Gbps).
- **Done when:** benchmark results are in `docs/phase1/`, and a security review of everything Phase 1
  added is written up and its findings fixed.
- **Done 2026-11-04:** `benches/bench.py` runs hopsync, `scp` and `sftp` on the same server and
  data (one big file and a small-file tree, push and pull), and records wall time and the peak RSS
  of each client's process tree, sampled from `/proc` (`wait4` would count the forking Python
  process). Results: `docs/phase1/benchmarks.md`; README numbers come from it. Peak RSS was 40 MiB
  at about 6 Gbps on loopback, against the 150 MiB target at 1 Gbps. Idle RSS has no meaning until
  the Phase 2 daemon exists.
  The first benchmark run found small transfers slower than `scp` (a tiny file took 0.23 s against
  0.12 s; 200 files took 0.35 s against 0.18 s), so two fixes went in first:
  - The CLI's "is the destination a directory" lookup now opens the connection that becomes
    lane 0 (`Remote::kind`, `with_root`). Test: one login for a lookup plus a one-chunk push.
  - Workers connect their lane before taking work, and handshakes still running when all work is
    done are dropped. Test: through a relay that delays later handshakes by 3 s, a 20-file tree
    finishes on lane 0 in under 2 s (6.5 s before).

  After them, a tiny file took 0.12 s and 200 files took 0.145 s.
  Security review: `docs/phase1/security-review.md`, six findings, each fixed with a test that
  failed first:
  1. A claimed exabyte size exhausted memory.
  2. Server text reached the terminal unescaped.
  3. Directory listings were unbounded.
  4. A leftover part file in a tree could corrupt its sibling.
  5. A fatal error with a full channel hung (a step 14 regression).
  6. The russh-sftp request race.
