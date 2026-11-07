#!/bin/bash
# Runs `hopsync setup` for real inside systemd containers: root with systemd on Debian, Ubuntu and a
# RHEL-family distro, a non-root user with a user manager, and a non-root user with neither (a shared
# seedbox). Needs Docker, and a binary that runs on old glibc (contrib/release/build.sh makes one).
# Usage: setup-e2e.sh /path/to/hopsync
set -u
bin=$(realpath "$1")
here=$(cd "$(dirname "$0")" && pwd)
PW=correct-horse-battery
total_fail=0

image() {
    tag=$1 from=$2 family=$3
    case "$family" in
        deb) add='RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends systemd systemd-sysv dbus libpam-systemd curl openssl hostname python3 cron sudo procps passwd && rm -f /lib/systemd/system/getty* && systemctl mask systemd-logind.service 2>/dev/null; systemctl unmask systemd-logind.service' ;;
        rpm) add='RUN dnf install -y -q systemd curl-minimal openssl hostname python3 cronie sudo procps-ng shadow-utils util-linux --allowerasing' ;;
    esac
    docker build -q -t "$tag" - >/dev/null <<EOF
FROM $from
$add
EOF
}

# Starts a container with systemd as PID 1, and waits until it is up.
boot() {
    docker run -d --rm --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
        --tmpfs /run --tmpfs /run/lock --tmpfs /tmp:exec,mode=1777 \
        -v "$bin:/opt/incoming/hopsync:ro" -v "$here/setup-e2e-inner.sh:/inner.sh:ro" \
        -e container=docker "$1" /sbin/init
}

run_case() {
    name=$1 tag=$2 fn=$3
    if [ -n "${E2E_ONLY:-}" ] && [ "$E2E_ONLY" != "$fn" ]; then return; fi
    echo "=== $name"
    c=$(boot "$tag")
    docker exec "$c" sh -c 'for i in $(seq 1 60); do s=$(systemctl is-system-running 2>/dev/null); case "$s" in running|degraded) exit 0;; esac; sleep 0.5; done; exit 0'
    docker exec -e PW="$PW" "$c" bash /inner.sh "$fn"
    rc=$?
    if [ "$rc" -ne 0 ]; then
        total_fail=$((total_fail + rc))
        docker exec "$c" sh -c 'journalctl -u hopsync --no-pager -n 25 2>/dev/null | cut -c1-200'
    fi
    docker rm -f "$c" >/dev/null
}

image hopsync-e2e-debian debian:12 deb
image hopsync-e2e-ubuntu ubuntu:22.04 deb
image hopsync-e2e-rocky rockylinux:9 rpm

run_case "root with systemd, Debian 12" hopsync-e2e-debian root_systemd
run_case "root with systemd, Ubuntu 22.04" hopsync-e2e-ubuntu root_systemd
run_case "root with systemd, Rocky 9" hopsync-e2e-rocky root_systemd
run_case "sudo user, Debian 12" hopsync-e2e-debian sudo_user
run_case "no root, no user systemd, Ubuntu 22.04 (like Seedhost)" hopsync-e2e-ubuntu shared_box
run_case "no root, no user systemd, Debian 12" hopsync-e2e-debian shared_box
run_case "no root, user systemd, Debian 12" hopsync-e2e-debian user_manager

echo "=== root without systemd"
out=$(docker run --rm -v "$bin:/h:ro" debian:12-slim sh -c 'printf "%s\n" "x" | /h setup --plain --password-stdin; echo "rc=$?"' 2>&1)
if echo "$out" | grep -q 'rc=1' && echo "$out" | grep -qi 'as root'; then
    echo "ok   - refused, because it will not run as root without a service"
else
    echo "FAIL - $out"
    total_fail=$((total_fail + 1))
fi

echo "total failures: $total_fail"
exit "$total_fail"
