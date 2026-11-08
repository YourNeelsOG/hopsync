# 2026-11-08: 32 GiB push between two HDD seedboxes

Source: a shared seedbox (Toshiba MG06 HDD, ext4). Destination: another box in the same data centre
(same disk model). RTT 0.29 ms; iperf3 8.1 Gbit/s on one stream. Method and scripts:
`benches/hdd/`. Analysis and plan: `docs/plans/2026-11-08-hdd-throughput-plan.md`.

| Run | Time | Average MiB/s | CPU | Source busy / iowait | Destination busy |
|---|---|---|---|---|---|
| Ideal pipe (`dd` 16 MiB direct, one `ssh`, `dd` + fdatasync) | 4 m 26 s | 123.2 | 28 % | 68 % / 8 % | 75 % |
| rclone 1.53, SFTP, hash check off | 8 m 54 s | 61.4 | 44 % | 37 % / 10 % | 37 % |
| Hopsync before the plan (defaults, 6 lanes) | 15 m 02 s | 36.3 | 14 % | 72 % / 48 % | 20 % |

Files: `NAME-src.csv` (source disk, from `mon.sh`), `dst/NAME-dst.csv` and `dst/NAME-iostat.txt`
(destination disk), `NAME.time` (`/usr/bin/time -v`), `summary.txt`. `idle-*.csv` are 30 s idle
samples taken before the runs; `ddr.csv` and `dst/ddw.csv` are raw disk read and write tests.
User and host names are replaced with `user@dst`.
