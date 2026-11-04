#!/usr/bin/env bash
# Phase 0 real-link sweep: lanes for both backends and directions, plus russh window sizes on download.
#
# Usage: wan_sweep.sh <out.jsonl> <host> <user> <key> <known_hosts> <ssh_config> <remote sparse file>
# The remote sparse file must be at least SIZE bytes. Uploads go to the remote /dev/null.
# Each line of output is one sftp-bench JSON record, or {"error": ...} for a failed run.
set -euo pipefail

out=$1 host=$2 user=$3 key=$4 known_hosts=$5 ssh_config=$6 down_file=$7
size=${SIZE:-256M}
reps=${REPS:-2}
workers=${TOKIO_WORKER_THREADS:-2}
bench=target/release/sftp-bench

run() {
    local rec
    if rec=$(TOKIO_WORKER_THREADS=$workers timeout 600 "$bench" host="$host" user="$user" key="$key" \
        known_hosts="$known_hosts" ssh_config="$ssh_config" size="$size" zc=1 "$@" 2>/dev/null); then
        echo "$rec" | tail -1 | tee -a "$out"
    else
        echo "{\"error\":\"exit $?\",\"args\":\"$*\"}" | tee -a "$out"
    fi
}

for rep in $(seq "$reps"); do
    for backend in russh openssh; do
        for lanes in 1 2 4 8; do
            run backend=$backend dir=down remote="$down_file" lanes=$lanes
            run backend=$backend dir=up remote=/dev/null lanes=$lanes
        done
    done
    for window in 16M 64M; do
        for lanes in 1 4; do
            run backend=russh dir=down remote="$down_file" lanes=$lanes window=$window
        done
    done
done
