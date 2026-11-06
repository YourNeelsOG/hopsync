#!/bin/sh
# Runs smoke.sh from every release archive in DIST_DIR: x86_64 on old and current distros, ARM under
# QEMU inside a container, so the host's binfmt settings stay untouched. Needs Docker.
set -eu
dist=$(cd "$1" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
run() {
    image=$1
    shift
    docker run --rm -v "$dist:/dist:ro" -v "$here/smoke.sh:/smoke.sh:ro" "$image" sh -c "$*"
}
unpack='mkdir /x && tar -xzf /dist/hopsync-$A-linux.tar.gz -C /x && B=/x/hopsync-$A-linux/hopsync'
for image in centos:7 debian:11-slim ubuntu:22.04 ubuntu:24.04; do
    echo "== x86_64 on $image"
    run "$image" "A=x86_64; $unpack && sh /smoke.sh \$B"
done
qemu() {
    echo "== $1 under QEMU"
    run debian:12-slim "A=$1; $unpack && apt-get update -qq >/dev/null &&
        apt-get install -y -qq --no-install-recommends qemu-user-static $3 $4 >/dev/null 2>&1 &&
        printf '#!/bin/sh\nexec $2 -L $5 '\$B' \"\$@\"\n' >/q && chmod +x /q && sh /smoke.sh /q"
}
qemu aarch64 qemu-aarch64-static libc6-arm64-cross libgcc-s1-arm64-cross /usr/aarch64-linux-gnu
qemu armv7 qemu-arm-static libc6-armhf-cross libgcc-s1-armhf-cross /usr/arm-linux-gnueabihf
echo "all release binaries passed"
