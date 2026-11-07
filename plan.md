# Hopsync: Design & Plan

**Status:** approved by owner 2026-11-03; Phases 0 and 1 done, Phase 2 done (2026-11-04), Phase 3 done (2026-11-04), web UI moved to React (2026-11-04, `docs/plans/2026-11-04-react-ui-plan.md`); Phase 4 started (2026-11-06: CI and release binaries done; 2026-11-07: one-line install and `hopsync setup`)
**Language:** Rust
**License:** MIT OR Apache-2.0
**Rules:** every contributor and agent follows `CLAUDE.md` in this folder.
**Prior art research:** `../filezilla-rclone-lsyncd-rsync-research-paper.md` (§7.4 and Appendix A)

---

## 1. Intent

Moving big media (season packs, 10–100+ GB folders) between seedboxes and servers today means choosing
between tools that are easy but slow (FileZilla, scp: one stream, no headless mode) or fast-ish but
unfriendly and risky to expose (rclone RC/Web GUI, lftp scripts, rsync with no UI).

Hopsync is **"qbittorrent-nox for SSH transfers"**: a background service you install on any Linux box,
open in a browser, add other machines by SSH, and push or pull files and folders at close to line speed
(1 / 2.5 / 10 Gbps), while using very little memory, page cache, and disk I/O.

Every install is equal (peer-style, like qBittorrent clients). The other end only needs SSH/SFTP:
like FileZilla, it does not need Hopsync installed.

### Success looks like

- A 50 GB season pack moves seedbox → seedbox over a 1 Gbps path at ≥ 90 % of line rate, where single-stream
  `scp`/SFTP manages a fraction of that. Published, reproducible benchmarks vs `scp`, `rsync`, `rclone`,
  `lftp` in the README.
- Install-to-first-transfer in under 5 minutes on a shared seedbox without root.
- Hopsync itself never becomes the reason a box is compromised.

## 2. Goals, non-goals, anti-goals

### Goals (v1)

1. Fast transfer of files and folders between the local box and any SSH box, push and pull.
2. Web UI: manage servers, browse both sides, start/pause/resume/cancel jobs, live progress.
3. CLI: same binary; drives the running service (`hopsync push|pull|ls|jobs|...`).
4. Runs as a systemd service (system or `--user`), survives restarts, resumes jobs.
5. Strict resource budget (§6): low RAM, low page-cache footprint, no extra disk I/O.
6. Security by default (§7).

### Non-goals (v1)

- Continuous folder mirroring / watch mode / two-way sync (planned later; the "sync" in the name).
- Cloud backends (S3, Drive, …). rclone already covers these.
- FTP/FTPS. SSH/SFTP only.
- Windows/macOS as the *host* (they work fine as *remote* SSH targets).
- Multi-user accounts and permissions (single admin in v1; designed so it can be added).

### Anti-goals (would be a failure even if it "works")

- A web UI that is shell-equivalent or reachable without auth (rclone R1/R8 lesson).
- Silently accepting SSH host keys (rclone R7 lesson).
- Saving passwords merely obfuscated (FileZilla F1 lesson).
- Memory or page cache that grows with file size, or a tool that slows the seedbox's torrent client.
- Writing temp copies or re-reading data "just in case".

## 3. Constraints

- **Shared seedboxes:** often no root, no system systemd, restricted ports, ARM or x86_64. Must run as a
  normal user, from a single binary that needs only glibc 2.17 (Decision log 2026-11-06), with
  `systemctl --user` or a plain `hopsync serve`.
- **Remote side is plain OpenSSH (or Dropbear/Windows OpenSSH).** We control nothing there except what
  SFTP and its extensions allow.
- **SSH server limits:** OpenSSH defaults `MaxSessions 10` and `MaxStartups 10:30:100`; providers may run
  fail2ban. Lane count must respect these and back off.
- **NAT:** the machine running Hopsync always initiates. A home PC behind NAT can push/pull to seedboxes;
  seedboxes cannot reach into it unless the user sets up port forwarding/Tailscale.

## 4. Decided (by the owner)

- Name: **Hopsync**. Language: **Rust**. Open source.
- v1 focus: **fast seedbox-to-seedbox transfer with a web UI**.
- Peer-style: any install can push to / pull from any SSH box; remote needs only SSH.
- Runs as a background service under systemd, like qbittorrent-nox; CLI and web UI.
- Workload: mostly big files (season packs), links 1 Gbps now, 2.5/10 Gbps soon; home PCs are also peers.
- Resource efficiency is a hard requirement: low memory, low page cache, no extra disk I/O.
- Security is a top concern; research before choosing libraries; think through edge cases; never drop
  existing good behavior (see `CLAUDE.md`).

## 5. Architecture

```
          Browser (web UI)            Terminal (hopsync CLI)
                 \                         /
                  \     HTTPS + SSE       /   (local Unix socket for CLI)
                   v                     v
        +------------------------------------------------+
        |                hopsync serve                   |
        |  API layer (axum)  - auth, CSRF, rate limiting |
        |  Job scheduler     - queue, priorities, retry  |
        |  Transfer engine   - lanes, chunks, resume     |
        |  SSH pool          - sessions per remote       |
        |  Local I/O layer   - fadvise, O_DIRECT, openat2|
        |  Store (SQLite)    - servers, keys, jobs, log  |
        |  Secret vault      - encrypted credentials     |
        +------------------------------------------------+
                         | N SSH connections
                         v
                remote box (sshd + sftp-server)
```

- **One binary**: `hopsync serve` (daemon), everything else is a CLI client of the daemon over a
  local Unix socket (0600, owner-only); no network auth needed for the local CLI.
- **Web UI** is a static SPA compiled into the binary at build time (never fetched at runtime).
  Live progress over Server-Sent Events (Decision log 2026-11-04); API is JSON.
- **Store:** SQLite in WAL mode with a small journal of completed chunks per job (§6.3). Low write rate:
  checkpoints are batched, not per-packet.

### Candidate libraries (MUST be verified in Phase 0, not decided)

| Need | Candidates to evaluate | What to measure / check |
|---|---|---|
| Async runtime | `tokio` | baseline |
| SSH transport | `russh`; `openssh` + `openssh-sftp-client` (uses system `ssh` with multiplexing); `ssh2` (libssh2) | throughput per connection, cipher support (AES-GCM, ChaCha20), CPU per Gbps, host-key API, maintenance, RustSec history |
| SFTP client | `russh-sftp`; `openssh-sftp-client` | request pipelining depth, max request size, extension support (`limits@`, `fsync@`, `statvfs@`, `posix-rename@`) |
| Web server | `axum` + `tower` | n/a |
| Frontend | Decided: React, TypeScript, Vite and Tailwind, built to static files and embedded by `build.rs` with `include_bytes!` (Decision log 2026-11-04) | bundle size, dependency count |
| DB | `rusqlite` vs `sqlx` (SQLite) | footprint, sync vs async fit |
| Password hashing | `argon2` | n/a |
| Secret encryption | `chacha20poly1305` / `aes-gcm` (RustCrypto) or `age` | audited, maintained |
| Hashing | `blake3`, `xxhash-rust` (xxh3) | CPU per GB |

## 6. Transfer engine & resource budget

### 6.1 Speed

- **Lanes:** a job opens N SSH connections (default auto, typically 4–8, capped by the remote's
  `MaxSessions`/`MaxStartups` and backing off on refusal). Multiple connections beat the single-core
  cipher limit and the single-TCP-window limit on long paths.
- **Segmented files:** big files are split into chunks (e.g. 64–256 MiB ranges) distributed across lanes;
  each lane writes its range directly at the right offset on the destination; no reassembly step.
- **Pipelining:** each lane keeps many SFTP read/write requests in flight (like `sftp -R`), using the
  server's advertised max request size (`limits@openssh.com`) when available.
- **Small files:** many files in parallel across lanes instead of segmenting.
- **Auto-tune:** start conservative, increase lanes/in-flight requests while throughput rises, back off
  on errors or CPU saturation. Manual overrides per job and per server.
- **Ciphers:** prefer `aes128-gcm`/`aes256-gcm` on CPUs with AES-NI/ARMv8 crypto, `chacha20-poly1305`
  otherwise. Never enable legacy ciphers.

### 6.2 Memory

- Fixed buffer pool: `lanes × in-flight requests × request size` (e.g. 8 × 64 × 256 KiB = 128 MiB worst
  case; tune defaults so typical use is far lower). Backpressure, never unbounded queues.
- Directory listings streamed and paginated, not loaded whole for huge trees.
- Targets (to confirm by benchmark): idle RSS < 30 MiB; under a 1 Gbps transfer < 150 MiB; configurable cap.

### 6.3 Page cache and disk I/O (local side)

- **Reads (push):** read sequential ranges, then `posix_fadvise(POSIX_FADV_DONTNEED)` on ranges already sent.
- **Writes (pull):** after each range, `sync_file_range(WRITE)` to start write-back, then
  `sync_file_range(WAIT)` + `POSIX_FADV_DONTNEED` a window behind, so dirty pages never pile up.
- **Optional `O_DIRECT`** mode for users who want zero cache use (alignment-aware; falls back if the
  filesystem refuses).
- **Setting `cache = drop | keep`**: `drop` is the default; `keep` exists because dropping clean pages of a
  file a torrent client is actively seeding can cost that client a re-read (edge case, documented).
- **No temp copies:** write to `<name>.hopsync-part` in the destination folder, then rename (metadata-only).
- **No read-back by default:** hashes are computed inline on the reading side while streaming.
  End-to-end verify (destination re-read) is optional because it doubles destination read I/O
  (see Open questions).
- **Durability without per-packet fsync:** chunks are marked complete in the journal only after the
  data is durable. Local: after `sync_file_range(WAIT)`; remote: after `fsync@openssh.com` at
  checkpoints (every few hundred MiB per file). A crash or remote reboot never leaves a "complete"
  chunk that was only in cache.

### 6.4 Remote side limits

Over plain SFTP we cannot control the remote's page cache or I/O pattern (sftp-server does normal
buffered I/O). **Later phase:** when Hopsync is installed on both ends, a peer mode can run the remote
side with the same cache/I/O discipline and add delta-resume for partially present files.

### 6.5 Resume and correctness

- Per-job journal of completed chunk ranges; restart resumes only missing ranges.
- Before resuming, re-stat the source: if size or mtime changed, the file restarts (never stitch two
  versions together).
- Existing destination file policy per job: skip / overwrite / resume / rename (FileZilla-style).
- Preserve mtime (and permissions when allowed); handle zero-byte files, empty dirs, sparse files
  (copied as regular data in v1, documented).
- Check free space (`statvfs@openssh.com` / local `statvfs`) before starting; fail early.

### 6.6 Topology

- v1: Hopsync runs on **one of the two ends** (e.g. installed on seedbox A, pushes to seedbox B over SSH).
- Hopsync on machine C moving A → B: either relay through C (slow over home links, shown clearly in
  the UI) or, later, ask A's Hopsync to do it (peer mode). Decision pending (see Open questions).

## 7. Security design

| Threat (paper ref.) | Control |
|---|---|
| Stolen DB / backup leaks credentials (F1, R2) | Secrets encrypted with AEAD; key in a separate 0600 key file, or optional **master passphrase** mode (key never on disk; jobs wait for unlock after restart). Prefer SSH keys: Hopsync can generate a per-remote ed25519 key and install it (ssh-copy-id style) so passwords need not be stored at all. |
| Exposed admin UI (R1, R8) | Auth required from first start; first-run setup token printed to the log/terminal (no default password); argon2id password hashes; HttpOnly + Secure + SameSite=Strict session cookies; CSRF tokens; login rate limiting and lockout; strict CSP; no arbitrary-command or "inline backend" endpoints; UI assets embedded, never fetched. |
| Plain-HTTP UI on a public seedbox | Built-in TLS (rustls) with user cert or auto-generated self-signed cert; loud warning when serving plain HTTP on a non-loopback address; reverse-proxy guide (Caddy/nginx). |
| SSH MITM (F2, R7, L3) | TOFU with explicit fingerprint confirmation in the UI/CLI, pinned per server; key change = hard failure until the user re-confirms; legacy KEX/ciphers/MACs never enabled. |
| Malicious remote file names (F4, S1, S2) | Reject `..`, absolute names, NUL/control chars from listings; local writes via `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` inside the chosen root; symlinks not followed by default; configurable allowed roots for both sides. |
| Command injection (L1, L4, R4) | Nothing is ever passed to a shell; no user-defined hooks in v1; config is declarative (TOML). |
| Supply chain | `Cargo.lock` committed; `cargo audit` + `cargo deny` (advisories, licenses, duplicate/banned crates) in CI; minimal dependencies; reproducible release builds; signed artifacts (minisign or Sigstore) plus checksums; no self-update that downloads code. |
| Local privilege | Runs as an unprivileged user; systemd unit hardening (`NoNewPrivileges`, `ProtectSystem=strict`, `ReadWritePaths=` the data roots, `PrivateTmp`, …); never needs root. |
| Secrets in logs | Redaction layer for all logs and error messages; tests assert secrets never appear in logs. |
| Audit | Append-only audit log: logins, server added/removed, host-key decisions, every transfer (who/what/where/when). |

Every phase ends with a security review of the code it added.

## 8. Edge cases checklist (must be handled or explicitly documented)

- Network drop mid-chunk; remote reboot; local reboot/power loss; service restart mid-job.
- Disk full / quota exceeded on either side mid-transfer.
- Source file modified, truncated, renamed, or deleted during transfer.
- Destination exists (same, older, newer, partial `.hopsync-part`).
- Remote `MaxSessions`/`MaxStartups` reached; fail2ban after too many connections; auth rate limits.
- Host key changed; server reinstalled; DNS points to a different box.
- Dropbear without sftp-server; SFTP-only chroot accounts; Windows OpenSSH paths and line endings.
- Filenames: unicode, spaces, newlines, very long names/paths, case-insensitive remote filesystems.
- Symlinks (inside and escaping the root), hardlinks, sparse files, zero-byte files, empty directories,
  100k+ file trees.
- Permission denied mid-tree; partial folder success reporting.
- Same file queued twice; two jobs writing the same destination.
- Clock skew between hosts (mtime comparisons).
- Very slow or stalled remote (timeouts, keepalives); hostile remote sending huge listings or slowloris replies.
- 32-bit ARM / low-RAM boxes (512 MiB).
- Torrent client seeding the same file while Hopsync reads it (cache policy).

## 9. Roadmap

Each phase gets its own detailed implementation plan (written when the phase starts) and ends with
tests passing, a benchmark run, and a security review.

### Phase 0: Research & spikes (no product code)
Notes and results: `docs/phase0/`.
- Benchmark SSH/SFTP crate candidates (§5) on real 1 Gbps (and if possible 10 Gbps) links: throughput per
  connection, CPU per Gbps, max in-flight requests, extension support.
- Measure the baseline: `scp`, OpenSSH `sftp`, `rsync -e ssh`, `rclone` (`--multi-thread-streams`),
  `lftp pget -n` on the same pair of boxes.
- Validate the page-cache strategy (measure `Cached`/`Dirty` in `/proc/meminfo` during transfer).
- Survey prior art in depth: SeedSync, lftp mirror, rclone, Resilio Sync, `jsync`, `hpsync`.
- Output: Decision log entries for every library choice.

### Phase 1: Transfer engine (CLI only)
- SSH pool, lanes, segmented chunks, pipelining, resume journal, cache/I/O discipline, verification.
- `hopsync push|pull` working standalone; benchmarks meet targets.

### Phase 2: Service, store, API
- `hopsync serve`, SQLite store, secret vault, host-key pinning, job scheduler, JSON API + SSE,
  Unix-socket CLI, systemd units (system and user).

### Phase 3: Web UI
- Login/setup, servers (add, fingerprint confirm, key install), two-pane browser, queue, live progress,
  history, settings.

### Phase 4: Hardening & packaging
- Show a percentage and time left for downloads and folders. Today only a single-file upload has a
  total, because the daemon learns other totals only by asking the remote or walking the tree.
- Done 2026-11-06: CI that runs the audits, tests and browser run, and release builds with
  `HOPSYNC_REQUIRE_UI=1`; binaries for x86_64, aarch64 and armv7; signed build records.
- TLS, rate limiting, audit log, fuzzing of path/listing parsing, systemd sandboxing.
- Done 2026-11-07: one-line install and `hopsync setup` for every kind of box (`docs/phase4/install-security-review.md`).
- AUR, `.deb`, Docker image, docs, benchmark page.
- Passphrase-protected keys or `ssh-agent`, and RSA if users ask for it after the advisory below.

### Phase 5: v1.0 release

### Later
- Peer mode (Hopsync on both ends): remote-side cache control, server-to-server orchestration, delta resume.
- Watch/mirror sync and scheduled jobs (the "sync" in Hopsync).
- Multi-user accounts, API tokens, bandwidth schedules, notifications (Discord/Telegram), *arr integrations.

## 10. Decision log

| Date | Decision | Reason |
|---|---|---|
| 2026-11-03 | Name: Hopsync | Owner choice; no file-transfer project with that name on GitHub/crates/PyPI/npm/AUR; hopsync.io and hopsync.dev were unregistered at check time. |
| 2026-11-03 | Language: Rust | Owner choice; speed, memory safety, single static binary. |
| 2026-11-03 | SSH/SFTP only in v1 | Matches the seedbox use case; smaller attack surface. |
| 2026-11-03 | Plan approved; start with Phase 0 | Owner choice. Library choices get measured before product code. |
| 2026-11-03 | License: MIT OR Apache-2.0 | Owner choice; the usual Rust dual license, permissive, widest adoption. |
| 2026-11-03 | Toolchain via rustup, pinned in `rust-toolchain.toml` | Same compiler for every contributor and CI; needed for reproducible release builds. Distro Rust packages move with system upgrades. |
| 2026-11-03 | Phase 0 benchmarks run on owner's real seedboxes, plus local loopback | Owner has boxes ready. Loopback isolates CPU cost per Gbps; real boxes give line-rate and RTT effects. |
| 2026-11-03 | SSH/SFTP: `russh` + `russh-sftp` (aws-lc-rs backend; `des`, `dsa` off) | Speed even with openssh-sftp-client: loopback 2.1 to 2.6 GB/s at 4 lanes with 2 to 4 workers; the seedbox (203 ms RTT, 30 MiB/s cap) both fill the cap at 8 lanes. russh uses less client CPU (0.5 to 0.8 CPU-s/GB at 1 to 2 workers) and keeps host-key pinning, auth and ciphers in-process: no `ssh` subprocess, no argument injection surface, no accept-new default. Costs: larger dependency tree (126 + 44 crates); we must cap request sizes to `limits@openssh.com` ourselves. Evidence: `docs/phase0/`. |
| 2026-11-03 | tokio runtime with an explicit small worker count (default 2, configurable) | The default one-worker-per-CPU (16 here) cost 2 to 3× the CPU and up to 40 % throughput at 4 lanes. Fewer threads also leave cores to the seedbox's torrent client. |
| 2026-11-03 | Local I/O: `rustix` (`fadvise`, `openat2`) plus `libc::sync_file_range` in one documented `unsafe` wrapper | rustix 1.1.6 and nix 0.31.3 do not wrap `sync_file_range`. rustix is already in the tree through tokio. |
| 2026-11-03 | CLI parsing: `clap` 4 (derive) | De facto standard (275 M downloads in 90 days), MIT OR Apache-2.0, active. Subcommands, help and error text for free; `lexopt`/`argh` would mean hand-written help and subcommand handling. |
| 2026-11-03 | Resume journal and later the store: SQLite via `rusqlite` with `bundled` | One embedded DB for Phase 1 journal and Phase 2 store, no rework. Synchronous API run on a blocking thread fits a single low-rate writer better than `sqlx` (async, heavier, MSRV 1.94). `bundled` keeps the single static binary. Advisories (2020 to 2022) are all patched. |
| 2026-11-03 | Inline hash: xxh3-128 via `twox-hash` 2 | Measured 0.017 CPU-s/GB vs 0.18 for `blake3` (single thread, this machine). The hash detects accidental corruption only; a hostile remote can lie about anything it stores, so a cryptographic hash buys nothing in v1. `xxhash-rust` is BSL-1.0, not on the license allow list. Revisit with `blake3` if a signed manifest is ever needed. |
| 2026-11-03 | No RSA in v1 (user keys and host keys); russh `rsa` feature off | Enabling it pulls in the `rsa` crate, which has an unfixed timing side-channel advisory (RUSTSEC-2023-0071, "Marvin"); `cargo deny` would have to ignore it. ed25519 and ECDSA cover OpenSSH since 2014, and Hopsync will generate ed25519 keys per remote. A host with only RSA host keys gets a clear "unsupported host key type" error. Revisit if users need it. |
| 2026-11-03 | Ciphers: AES-GCM first with AES instructions, else ChaCha20-Poly1305 first; AES-CTR + ETM MACs last | Phase 0: AES-GCM about 2x ChaCha20 on AES-NI. russh's default list lacks `aes128-gcm`, the fastest. CTR only as a fallback for old servers; no CBC, 3DES, SHA-1 MACs or SHA-1 KEX (already excluded by russh). |
| 2026-11-03 | Host-key negotiation limited to the algorithms pinned for the host | Same as OpenSSH: a known host offering an unpinned key type is a mismatch, never a new trust prompt. Unknown hosts are refused before any connection. |
| 2026-11-03 | Default SSH channel window 8 MiB (configurable) | 2 MiB caps one lane near 10 MB/s at 200 ms RTT; 16 MiB nearly doubled it on the seedbox, but 8 lanes x 16 MiB would break the memory budget (plan.md §6.2). Auto-tune (Phase 1 step 14) can raise it. |
| 2026-11-03 | Passphrase-protected private keys not supported in Phase 1 | No interactive prompt exists before the CLI, and the daemon (Phase 2) must not hold passphrases in memory longer than needed. Clear error today; ssh-agent support to decide in Phase 2. |
| 2026-11-03 | Overwrite needs `posix-rename@openssh.com`; no-overwrite uses plain SFTP rename / `renameat2(RENAME_NOREPLACE)` | Replacing a file must be atomic, so a server without posix-rename gets a clear "unsupported" error instead of a remove-then-rename window. Plain v3 rename fails on an existing target (OpenSSH: link + unlink), which is exactly no-overwrite. |
| 2026-11-03 | A file that changed (size or mtime) during transfer fails; the part file is kept | Stitching old and new bytes would produce a file that never existed. Restart or resume decides later (step 11). |
| 2026-11-03 | clap 4.6.7 with default features off (std, derive, help, usage, error-context) | D4; drops color and suggestions dependencies. |
| 2026-11-03 | Per-file fsync only for files of 1 MiB and more; a pull ends with one `syncfs` | An fsync per small file (and per directory) made a 10k-file tree pay 10k+ disk flushes. Big files are flushed before their rename; small files and renames are covered by one syncfs. A crash before it can leave a short small file under its final name, which the size+mtime comparison of step 12 re-transfers. |
| 2026-11-03 | Trees skip symlinks and special files, and refuse names that are not UTF-8 or contain control characters; all are listed in the report | Following symlinks would leave the configured roots (CLAUDE.md §3); carrying names with newlines or escapes into logs and other tools is a hazard. Nothing is dropped silently. |
| 2026-11-03 | One file failing does not stop a tree; SSH-level failures do | A permission error deep in a tree should not cost the other 9,999 files, but a rejected key or changed host key affects every file, so continuing would only repeat the error. |
| 2026-11-03 | Resume journal records a chunk only after the destination made it durable; only multi-chunk files are journaled | The journal may lag the data, never lead it, so a crash costs redone chunks, never holes. Single-chunk files are cheaper to redo than to journal (one row plus one fsync each). |
| 2026-11-03 | Resume requires the same source size and mtime and a part file covering the recorded chunks | Otherwise the file restarts: stitching two versions of a source, or trusting a part someone truncated or deleted, would produce a file that never existed. |
| 2026-11-03 | rusqlite 0.41.1 (`bundled`), WAL with `synchronous=NORMAL` | D5. NORMAL may lose the last journal commits on power loss but never corrupts the database, and a lost record only redoes a chunk. Bundled SQLite avoids depending on the system library version. |
| 2026-11-04 | Destination policies: skip (default), update, overwrite, rename; no "resume" policy | Skip is the only default that can never destroy data. Update keeps a destination newer than the source because it may hold edits. Resume is the journal's job (step 11) for every policy, and update completes a short copy made by another tool. Times compare in whole seconds: SFTP v3 carries no more. |
| 2026-11-04 | Free-space check up front with a `--min-free` reserve | A seedbox that fills up breaks its torrent client, so the reserve protects the box, not only the transfer. Checking before a file's first byte means a refused file leaves nothing behind. |
| 2026-11-04 | No space check when the server lacks `statvfs@openssh.com` | SFTP v3 has no portable free-space request, and running `df` would be a remote command (forbidden by CLAUDE.md §3). The transfer then fails on the server's own ENOSPC like any write error. |
| 2026-11-04 | Verify per chunk, right after the chunk is written, and only with `--verify` | A final whole-file re-read runs on one lane and is slow at high RTT; per chunk it is spread over all lanes. Hashing is skipped when verify is off, because without a read-back the hash proves nothing that SSH's MAC does not. Off by default (§11 question 1, proposed default) until the owner decides. |
| 2026-11-04 | A verify mismatch fails the file and drops its journal rows | A chunk that read back wrong may sit next to other damage; trusting none of the part is the only safe resume. The part file is kept, and the next run rewrites every chunk of it. |
| 2026-11-04 | At most 2 SSH handshakes in flight per transfer | sshd drops unauthenticated connections past `MaxStartups` (default 10:30:100, lower on some shared boxes); pacing handshakes costs about one connect time per lane at start, while data already flows on lane 0. |
| 2026-11-04 | A lane refused before it ever connected is retired, not retried; a lane that was up and drops is retried | A refusal at start is a server limit (MaxStartups, a firewall connection cap, a per-user limit), and retrying it burns the shared budget until the whole transfer fails. A drop after working is usually a blip, and retiring there would shrink the transfer for good. Host key errors are never retired: a second connection seeing a different key is the attack pinning stops. |
| 2026-11-04 | Auto-tune adds lanes while each raises the plateau rate by 10 %; default 4 to 16 lanes on the CLI, fixed in the library unless `max_lanes` is set | Measured on the seedbox: single-sample decisions mistook TCP ramp-up for gains. Starting at the old default of 4 means tuning can only add lanes that measurably help; a capped box stops early instead of getting 16 connections. |
| 2026-11-04 | Every SFTP request is raced against end of the channel stream | russh-sftp 3.0.1 leaves a request sent in a short window after the transport dies unanswered until its timeout (60 s). Fixing it in our wrapper needs no fork; worth reporting upstream. |
| 2026-11-04 | Tree walks skip `*.hopsync-part` files and report them | They are unfinished transfers, never user data, and sending one collides with the part file of its complete sibling (security review F4). |
| 2026-11-04 | Remote listings are capped (`max_dir_entries`, default 10 million) | A hostile or broken server could otherwise stream names until the client runs out of memory (security review F3). Ten million covers any real seedbox directory. |
| 2026-11-04 | A lookup before a transfer reuses its connection; lanes connect before taking work | Each SSH handshake costs about 0.1 s on loopback and about 1 s at 200 ms RTT. Small transfers were slower than `scp` until a transfer stopped paying for handshakes it did not need. |
| 2026-11-04 | UX requirements for Phases 2 and 3 come from `docs/research/ux-study.md` (U1 to U21) | Owner asked to study rclone, rsync, lsyncd, bisync and FileZilla before the web UI. Key outcomes: no modal prompts mid-job, durable queue history, dry run shares the planner, progress as snapshots. |
| 2026-11-04 | HTTP server: `axum` 0.8.9 with default features off (`http1`, `json`, `query`, `tokio`); no HTTP/2 | Compared with `actix-web` 4.18: actix runs its own per-thread arbiters beside tokio, which fights the small fixed worker count (Decision 2026-11-03), and has 12x fewer downloads (13 M vs 151 M in 90 days). axum is tokio, hyper and tower, already the stack under russh's runtime. All RustSec advisories for hyper, axum-core and tower-http are patched in current versions. HTTP/2 brings `h2`, with four DoS advisories since 2023, and a single-admin UI does not need it. |
| 2026-11-04 | Live progress over Server-Sent Events, not WebSocket (changes §5) | Progress flows one way. SSE is plain HTTP: the session cookie and same-origin rules apply, so there is no cross-site WebSocket hijacking to defend against. Browsers reconnect it by themselves, and it needs no `tungstenite` (one DoS advisory, 2023). Commands stay on the JSON API. |
| 2026-11-04 | Each job publishes its latest progress snapshot through a `tokio::sync::watch` | Latest value wins: a slow browser skips snapshots instead of queueing them, so memory stays bounded however many clients watch (UX study §3). |
| 2026-11-04 | Admin password hash: `argon2` 0.6.0, argon2id, m = 19 MiB, t = 2, p = 1 (OWASP minimum) | Compared with PBKDF2 from `aws-lc-rs` (no new crate, but even 600k HMAC-SHA256 rounds are cheap on GPUs) and `scrypt`. argon2id is the OWASP first choice; RustCrypto crate, MIT OR Apache-2.0, no advisories. 19 MiB for one login at a time fits a 512 MiB box. |
| 2026-11-04 | Secret vault: `aws-lc-rs` AEAD (ChaCha20-Poly1305), key in a separate 0600 `vault.key` file | `aws-lc-rs` is already linked through russh, so the vault adds no crate. Compared with the RustCrypto `chacha20poly1305` crate (same primitive, new dependencies) and `age` (a file format with many dependencies, built for files, not database rows). Random 96-bit nonces; the associated data binds each ciphertext to its table, row id and purpose, so a sealed value cannot be moved to another row. A stolen database without the key file reveals no secret. The master-passphrase mode of §7 comes later. |
| 2026-11-04 | Sessions and CSRF are hand-written, not `tower-sessions` | One admin and one SQLite store need about 100 lines; tower-sessions brings generic store traits and more crates. Token: 256 random bits, only its SHA-256 stored. Cookie: HttpOnly, SameSite=Strict, Secure and the `__Host-` prefix under TLS. Every state-changing request must have an `Origin` matching the host (or `Sec-Fetch-Site: same-origin`) and a JSON body (OWASP CSRF guidance for SameSite plus origin checks). |
| 2026-11-04 | Local CLI talks to the daemon over the same router on a Unix socket, checked by `SO_PEERCRED` | The socket lives in a 0700 directory with mode 0600, and connections from another uid are refused, so the local CLI needs no password (plan.md §5). One router means the CLI and the UI cannot drift apart. |
| 2026-11-04 | Store schema migrations hand-written on `PRAGMA user_version` | A dozen lines instead of `refinery` or `rusqlite_migration`. Each migration runs in one transaction, and a database newer than the binary is refused, never downgraded. |
| 2026-11-04 | Daemon settings (bind address, data directory) come from flags and the environment; transfer defaults live in the store | The UI edits transfer defaults (policy, verify, lanes, min free). Changing where the daemon listens from the UI could lock the user out or expose it, so it stays outside the API. |
| 2026-11-04 | The resume journal stays in its own file (`journal.sqlite`) beside `hopsync.db` | It writes one row and one sync per durable chunk; sharing a file with the store would make API requests wait on that write rate. Two small files cost nothing. |
| 2026-11-04 | One daemon per data directory, enforced by `flock` on `hopsync.lock` before the store opens | A second daemon put the first one's running jobs back in the queue, which could run them twice (Phase 2 security review P2). |
| 2026-11-04 | The setup token goes to `<data dir>/setup-token` (0600), never to the log | CLAUDE.md §3 forbids logging tokens, and journald is often readable by more than the owner (security review P1). The log names the file. |
| 2026-11-04 | On a loopback listener, `Host` must be a loopback name | Defeats DNS rebinding, which the Origin check alone cannot see (security review P3). |
| 2026-11-04 | The daemon caps browse listings at 100,000 entries and transfer listings at 1 million | The CLI's 10 million is sized for a one-shot command; a long-lived daemon on a 512 MiB box needs a tighter bound (security review P4). |
| 2026-11-04 | Key installation appends to `authorized_keys` over SFTP, with the password used for one connection only | No remote command (`ssh-copy-id` runs a shell snippet). Appending keeps the file's other keys, owner and mode; the key blob is compared so a second install adds nothing; a login with the new key proves it. |
| 2026-11-04 | Web UI: plain JavaScript modules and CSS, no framework, no build step, no npm packages; embedded with `include_bytes!` | Compared with Svelte and Solid (plan.md §5): both need a Vite build that pulls in hundreds of npm packages, which is a supply chain larger than the whole Rust tree for six screens and one user. With no packages there is no lockfile to audit, the shipped code is the reviewed code, and the strict CSP (`default-src 'self'`, no inline script or style) holds. All DOM text goes through `textContent`, never `innerHTML`. `node --check` validates syntax in tests when node is present. Revisit if the UI outgrows this. |
| 2026-11-04 | The UI is served by the same router at `/`, unauthenticated, behind the same Host check and headers | Assets hold no data; every data call is an authenticated API request. One origin keeps cookies SameSite=Strict and CSRF checks simple. |
| 2026-11-04 | Event streams re-check their session before each snapshot, without refreshing its idle timer | The middleware checks a session once per request, and a stream lives for hours: after logout or a password change it kept streaming (Phase 3 review W2). Re-checking only on a change costs one indexed read per snapshot; not touching `seen` keeps a forgotten open tab from holding a session alive. |
| 2026-11-04 | Shutdown ends event streams and gives other requests 5 seconds | Graceful shutdown waited for every open browser tab, so SIGTERM never stopped the daemon (Phase 3 review W1). A key install or a large listing may still be cut at 5 s; the jobs it would affect are resumable. |
| 2026-11-04 | Web UI moves to React 19, TypeScript, Vite and Tailwind 4 (replaces the plain JavaScript decision above) | Owner choice: the UI must stay easy to change and upgrade over the long run. Studied `autobrr/qui` (GPL-2.0-or-later, so ideas only, never its code). Its fast live numbers come from updating only what changed and from virtual lists, which this stack gives directly. Cost: an npm supply chain, contained by the entries below. |
| 2026-11-04 | npm supply chain: exact versions; pnpm 10 `minimumReleaseAge` refuses anything under 14 days old, direct or indirect; `pnpm-lock.yaml` committed; install scripts off; `pnpm audit` before a release | CLAUDE.md §3. Hopsync holds SSH credentials, so a compromised package is the main new risk. Most malicious releases are found within days. Nothing from npm runs on the server: only the built files ship, and they are compiled into the binary. |
| 2026-11-04 | Native `<dialog>` instead of Radix Dialog; own toasts instead of Sonner; native `<select>`; Radix used only for Tabs | Read the packages: Sonner and `react-style-singleton` (under Radix Dialog) add `<style>` elements at runtime, which the CSP blocks. Native `<dialog>` already traps focus and handles Escape. `@radix-ui/react-tabs` adds no styles. The CSP stays `default-src 'self'`. |
| 2026-11-04 | `motion` for animated numbers and enter or exit transitions, without `popLayout` or view transitions | Those two features inject `<style>`. The rest writes through the CSSOM, which the CSP allows. Numbers animate through motion values that write to the DOM directly, so a 60 fps counter causes no React render. Compared with CSS only, which cannot interpolate digits. |
| 2026-11-04 | `@tanstack/react-virtual` for file and result lists | Listings reach 100,000 entries (Decision above), and every row in the DOM made big folders slow. qui uses the same library. Compared with `react-window`, which has fixed-size assumptions and needs an extra package for auto-sizing. |
| 2026-11-04 | `dist/` is not committed; `build.rs` embeds it, or a page that says how to build it; `HOPSYNC_REQUIRE_UI=1` turns a missing UI into a build error | Compared with committing bundles (unreviewable diffs) and with `build.rs` running pnpm (npm code running inside `cargo build`, and no offline builds). Rust contributors still build and test without node. Release builds set the variable. The generated table needs no new crate. |
| 2026-11-04 | Hashed files under `/assets/` are cached for a year and served gzipped when accepted; everything else stays `no-store` | A hash in the name means the content cannot change, and they hold no data. Loading the bundle over a 200 ms tunnel should not repeat on every page load. |
| 2026-11-04 | Default theme is dark, with a light theme a click away | An assumption, not an owner decision: the owner saw this direction and did not object, so it is easy to change. The choice lives in `localStorage` as a per-viewer convenience. |
| 2026-11-04 | SSE sends a named `heartbeat` event every 5 s when idle and re-checks its session then; the UI reconnects after 15 s of silence | Keep-alive comments are invisible to `EventSource`, so a stalled stream looked like a healthy idle one. The browser test also found that on an idle daemon a revoked session's stream stayed open forever: the session was checked only when a snapshot changed. |
| 2026-11-04 | `main` is production and only the owner can change it; new work reaches `dev` by pull request; enforced with repository rulesets | Owner choice: keep production stable while accepting outside work. Compared with classic branch protection, whose per-user push restrictions exist only for organization repositories. A ruleset can restrict updates to `main` and let only the repository admin role bypass. |
| 2026-11-06 | Release binaries link against glibc 2.17 through `cargo-zigbuild`, not static musl (changes §3) | Measured on loopback tmpfs, 1 GiB, median of 3, same batch: native glibc 764 MB/s push and 1,200 pull; glibc target 804 and 1,176; musl 592 and 464, so musl loses 61 % on pull. The cause was not proved (no profiler here); musl's allocator is the usual suspect. A separate run, with other containers on the machine in both batches, gave glibc 2.17 832 and 1,337 against native 833 and 1,303. Musl plus `mimalloc` was not tried, because it adds a C dependency to solve a problem the glibc target does not have. glibc 2.17 ran on CentOS 7, Debian 10 to 12 and Ubuntu 20.04 to 24.04. Cost: no Alpine. Revisit if users ask. |
| 2026-11-06 | Cross builds use `cargo-zigbuild` 0.24.1 with zig 0.17.1 | Compared with `cross`: its last release is 0.2.5 from February 2023 and it needs Docker for each target, while cargo-zigbuild had 1.9 M recent downloads and a release this week. It can pick the glibc version per target, which the Decision above needs. It runs only at build time and ships nothing. |
| 2026-11-06 | Password login falls back to `keyboard-interactive`, and sends the password only to a single hidden prompt | A real OpenSSH with only that method refused the correct password. Prompts that are visible or come in pairs are not asking for just the account password, so they get a plain rejection and never see it. A pinned host gets the password either way, so this adds no exposure. Rounds are capped at 4. |
| 2026-11-06 | RSA user keys are refused when loaded, with the `ssh-keygen` command in the message | Without the `rsa` feature russh still loads such a key but signs with SHA-1, which current OpenSSH refuses, and the user saw only "rejected the key". The 2026-11-03 decision against RSA stands. |
| 2026-11-06 | Release builds are signed with GitHub build attestations (Sigstore), not minisign | No long-lived signing key to store, lose or leak; the record also names the workflow and commit that built the file. Cost: verifying needs the `gh` CLI or a Sigstore client, so `sha256sums.txt` ships too. Compared with minisign, which is simpler to verify but needs a private key held as a repository secret. |
| 2026-11-06 | Actions pinned by commit hash; the job that builds has read-only access and the job that can write runs no repository code | A moved tag or a poisoned build script must not reach release publishing (CLAUDE.md §3). No dependency caches in the release job, for the same reason. |
| 2026-11-06 | A held queue and a stop pressed on a running job are saved in the store (schema 2), and a restart honors both | Both lived only in memory, so a restart started a held queue, and a daemon that died in the moment after a pause or cancel ran that job again (found by a TLA+ model, then reproduced on the binary). The stop is a new column, not a new state: the job really is still running until the transfer reports back, and a transfer that ends first keeps its real result. Cost: a binary from before schema 2 refuses this database. |
| 2026-11-06 | pnpm 9 is refused through `engines`, not supported | pnpm 9 ignores `minimumReleaseAge`, so letting it install would quietly drop the 14-day rule. `packages` in the workspace file exists so pnpm 9 reaches the `engines` check instead of a confusing parse error. Corepack is no help: Node 25 and later do not ship it. |

| 2026-11-07 | Install is `curl`/`wget` piped to `install.sh`, then `hopsync setup` | Owner's choice. The script only places the binary, so setup can ask its questions on a real terminal, which a script read from a pipe cannot do reliably. It is a release file, so it matches the archives beside it, and it verifies them against `sha256sums.txt`. |
| 2026-11-07 | Setup picks one of three modes: system unit, user unit, or a detached process with a cron `@reboot` line | Boxes differ. Servarica has root and systemd; Seedhost (measured over SSH) has neither root nor `systemctl --user`, but has cron. A mode that only printed an error would leave shared seedboxes out, and they are a main target. `hopsync status`, `start`, `stop`, `restart` and `logs` work the same in all three. |
| 2026-11-07 | Setup sets the password through the daemon's own `/api/setup`, after checking the port's owner in `/proc/net/tcp` | No second way to set a password, so hashing, rules and audit stay in one place, and root never has to create files the service account owns. On a shared box another customer can listen on a loopback port, so the port must belong to the service's uid before the password is sent. |
| 2026-11-07 | Setup's terminal output is written here on `rustix` termios, not `cliclack` or `indicatif` + `rpassword` | Compared on 2026-11-07: all are maintained (cliclack 0.5.7, indicatif 0.18.6, rpassword 7.5.4; MIT or Apache-2.0). In a clean project, `cliclack` pulls 58 crates including the ICU stack, and `indicatif` + `rpassword` pull 9 and offer no hop animation. CLAUDE.md prefers fewer dependencies, and this needs only ANSI codes and raw input, in about 300 lines with `rustix`, already a dependency. |
| 2026-11-07 | Setup refuses folders with spaces or other shell, unit-file or cron characters | Each of those formats escapes differently, and one mistake is a command injection. A plain allowlist cannot get that wrong. A symlink covers the rare folder with a space. |
| 2026-11-07 | A system service runs as the owner of an existing folder, or the `hopsync` account for a new one, and a root-owned folder is refused | Never root (CLAUDE.md section 3). Setup never runs `chown -R` on existing files unasked: on a seedbox those can be terabytes another program is using. |

| 2026-11-07 | First release is `v1.0.0`, tagged `v1.0.0` and titled "prod v1.0.0" | Owner's choice, ahead of the Phase 5 plan and with built-in TLS, fuzzing, packages and an independent security review still open (README, Project status). The release workflow accepts only `vX.Y.Z` tags that match `Cargo.toml`, so a tag named `prodV1.0.0` would not start it. |
| 2026-11-07 | `hopsync setup` refuses a folder holding `.ssh`, the home, or the data folder; default without root is `~/hopsync` | Real test on Seedhost: v1.0.0 defaulted to the whole home, so a web login could write `authorized_keys` and `.bashrc`, which is running commands (`CLAUDE.md` section 3). A subfolder such as `~/downloads` works, and `~` is understood at the prompt. |
| 2026-11-07 | Call glibc `malloc_trim` after each job and each login | Real test on Servarica: 105 MiB stayed in glibc arenas when idle after a 2 GiB push, against 1.3 MiB before it. Compared: a fixed `M_MMAP_THRESHOLD` (needs a call at start, and leaves Argon2 and SSH buffers in arenas), and mimalloc or jemalloc (a new dependency, more than this needs). `libc` is already a dependency. |
| 2026-11-07 | Drop a source file's whole page range when its reader closes | Real test: 44 MiB of a 2 GiB source stayed cached after the job, because read-ahead from one lane brought back ranges another lane had dropped. |
| 2026-11-07 | The web UI can serve HTTPS on the server's IP, built in: `tokio-rustls` with the `aws-lc-rs` backend and TLS 1.3 only, `rcgen` for a self-signed certificate | Owner's request: open the UI at the main IP like filebrowser or qBittorrent, but with TLS. Compared `axum-server` (0.9.0, adds its own accept loop and more crates) and `tokio-rustls` directly (232M recent downloads, the approach axum's own example uses, and it lets the listener run handshakes in their own tasks). `aws-lc-rs` is already in the tree through russh, so no second crypto library. `rustls-pemfile` is unmaintained, so PEM parsing comes from `rustls-pki-types`. |
| 2026-11-07 | Self-signed by default, fingerprint shown by setup; own certificate through `--tls-cert` and `--tls-key`; no ACME | A bare IP has no certificate a browser trusts without a public CA, and ACME needs ports 80 or 443, which a box without root cannot bind. Comparing the printed SHA-256 fingerprint with the browser's warning is what makes the first visit safe. No HSTS: it would make a self-signed certificate's exception impossible to accept. Validity 825 days, Apple's limit; renewed at start within 30 days of expiry or when the names change. |
| 2026-11-07 | The TLS listener answers by first byte: TLS gets HTTPS, plain HTTP from loopback is served, plain HTTP from elsewhere is redirected to `https://` | Typing `http://ip:port` otherwise gives a browser error about an invalid response. Loopback stays plain so SSH tunnels and setup's own password call keep working. |
| 2026-11-07 | Each handshake runs in its own task with a 10 s limit; at most 32 open connections per IP and 1024 in all | This listener faces the internet: one stalled client must not block `accept`, and one address must not use up the connections. A distributed flood is out of scope and belongs to a firewall. |
| 2026-11-07 | Public listening is IPv4 (`0.0.0.0`) | `/proc/net/tcp` is what setup's port-owner check reads, and every box tested has IPv4. An IPv6-only box keeps the SSH tunnel for now. |

## 11. Open questions (owner to decide)

1. **End-to-end verification default.** Re-reading the destination to hash it doubles destination read
   I/O. Proposed: off by default (SSH already guarantees integrity in transit, chunks are durability-checked),
   available per job or per server. Owner to confirm.
2. **A → B when Hopsync runs on neither.** Relay through the Hopsync box in v1, or require Hopsync on A or B
   until peer mode exists?
3. ~~**License.**~~ Decided 2026-11-03: MIT OR Apache-2.0 (see Decision log).

## 12. Left to the builder

Exact chunk sizes, lane defaults and auto-tune algorithm (from Phase 0 measurements); DB schema; API
shape; frontend framework (after Phase 0 comparison); UI layout details.
