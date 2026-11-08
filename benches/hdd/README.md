# HDD benchmarks

Measures a large transfer between two boxes whose disks are hard drives, and how hard each disk
works for it. Used for the 2026-11-08 HDD throughput plan
(`docs/plans/2026-11-08-hdd-throughput-plan.md`); results are in `docs/benchmarks/2026-11-08-hdd/`.

- `mon.sh DEV REGEX INTERVAL OUT`: samples one block device from `/proc/diskstats` and the CPU of
  this user's processes matching `REGEX`. Writes CSV: utilisation, MiB/s, IOPS, average request
  size, process CPU, system iowait. Needs no root and no sysstat.
- `run.sh NAME -- CMD...`: drops the test file from the page cache, starts `mon.sh` on both boxes,
  times `CMD`, and appends one line to `logs/summary.txt`. See the header for its environment.

## Setup

On the source box, in a bench directory (for example `~/hopsync-bench`): `mon.sh`, `run.sh`, a test
file of random data, a key the destination accepts, and a `known_hosts` with the destination's pinned
key. On the destination: the same directory name with `mon.sh` in it.

```bash
head -c 32G /dev/urandom > test32g.bin && dd if=test32g.bin iflag=nocache count=0
```

## Runs used for the plan

```bash
export DST=user@dst SRC_DEV=sdl DST_DEV=sdab
# Ideal pipe: one sequential stream each side; the ceiling for any tool on this pair of disks.
./run.sh pipe16m -- sh -c 'dd if=test32g.bin bs=16M iflag=direct status=none |
  ssh -c aes128-gcm@openssh.com -i key -o UserKnownHostsFile=known_hosts "$DST" \
  "dd of=hopsync-bench/pipe16m.bin bs=16M iflag=fullblock conv=fdatasync status=none"'
# rclone (SFTP backend, hash check off to match Hopsync's default).
RCLONE_CONFIG_DST_TYPE=sftp RCLONE_CONFIG_DST_HOST=dst RCLONE_CONFIG_DST_USER=user \
RCLONE_CONFIG_DST_KEY_FILE=$PWD/key RCLONE_CONFIG_DST_DISABLE_HASHCHECK=true \
  ./run.sh rclone -- rclone copyto test32g.bin dst:hopsync-bench/rclone.bin
# Hopsync with its defaults.
./run.sh hopsync -- hopsync --known-hosts known_hosts --journal logs/journal.sqlite \
  push -i key test32g.bin "$DST:hopsync-bench/hopsync.bin"
```

Disks on shared seedboxes also serve other customers: record an idle sample first
(`timeout 30 ./mon.sh sdl x 3 logs/idle.csv`) and compare runs made close together.
