# Phase 0: loopback benchmark results (2026-11-03)

Status: loopback only. No library decision yet; the real-link run is still needed (see "What loopback
cannot tell us").

## Setup

- Machine: AMD Ryzen 7 5700G (8 cores / 16 threads, AES-NI), 13 GiB RAM, Arch Linux, OpenSSH 10.6p1.
- Server: a throwaway `sshd` on `127.0.0.1:2222`, running as the user, with its own host key, `UsePAM no`,
  and default `MaxSessions`/`MaxStartups`. Server SFTP limits (`limits@openssh.com`): 261120 bytes max
  read and max write.
- Client: `spikes/sftp-bench` (Rust 1.99.1, release build), russh 0.65.0 + russh-sftp 3.0.1 (aws-lc-rs),
  openssh 0.11.6 + openssh-sftp-client 0.15.11. Baselines: OpenSSH `scp` and `sftp` CLIs.
- Data: 2 GiB per transfer. Uploads write to `/dev/null`; downloads read a 2 GiB sparse file. No disk I/O
  on either side, so this measures protocol and crypto cost only.
- Request size 255 KiB (the server's limit). Three repeats per configuration. Raw data:
  `results/loopback-2026-11-03.jsonl`. Reproduce with `spikes/sftp-bench/matrix.py`, then
  summarize with `summarize.py`.
- CPU: "sys CPU" is busy CPU time across the whole machine per GB moved, minus the idle load measured
  at the start (0.85 CPU-s/s). On loopback it covers client and server together. "Client CPU" is the
  bench process only; for the openssh backend it leaves out the `ssh` child processes, which do the crypto.
- Timing: sftp-bench times the transfer only. The `scp`/`sftp` baselines include connection setup
  (about 0.1 s of a roughly 2 s run).

## Results (median of 3; MB/s = 10^6 bytes/s)

### One connection, 16 requests in flight, by cipher

| Tool | Dir | aes128-gcm | aes256-gcm | chacha20-poly1305 |
|---|---|---|---|---|
| russh | up | 1066 | 1035 | 573 |
| russh | down | 879 | 862 | 590 |
| openssh-sftp-client | up | 1125 | 1107 | 608 |
| openssh-sftp-client | down | 817 | 806 | 511 |
| `sftp` CLI | up | 1173 | 1136 | 599 |
| `sftp` CLI | down | 1163 | 1111 | 604 |
| `scp` CLI | up | 1089 | 1142 | 575 |
| `scp` CLI | down | 1125 | 1106 | 607 |

### Requests in flight (one connection, aes128-gcm)

| Tool | Dir | 1 | 4 | 16 | 64 |
|---|---|---|---|---|---|
| russh | up | 655 | 1042 | 1066 | 1166 |
| russh | down | 435 | 703 | 879 | 883 |
| openssh-sftp-client | up | 262 | 549 | 1125 | 1143 |
| openssh-sftp-client | down | 262 | 390 | 817 | 883 |

### Lanes (separate SSH connections, 16 in flight each, aes128-gcm)

| Tool | Dir | 1 | 2 | 4 | 8 |
|---|---|---|---|---|---|
| russh | up | 1066 | 1522 | 1280 | 1146 |
| russh | down | 879 | 1690 | **2510** | 1818 |
| openssh-sftp-client | up | 1125 | 1797 | **2070** | 1537 |
| openssh-sftp-client | down | 817 | 1339 | 1572 | 1082 |

### CPU and memory (aes128-gcm, 1 lane, 16 in flight)

| Tool | Dir | sys CPU-s/GB | client CPU-s/GB | peak RSS MiB |
|---|---|---|---|---|
| russh | up | 3.62 | 1.88 | 20.8 |
| russh | down | 2.34 | 1.08 | 14.3 |
| openssh-sftp-client | up | 2.21 | 0.55 (+ `ssh` children) | 8.9 |
| openssh-sftp-client | down | 2.70 | 0.59 (+ `ssh` children) | 12.9 |
| `sftp` CLI | up | 1.82 | n/a | n/a |
| `sftp` CLI | down | 1.70 | n/a | n/a |

Peak RSS stayed under 75 MiB in every run, including 8 lanes × 16 requests.

## Findings

1. **Both libraries reach OpenSSH-class speed on one connection.** About 0.8 to 1.2 GB/s with AES-GCM,
   that is 7 to 9 times a 1 Gbps link. The software is not the 1 Gbps bottleneck.
2. **Pipelining is required.** One request in flight gives 262 to 655 MB/s; 16 in flight gets most of
   the gain. Hopsync should default to at least 16 per lane.
3. **AES-GCM is about 2× ChaCha20-Poly1305 on this AES-NI CPU** for every tool. This confirms the cipher
   rule in plan.md §6.1 (AES-GCM when the CPU has AES instructions).
4. **Multiple lanes scale up to 2 to 4 connections here**, reaching 2.0 to 2.5 GB/s (16 to 20 Gbps).
   Beyond that, loopback runs out of CPU because client and server share 16 threads (see the limits
   below), so the 8-lane drop is not a general result.
5. **The server's `limits@openssh.com` values must be honoured.** OpenSSH accepts at most 261120
   bytes per read or write. A 262144-byte write made sftp-server drop the connection
   (russh-sftp reported "sender dropped"). russh-sftp's `RawSftpSession` does not cap request sizes
   itself; openssh-sftp-client does.
6. **openssh-sftp-client's buffered `write_all` costs about 9 CPU-s/GB in the client process.**
   `write_all_zero_copy` costs 0.56 and is faster (646 vs 1058 MB/s on one lane). Measured in the
   smoke runs before the matrix. Hopsync must use the zero-copy API if it picks this crate.
7. **The `openssh` crate's default host-key policy is `KnownHosts::Add`** (accept unknown keys). Hopsync
   would have to force `Strict` and use its own known_hosts file. russh rejects every key by default
   until the `check_server_key` callback approves it.
8. **Dependency trees (normal deps, unique crates):** `russh` 126 + `russh-sftp` 44 (overlapping);
   `openssh-sftp-client` with the `openssh` feature 67. `cargo deny` passes for both (advisories, bans,
   licenses, sources).

## Explained: russh upload scaling (investigated 2026-11-03)

In the matrix, russh uploads stopped scaling after 2 lanes (1522 → 1280 → 1146 MB/s) while CPU per GB
more than doubled. The matrix ran with tokio's default runtime: 16 worker threads, one per CPU thread.

- **Per-process CPU** (`spikes/sftp-bench/cpu_by_process.py`, 4.29 GB upload, 4 lanes): russh made the
  bench use 12.7 s and the server's `sshd-session` 9.5 s. openssh-sftp-client used 9.7 s on the client
  (bench 3.8 + `ssh` 5.9) and 6.6 s on the server. No russh thread was saturated (busiest 0.35 of wall).
- **`nodelay` on or off:** no measurable difference (4 lanes, 2 runs each).
- **Tokio worker threads (`TOKIO_WORKER_THREADS`), russh upload:**

  | Lanes | 1 worker | 2 workers | 4 workers | 16 workers (default) |
  |---|---|---|---|---|
  | 1 | **1376** MB/s, 0.62 CPU-s/GB | 1325, 1.01 | 1346, 1.16 | 1066, 1.88 (matrix) |
  | 4 | 1615, 0.54 | 2071, 0.82 | **2164**, 1.54 | 1280, 3.17 (matrix) |
  | 8 | 1348, 0.63 | 1875, 0.87 | 1785, 1.79 | 1146, 3.67 (matrix) |

  Downloads at 4 lanes: 2238 (2 workers), **2581** (4), 2404 (16).

**Cause:** with 16 workers, the 64 in-flight request tasks bounce between threads, so the client spends
CPU on cross-thread wake-ups and cache traffic. On loopback that also takes cores away from the server,
which is why the server's CPU rose too. With a few workers, russh matches or beats openssh-sftp-client and
uses less client CPU (0.5 to 0.8 CPU-s/GB at 1 to 2 workers).

**Consequence for Hopsync:** build the tokio runtime with an explicit, small worker count (start around
2 to 4, tune in Phase 1). Never take the one-thread-per-CPU default. This matters even more on shared
seedboxes, where the torrent client competes for the same cores.

## Unexplained

- **Single-connection downloads through either library are 25 to 30 % slower than the `sftp` CLI**
  (817 to 879 vs 1163 MB/s). The CLI defaults to 64 requests of 32 KiB; the libraries were run with
  16 requests of 255 KiB.

## What loopback cannot tell us

- **No round-trip time.** On a real path the SSH channel window limits throughput to window ÷ RTT.
  russh's default window is 2 MiB (`client::Config::window_size`), which caps one lane at about
  100 MB/s at 20 ms RTT, below 1 Gbps. The real-link run must sweep `window=`.
- No packet loss, no TCP congestion control, no server-side disk.
- CPU is shared by client and server, so lane-scaling limits here are not the limits of a real pair of
  boxes.

## Lean (not a decision)

Performance is roughly even. russh is ahead on downloads and on security control: in-process host-key
pinning, password auth without a tty, and no subprocess arguments to guard. openssh-sftp-client is
ahead on uploads at 4 lanes and has a smaller dependency tree. Before deciding: explain the russh
upload scaling, then run on two real boxes with RTT, sweeping the window size.
