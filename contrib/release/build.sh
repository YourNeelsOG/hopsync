#!/bin/sh
# Builds one release archive. Usage: build.sh RUST_TARGET ARCH_NAME OUT_DIR
# Needs cargo-zigbuild and zig, and web/dist already built. The target carries the oldest glibc to
# link against (x86_64-unknown-linux-gnu.2.17); plan.md's Decision log says why it is not musl.
set -eu
target=$1
arch=$2
out=$3
triple=${target%%.*}
export HOPSYNC_REQUIRE_UI=1
cargo zigbuild --release --locked -p hopsync --target "$target"
root=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
name=hopsync-$arch-linux
mkdir -p "$stage/$name" "$out"
cp "$root/$triple/release/hopsync" LICENSE-APACHE LICENSE-MIT README.md "$stage/$name/"
cp -r contrib/systemd "$stage/$name/systemd"
# Fixed order, owner and time: the same inputs give the same archive.
stamp=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$stamp" \
    -C "$stage" -cf - "$name" | gzip -n >"$out/$name.tar.gz"
