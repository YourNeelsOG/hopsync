# Phase 1 security review (2026-11-04)

Scope: everything Phase 1 added (`crates/hopsync`: hostkeys, ssh, sftp, paths, localio, plan,
pool, engine, policy, journal, tune, cli and the `hopsync` binary), checked against CLAUDE.md §3
and the threat model "Hopsync holds SSH credentials for many servers; any remote may be hostile".
Reviewer: the implementing agent, so this review is not independent. A second reviewer should check
it before a public release.

## Method

1. Walk the CLAUDE.md §3 rules and check each against the code, citing the test that enforces it.
2. Treat every value a server sends as hostile: sizes, mtimes, names, listings, limits, status
   messages, host keys and timing. For each one, ask what it can make the client do.
3. Every finding gets a failing test before the fix. Where the unfixed code could harm the
   machine running the test (memory), the test caps its own address space first.

## CLAUDE.md §3 checklist

| Rule | Status | Where it is enforced |
|---|---|---|
| No plaintext secrets stored | Holds | Hopsync stores no secrets: private keys stay in the user's key files. Passphrase-protected keys are refused, not prompted for (Decision log). |
| Never log secrets | Holds | Errors name key file paths and parse reasons, never key material; no request or auth data is printed. |
| Host keys verified and pinned; a change is a hard failure | Holds | `hostkeys` and `ssh::connect` (step 4 tests). New this phase: a lane retired for a refusal never covers a host key error. `a_changed_host_key_on_a_later_lane_is_fatal_not_retired` sends every lane after the first to an impostor sshd. |
| Paths confined to roots; no path reaches a shell | Holds | Local: `openat2` with `RESOLVE_BENEATH`, `NO_SYMLINKS` and `NO_MAGICLINKS`, with `O_NOFOLLOW` on final components (paths tests). Remote: every name is a validated `Name` (no `/`, `.`, `..`, NUL or control characters). No `std::process::Command` anywhere in the crate. |
| No endpoint, option or plugin executes commands | Holds | Only SFTP requests are sent. The free-space check uses `statvfs@openssh.com`, never `df` (Decision log). |
| Web UI auth from first start | Not applicable | There is no web UI in Phase 1. |
| Supply chain | Holds | `Cargo.lock` is pinned; `cargo deny check` and `cargo audit` are clean. One crate added this phase: `twox-hash` 2.1.5 with default features off. `rsa` stays off (RUSTSEC-2023-0071). |
| `unsafe` only with a SAFETY note | Holds | One block: `localio::sync_range` (`sync_file_range`, which no safe wrapper covers). Workspace lint `unsafe_code = "deny"`. |

## Findings, all fixed

### F1. A claimed file size could exhaust client memory (high)

`Shared::enqueue` collected every chunk range of a file into a `Vec` before the free-space
check. A remote reporting a 4 EiB file (a sparse file does this with no disk space) made a pull
plan 2^36 ranges and try to allocate 1.6 TB. A local sparse file pushed to a server without
`statvfs@` had the same effect. `part_holds` also walked every chunk.

Fix: chunks are counted with `plan::count_with` and produced one at a time with
`plan::chunk_with`; resumed bytes and the part-coverage check use only the journaled indices. The
space check now refuses the file before any chunk is produced.

Test: `engine_hostile::a_claimed_size_of_exabytes_costs_no_memory` caps the address space at 8 GiB
and pulls a 2^62-byte sparse file. Before the fix the process aborted on "memory allocation of
1649267441664 bytes failed"; it now fails cleanly with `NoSpace` and leaves no part file.

### F2. Server-chosen text reached the terminal unescaped (medium)

The top-level error line and the per-entry problem lines printed `TransferError` text raw. That
text includes SFTP status messages, which the server chooses freely. Escape sequences in them could
clear the screen, rewrite earlier lines, or fake a success message. The host-trust prompt printed
the offered algorithm name raw as well, on the very line the user reads before pinning a key.

Fix: `cli::printable` escapes control characters, and every CLI line that carries server-chosen or
path text goes through it: errors, problems, destinations and the trust prompt.

Test: `cli::tests::printable_escapes_control_characters_only`.

### F3. A directory listing could grow without bound (medium)

`Sftp::read_dir` appended names until the server said EOF, so a hostile or broken server could
stream names forever.

Fix: `SftpOptions::max_dir_entries` (default 10 million) ends the listing with
`ListingTooLong`. That directory fails and is reported; the rest of the tree goes on.

Test: `engine_hostile::an_endless_listing_is_cut_off`.

### F4. A leftover part file in a tree could silently corrupt its sibling (medium, data integrity)

A source directory holding both `a` and `a.hopsync-part` (left by an interrupted earlier run into
that directory) sent both. `a` was written through the destination `a.hopsync-part` while the stale
part was being renamed onto that same name. With `--overwrite` this replaced the in-progress part,
so `a` arrived with the stale content and no error.

Fix: tree walks skip files ending in `.hopsync-part` and report them (`Problem::PartFile`). An
explicitly named single file is still sent, under its own distinct part name.

Test: `engine_hostile::leftover_part_files_in_a_tree_are_not_sent`.

### F5. An aborted transfer could hang forever (medium, availability)

Found by the F-series tests rather than by reading the code. Since step 14 kept a receiver handle
for auto-tune, a fatal error with a full work channel left the producer blocked on `send` forever:
the workers had exited, but the channel never closed.

Fix: the producer's send also wakes on the transfer's `finished` signal, which an abort sets.

Test: `a_changed_host_key_on_a_later_lane_is_fatal_not_retired`, which hung for 60 s before the fix
and now fails correctly in 0.5 s.

### F6. A request could hang for the full timeout after a connection loss (low, availability)

A russh-sftp 3.0.1 race left a request unanswered until its 60 s timeout. The request had to be
sent between the read loop ending and the write loop noticing.

Fix: the channel stream is wrapped to flag its end, and every request is raced against that flag
(step 14, Decision log).

Test: `a_blip_that_drops_every_lane_keeps_them_all` hung in 2 of 12 runs before the fix, and in 0 of
24 after it.

## Accepted risks (documented, not fixed)

- **Remote-side symlinks.** Hopsync never follows a symlink it finds in a listing. A symlink inside
  the remote root is still resolved by the SFTP server when a path through it is opened, and the
  client cannot prevent that. Since the remote account belongs to the user, this only lets the user
  redirect their own data.
- **Name collision with the part suffix.** A destination file that already has the exact name
  `<name>.hopsync-part` is treated as Hopsync's part file and overwritten when `<name>` is
  transferred. The suffix is reserved; trees no longer send such files (F4).
- **Verify trusts what the destination returns.** The read-back may be served from the destination's
  page cache, and a hostile server can return whatever bytes it likes. Verify detects accidental
  damage only. The xxh3 choice was made with this in mind (Decision log).
- **A warm session goes stale.** The session that `Remote::kind` keeps open is meant to be used
  right away. If the server drops it in between, the transfer's first request fails with a
  connection error instead of reconnecting.
- **Resource use is bounded by configuration, not by the server.** Buffers: `max_lanes ×
  inflight × 256 KiB` (64 MiB at the CLI defaults; the CLI caps lanes at 64). Connections: at most
  `max_lanes`, with at most 2 handshakes at a time.
- **No independent review yet.** See above.
