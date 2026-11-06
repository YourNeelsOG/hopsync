#!/bin/sh
# Runs inside a container or on a target machine: the release binary must start its daemon and answer
# its own CLI with only the system's C library around. Usage: smoke.sh /path/to/hopsync
set -eu
bin=$1
dir=$(mktemp -d)
trap 'kill "$pid" 2>/dev/null || true; rm -rf "$dir"' EXIT
"$bin" --version
mkdir "$dir/files"
"$bin" serve --listen 127.0.0.1:0 --data-dir "$dir/data" --root "$dir/files" \
    --known-hosts "$dir/known_hosts" --journal "$dir/journal.sqlite" --socket "$dir/s" \
    >"$dir/log" 2>&1 &
pid=$!
tries=0
until [ -S "$dir/s" ]; do
    tries=$((tries + 1))
    if [ "$tries" -gt 100 ] || ! kill -0 "$pid" 2>/dev/null; then
        cat "$dir/log"
        echo "the daemon did not start" >&2
        exit 1
    fi
    sleep 0.1
done
"$bin" servers --socket "$dir/s"
"$bin" jobs --socket "$dir/s"
echo "smoke test passed"
