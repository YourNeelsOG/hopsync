# Phase 1 benchmarks (2026-11-04)

`benches/bench.py` runs hopsync, `scp` and OpenSSH `sftp` on the same server and the same data, one
after another, each into an empty or fully rewritten destination. It records the wall time and
the peak resident memory of the client process tree (sampled from `/proc` every 10 ms). Raw rows
are in `results/`; the tables below are medians.

Release build of hopsync with defaults: lanes start at 4 and auto-tune up to 16, `--existing
overwrite`, resume journal off (`--no-resume`), page cache dropped after streaming. `scp` and
`sftp` use the same key, host key pin and OpenSSH client defaults.

## Loopback (results/loopback-2026-11-04.jsonl)

Both sides on tmpfs on this development machine, so the numbers measure protocol and CPU cost, not
disks or the network. 512 MiB file; 2,000 files of 0.5 to 4.5 KiB. 3 runs each.

| Scenario | hopsync | scp | sftp |
|---|---|---|---|
| Push one 512 MiB file | **734 MB/s** | 526 MB/s | 527 MB/s |
| Pull one 512 MiB file | **1057 MB/s** | 573 MB/s | 568 MB/s |
| Push 2,000 small files | **0.30 s** | 0.65 s | 0.52 s |
| Pull 2,000 small files | **0.28 s** | 0.67 s | 0.66 s |
| Peak client memory, big file | 40 MiB | 12 MiB | 13 MiB |

On loopback, one OpenSSH stream is limited by a single core doing cipher work. Hopsync spreads
the file over several connections, so several cores share it.

## Real link: a shared seedbox

A shared seedbox about 200 ms away (Phase 0 measured 203 ms RTT and a provider cap near
30 MiB/s per account). 256 MiB file; 300 files of 0.5 to 0.8 KiB. 2 runs each, because one
`scp` tree run takes over 4 minutes. The local side is tmpfs; the remote side is the provider's
disk, shared with other users.

| Scenario | hopsync | scp | sftp |
|---|---|---|---|
| Push one 256 MiB file | **11.7 MB/s** (23.0 s) | 5.9 MB/s (45.7 s) | 6.1 MB/s (43.9 s) |
| Pull one 256 MiB file | **15.7 MB/s** (17.1 s) | 7.9 MB/s (34.0 s) | 7.5 MB/s (35.7 s) |
| Push 300 small files | **39.0 s** | 249.0 s | 188.2 s |
| Pull 300 small files | **31.3 s** | 250.2 s | 251.5 s |
| Peak client memory, big file | 41 MiB | 15 MiB | 15 MiB |
| Peak client memory, tree | 11 MiB | 13 MiB | 13 MiB |

What explains the numbers:

- **Big files, about 2x.** At 200 ms, one SSH channel's window limits one stream to a few MB/s,
  which is exactly where `scp` and `sftp` sit. Hopsync runs several lanes. It does not reach the
  provider cap here: these runs include the auto-tune ramp (a lane is added every few seconds
  only while it adds 10 %), and a 256 MiB file is over in 20 seconds. Phase 0 showed 8 fixed
  lanes filling the cap on this box.
- **Small files, 5 to 8x.** `scp` and `sftp` handle one file at a time, paying several round
  trips (open, write, close, set times) of 200 ms each per file. Hopsync keeps `lanes × 4` files
  in flight and reuses the lookup's connection as its first lane.
- **Memory.** The big-file peak is the buffer pool (`lanes × in-flight pieces × piece size`),
  bounded by configuration and never by file size. Trees use fewer buffers at once.

## Caveats

- The seedbox is shared: other users' traffic and disk load vary between runs. The two
  repetitions of each scenario agree within 15 % (push file: 25.8 s and 20.2 s), which is the
  ramp-up difference.
- Loopback on tmpfs is a CPU benchmark. Real disks and real links change every number.
- Not yet compared: `rsync`, `rclone` and `lftp` (plan.md success criteria). That needs their
  equivalents of parallel streams set up fairly; it belongs in the Phase 4 benchmark page.

## Reproduce

```bash
python3 benches/bench.py --host SEEDBOX --user USER --key KEY \
  --known-hosts OPENSSH_KNOWN_HOSTS --hopsync-known-hosts HOPSYNC_KNOWN_HOSTS \
  --local-dir /dev/shm/bench --remote-dir hopsync-bench --size 256M --files 300 --reps 2 \
  --out wan.jsonl
```

The script only runs SFTP commands on the server. It removes everything it created there at the
end, unless `--keep-seed` is given.
