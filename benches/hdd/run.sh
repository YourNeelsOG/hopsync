#!/bin/bash
# run.sh NAME -- CMD...
#
# Runs one timed transfer on the source box with a cold page cache and both disks monitored.
# Run it from a bench directory on the source box that holds mon.sh, the test file, and a key.
#
# Environment:
#   DST       user@host of the destination (required)
#   DST_DIR   bench directory on the destination, holding mon.sh (default: hopsync-bench)
#   SRC_DEV   source block device as in /proc/diskstats, e.g. sdl (required)
#   DST_DEV   destination block device, e.g. sdab (required)
#   FILE      test file on the source (default: test32g.bin)
#   KEY       private key for DST (default: key); KNOWN known_hosts file (default: known_hosts)
#
# The transfer must write DST_DIR/NAME.bin. Logs go to logs/NAME-*.csv here and on the destination.
set -u
: "${DST:?} ${SRC_DEV:?} ${DST_DEV:?}"
DST_DIR=${DST_DIR:-hopsync-bench} FILE=${FILE:-test32g.bin} KEY=${KEY:-key} KNOWN=${KNOWN:-known_hosts}
name=$1; shift; [ "${1:-}" = "--" ] && shift
mkdir -p logs
SSH=(ssh -i "$KEY" -o IdentitiesOnly=yes -o UserKnownHostsFile="$KNOWN" -o StrictHostKeyChecking=yes -o BatchMode=yes "$DST")
size=$(stat -c %s "$FILE")

"${SSH[@]}" "rm -f $DST_DIR/$name.bin $DST_DIR/$name.bin.hopsync-part; mkdir -p $DST_DIR/logs; sync -f $DST_DIR"
dd if="$FILE" iflag=nocache count=0 2>/dev/null
sleep 5
"${SSH[@]}" "cd $DST_DIR && (nohup timeout 7200 ./mon.sh $DST_DEV 'sftp-server|rclone|hopsync' 2 logs/$name-dst.csv >/dev/null 2>&1 &)"
(timeout 7200 ./mon.sh "$SRC_DEV" 'hopsync|rclone' 2 "logs/$name-src.csv" >/dev/null 2>&1 &)
start=$(date +%s.%N)
/usr/bin/time -v "$@" > "logs/$name.out" 2> "logs/$name.time"
rc=$?
end=$(date +%s.%N)
sleep 4
# Bracketed patterns so pkill never matches the shell that runs it.
pkill -u "$(id -u)" -f "[m]on.sh $SRC_DEV"
got=$("${SSH[@]}" "pkill -u \$(id -u) -f '[m]on.sh $DST_DEV'; stat -c %s $DST_DIR/$name.bin 2>/dev/null || echo 0")
awk -v s="$start" -v e="$end" -v n="$name" -v rc="$rc" -v size="$size" -v got="$got" 'BEGIN{
  d = e - s
  printf "%s rc=%d secs=%.1f avg_MiBs=%.1f size_ok=%s\n", n, rc, d, size/1048576/d, (got == size ? "yes" : "NO")
}' | tee -a logs/summary.txt
