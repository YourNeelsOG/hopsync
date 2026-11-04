# Phase 0: real-link results, home PC to a shared seedbox (2026-11-03)

## Setup

- Client: the home PC from `loopback-results.md`. Server: a shared seedbox
  (80-core Xeon E5-2673 v4 with AES-NI, OpenSSH 8.9p1 Ubuntu, load average about 6.5).
- Path: 203 ms RTT (ping, 0 % loss). Both ends allow 64 MiB TCP buffers and use BBR.
- **The owner reports a 30 MiB/s (31.5 MB/s) cap on this seedbox.** The top results below sit at
  that cap, so they measure the cap, not the libraries or the link.
- `spikes/sftp-bench/wan_sweep.sh`, 256 MiB per run, 2 repeats, aes128-gcm, 16 requests in flight per
  lane, 2 tokio workers, openssh backend with zero-copy writes. Pull reads a sparse file on the seedbox;
  push writes to the seedbox's `/dev/null`. Raw data is kept out of the repository because it names the server. 40 runs, no errors.

## Results (median of 2, MB/s)

| Lanes | russh pull | russh push | openssh pull | openssh push |
|---|---|---|---|---|
| 1 | 4.9 | 5.2 | 8.3 | 5.8 |
| 2 | 9.3 | 9.9 | 12.1 (8.6-15.5) | 10.2 |
| 4 | 17.0 | 20.1 | 16.0 (13.8-18.1) | 20.8 |
| 8 | 25.7 (21.4-30.0) | **33.6** | 27.6 | **33.0** |

russh pull with a bigger channel window (`client::Config::window_size`, default 2 MiB):

| Lanes | 2 MiB | 16 MiB | 64 MiB |
|---|---|---|---|
| 1 | 4.9 | 8.0 | 9.3 |
| 4 | 17.0 | 22.1 | 19.8 |

Connection setup (TCP, key exchange, auth, SFTP init): russh 2.7 to 3.1 s, openssh 3.2 to 3.4 s, about
14 to 16 round trips. Peak RSS: russh 46 MiB, openssh 70 MiB (8 lanes).

## Findings

1. **One SSH connection cannot use this path.** 5 to 8 MB/s per lane, whichever library. A 2 MiB
   window over 203 ms allows at most 10.3 MB/s, and per-lane results sit just below that.
2. **Lanes scale almost linearly until the cap.** 1 → 8 lanes takes push from about 5 to 33 MB/s,
   which is the 30 MiB/s cap. This confirms the core bet in plan.md §6.1.
3. **A bigger receive window helps pull per lane, but only up to about 9 MB/s.** Going from 2 to 16 MiB
   nearly doubles one lane (4.9 → 8.0); 64 MiB adds little (9.3). The next limit is not identified: candidates
   are the 16 × 255 KiB = 4 MiB of requests in flight per lane (4 MiB / 203 ms = 20 MB/s, so not
   binding alone), sshd's send side on the seedbox, or the seedbox's traffic shaping. Phase 1 auto-tune
   should treat window and in-flight depth as tunables and measure.
4. **Push depends on the server's window** (sshd's receive window, 2 MiB by default in OpenSSH), which
   Hopsync cannot change over plain SFTP. Push can only scale through lanes.
5. **russh and openssh-sftp-client are even on this path.** Both fill the cap at 8 lanes.
6. **russh pull costs 2 to 3 client CPU-s/GB at these low rates**, against 1.2 for push. At 5 to 30 MB/s
   this is 1 to 6 % of one core, so it does not matter here. Recheck at 1 Gbps or more.

## Not measured yet

- An uncapped path at 1 Gbps or more, and seedbox to seedbox. Needs two boxes without the cap.
- More than 8 lanes, and the seedbox's `MaxStartups` / fail2ban behaviour under many connections.
