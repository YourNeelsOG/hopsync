# Phase 0: SSH/SFTP library candidates

Status: done 2026-11-03. Loopback (`loopback-results.md`) and real-link (`wan-results.md`)
benchmarks run. Decision: `russh` + `russh-sftp` (plan.md Decision log).

## Sources

- crates.io API, queried 2026-11-03 (versions, release dates, downloads, licenses, dependencies).
- RustSec advisory database as fetched by `cargo audit` on 2026-11-03 (1403 advisories).

Claims marked **(unverified)** come from project docs or general knowledge. The spike must confirm them.

## Candidates

| Crate | Latest | Released | Downloads, 90 days | License | Approach |
|---|---|---|---|---|---|
| `russh` | 0.65.0 | 2026-10-25 | 4.2 M | Apache-2.0 | Pure Rust SSH client/server on tokio |
| `russh-sftp` | 3.0.1 | 2026-09-28 | 2.0 M | Apache-2.0 | SFTP client/server over a russh channel |
| `openssh` | 0.11.6 | 2025-12-03 | 0.49 M | MIT OR Apache-2.0 | Drives the system `ssh` binary (ControlMaster mux) |
| `openssh-sftp-client` | 0.15.11 | 2026-11-02 | 0.28 M | MIT | Async SFTP client over any stdin/stdout pair, e.g. `ssh -s sftp` |
| `ssh2` | 0.9.6 | 2026-06-30 | 1.3 M | MIT OR Apache-2.0 | Blocking bindings to libssh2 (C) |
| `async-ssh2-lite` | 0.5.0 | 2024-07-21 | 33 k | Apache-2.0 OR MIT | Async wrapper over ssh2 |
| `libssh-rs` | 0.3.8 | 2026-07-05 | 45 k | MIT | Bindings to libssh (C) |

### Advisories (RustSec)

- `russh`: RUSTSEC-2026-0154 (agent frame length unbounded allocation, DoS) and RUSTSEC-2026-0153
  (unchecked `CryptoVec` growth, in `russh-cryptovec`). Both patched in `>=0.60.3`. Both are
  memory-exhaustion bugs reachable from a peer, which matters because Hopsync talks to remotes that may be hostile.
- `russh-sftp`, `openssh`, `openssh-sftp-client`, `ssh2`, `libssh2-sys`: no entries. For `ssh2` this covers
  only the Rust side; libssh2 C CVEs are tracked upstream, not in RustSec.

### Dependency footprint (direct, non-dev)

- `russh` 0.65.0: 67 crates. Default features `aws-lc-rs`, `rsa`, `flate2`; `ring` is an alternative crypto
  backend. `des` and `dsa` are optional and must stay off (legacy algorithms, plan.md §6.1).
- `russh-sftp` 3.0.1: 13 crates.
- `openssh` 0.11.6: 8 crates. `openssh-sftp-client` 0.15.11: 13 crates.
- `ssh2` 0.9.6: 4 crates plus vendored libssh2 C code.

## Shortlist for the spike

1. **`russh` + `russh-sftp`.** In-process, so host-key pinning, password auth and cipher choice stay under
   our control. Most used and actively released. Open questions for the spike: throughput per connection
   against OpenSSH, CPU per Gbps, and whether `russh-sftp` keeps many read/write requests in flight
   **(unverified)**.
2. **`openssh` + `openssh-sftp-client`.** Uses the system OpenSSH, so it gets OpenSSH's crypto speed and
   ssh_config handling **(unverified for speed)**. The SFTP client is built for request pipelining
   **(unverified)**. Costs: needs `ssh` on the Hopsync host; password auth is awkward (OpenSSH reads it from a
   tty or askpass); host-key pinning has to go through our own `UserKnownHostsFile` with
   `StrictHostKeyChecking=yes`; arguments passed to `ssh` need guarding against option injection (for example
   a host name starting with `-`). Only the `sftp` subsystem is used, so no path reaches a shell.

Dropped:

- `ssh2`: blocking API, C library outside RustSec coverage. Kept only as an optional reference if time allows.
- `async-ssh2-lite`: no release since 2024-07.
- `libssh-rs`: low adoption, C library.

## Benchmark design (spike)

A throwaway binary in `spikes/sftp-bench`, run on box A against box B, and on loopback.

- **Backends:** `russh`+`russh-sftp`, `openssh`+`openssh-sftp-client`. Baselines run by a script with the
  same file: `scp`, OpenSSH `sftp`, `rsync -e ssh`, `rclone --multi-thread-streams`, `lftp pget -n`.
- **Matrix:** upload and download; lanes 1, 2, 4, 8; in-flight requests per lane 1, 16, 64; request size
  32 KiB to 256 KiB (capped by `limits@openssh.com` when the server reports it).
- **Data path:** two modes. Memory to `/dev/null` and `/dev/zero` on the remote isolates network and crypto
  **(unverified that OpenSSH sftp-server reads `/dev/zero` past its zero size)**. File to file with a 20 GiB
  test file measures the real path.
- **Ciphers:** `aes128-gcm@openssh.com`, `aes256-gcm@openssh.com`, `chacha20-poly1305@openssh.com`.
- **Record per run:** throughput (MB/s, median of 3), CPU user+sys seconds per GB on both ends, peak RSS,
  `Cached` and `Dirty` from `/proc/meminfo` on the local side, errors and retries.
- **Checklist:** each result is checked with the `benchmark-checklist` skill before it goes in the Decision log.
