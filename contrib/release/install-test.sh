#!/bin/sh
# Tests install.sh in clean containers against a local mirror with fake archives: curl only, wget
# only, root and non-root install folders, and every refusal. Needs Docker. Usage: install-test.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# A fake release: the "binary" prints a version, so a test can tell which archive it came from.
mkdir -p "$work/site/hopsync-x86_64-linux" "$work/bad/hopsync-x86_64-linux"
printf '#!/bin/sh\necho "hopsync fake-good"\n' >"$work/site/hopsync-x86_64-linux/hopsync"
chmod +x "$work/site/hopsync-x86_64-linux/hopsync"
tar -czf "$work/site/hopsync-x86_64-linux.tar.gz" -C "$work/site" hopsync-x86_64-linux
(cd "$work/site" && sha256sum ./hopsync-x86_64-linux.tar.gz >sha256sums.txt)
rm -rf "$work/site/hopsync-x86_64-linux"
# The same names with an archive that does not match its checksum.
cp "$work/site/sha256sums.txt" "$work/bad/sha256sums.txt"
printf '#!/bin/sh\necho "hopsync fake-evil"\n' >"$work/bad/hopsync-x86_64-linux/hopsync"
chmod +x "$work/bad/hopsync-x86_64-linux/hopsync"
tar -czf "$work/bad/hopsync-x86_64-linux.tar.gz" -C "$work/bad" hopsync-x86_64-linux
rm -rf "$work/bad/hopsync-x86_64-linux"

cat >"$work/inner.sh" <<'INNER'
set -u
fails=0
ok() { echo "ok   - $1"; }
bad() { echo "FAIL - $1"; fails=$((fails + 1)); }
expect_ok() { if [ "$2" -eq 0 ]; then ok "$1"; else bad "$1 (exit $2): $3"; fi; }
expect_no() { if [ "$2" -ne 0 ]; then ok "$1"; else bad "$1 (should have failed): $3"; fi; }
SH="sh /t/install.sh"

python3 -m http.server 8801 --bind 127.0.0.1 --directory /t/site >/dev/null 2>&1 &
python3 -m http.server 8802 --bind 127.0.0.1 --directory /t/bad >/dev/null 2>&1 &
sleep 1
export HOPSYNC_BASE_URL=http://127.0.0.1:8801

out=$($SH 2>&1); rc=$?
expect_ok "root installs" "$rc" "$out"
[ "$(/usr/local/bin/hopsync)" = "hopsync fake-good" ] && ok "root installs to /usr/local/bin" || bad "root binary missing"
[ "$(stat -c %a /usr/local/bin/hopsync)" = "755" ] && ok "binary is mode 755" || bad "binary mode"
echo "$out" | grep -q 'hopsync setup' && ok "tells you to run hopsync setup" || bad "no next step: $out"
[ -z "$(ls -A /tmp | grep -v '^$')" ] && ok "leaves nothing in /tmp" || bad "left files: $(ls -A /tmp)"

out=$($SH 2>&1); rc=$?
expect_ok "a second run works" "$rc" "$out"

useradd -m tester
out=$(su tester -c "HOPSYNC_BASE_URL=$HOPSYNC_BASE_URL $SH" 2>&1); rc=$?
expect_ok "non-root installs" "$rc" "$out"
[ -x /home/tester/.local/bin/hopsync ] && ok "non-root installs to ~/.local/bin" || bad "non-root binary missing"
echo "$out" | grep -q 'PATH' && ok "non-root is told about PATH" || bad "no PATH hint: $out"

rm -f /usr/local/bin/hopsync
out=$(HOPSYNC_BASE_URL=http://127.0.0.1:8802 $SH 2>&1); rc=$?
expect_no "a tampered archive is refused" "$rc" "$out"
[ ! -e /usr/local/bin/hopsync ] && ok "nothing installed after a bad checksum" || bad "installed a tampered binary"
echo "$out" | grep -qi 'checksum' && ok "the refusal names the checksum" || bad "unclear refusal: $out"

out=$(HOPSYNC_BASE_URL=http://127.0.0.1:1 $SH 2>&1); rc=$?
expect_no "an unreachable mirror fails" "$rc" "$out"

mkdir -p /fake
printf '#!/bin/sh\n[ "$1" = "-s" ] && echo Linux && exit 0\necho sparc64\n' >/fake/uname; chmod +x /fake/uname
out=$(PATH=/fake:$PATH $SH 2>&1); rc=$?
expect_no "an unknown CPU is refused" "$rc" "$out"
echo "$out" | grep -q 'sparc64' && ok "the refusal names the CPU" || bad "unclear refusal: $out"

printf '#!/bin/sh\necho FreeBSD\n' >/fake/uname
out=$(PATH=/fake:$PATH $SH 2>&1); rc=$?
expect_no "a non-Linux system is refused" "$rc" "$out"
rm -f /fake/uname

out=$(HOPSYNC_INSTALL_DIR=/opt/hs $SH 2>&1); rc=$?
expect_ok "a chosen folder works" "$rc" "$out"
[ -x /opt/hs/hopsync ] && ok "installs to the chosen folder" || bad "chosen folder empty"

out=$(NO_COLOR=1 $SH 2>&1)
printf '%s' "$out" | grep -q "$(printf '\033')" && bad "colour on a pipe or with NO_COLOR" || ok "no escape codes without a terminal"

# Cut in the middle of main: as a whole file the script is only a function that never gets called.
rm -f /usr/local/bin/hopsync
head -c 3000 /t/install.sh >/tmp/cut.sh
sh /tmp/cut.sh >/dev/null 2>&1
[ ! -e /usr/local/bin/hopsync ] && ok "a cut-off download installs nothing" || bad "a partial script ran"
rm -f /tmp/cut.sh

echo "failures: $fails"
exit "$fails"
INNER

cp "$repo/install.sh" "$work/install.sh"
chmod -R a+rX "$work"

run_case() {
    name=$1
    image=$2
    packages=$3
    remove=$4
    echo "== $name"
    docker run --rm -v "$work:/t:ro" "$image" sh -c "
        apt-get update -qq >/dev/null && apt-get install -y -qq --no-install-recommends python3 ca-certificates $packages >/dev/null 2>&1 &&
        rm -rf /tmp/* && $remove sh /t/inner.sh"
}
run_case "curl on Debian 12" debian:12-slim curl ""
run_case "wget on Ubuntu 22.04" ubuntu:22.04 wget ""
echo "== neither curl nor wget"
docker run --rm -v "$work:/t:ro" debian:12-slim sh -c '
    if out=$(HOPSYNC_BASE_URL=http://127.0.0.1:1 sh /t/install.sh 2>&1); then echo "FAIL - should have refused"; exit 1; fi
    echo "$out" | grep -q "curl or wget" && echo "ok   - asks for curl or wget" || { echo "FAIL - $out"; exit 1; }'
echo "install tests passed"
