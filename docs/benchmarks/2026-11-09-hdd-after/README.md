# 2026-11-09: 32 GiB push between two HDD seedboxes, after the HDD plan

Same boxes, file and scripts as `../2026-11-08-hdd/` (`benches/hdd/`). All four runs were made back
to back in one window on 2026-11-08 evening CET, so they share the same background load on the shared
disks. "Hopsync after" is the build with every step of
`docs/plans/2026-11-08-hdd-throughput-plan.md` (steps 1 to 7), run with its defaults.

| Run | Time | Average MiB/s | CPU | Max RSS | Source busy / iowait | Destination busy (samples over 90 %) |
|---|---|---|---|---|---|---|
| Ideal pipe (`dd` 16 MiB direct, one `ssh`, `dd` + fdatasync) | 4 m 25 s | 123.7 | 30 % | 18 MB | 65 % / 11 % | 62 % (71 of 132) |
| rclone 1.53, SFTP, hash check off | 9 m 11 s | 59.5 | 46 % | 65 MB | 38 % / 10 % | 32 % (74 of 273) |
| Hopsync before (old build, defaults) | 15 m 30 s | 35.2 | 15 % | 52 MB | 72 % / 47 % | 22 % (2 of 460) |
| **Hopsync after (defaults)** | **3 m 31 s** | **155.0** | 57 % | 162 MB | 69 % / 8 % | 90 % (98 of 106) |

Hopsync after is 4.4 times the old build, 2.6 times rclone and 1.25 times the ideal one-stream pipe,
and the source disk does it at the same busy level the old build needed for 35 MiB/s, with iowait
down from 47 % to 8 %. The limit is now the destination disk: busy over 90 % in 98 of 106 samples.
An earlier run of a near-final build (`hopsync-after2`, without the governor, buffer reuse and
per-disk turns) reached 172.8 MiB/s with the destination 89 % busy. The 10 % gap is not explained
yet: this run's destination was saturated, which points at the shared destination disk, but no run
isolates the governor's share.

Files: `NAME-src.csv` (source disk, `mon.sh`), `dst/NAME-dst.csv` (destination disk), `NAME.time`
(`/usr/bin/time -v`), `summary.txt`, `idle-final.csv` (30 s idle sample before the runs). User and
host names are replaced with `user@dst`.
