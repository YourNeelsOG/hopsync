#!/bin/sh
# Installs the Hopsync binary for this machine from a GitHub release, after checking its checksum.
# Usage: curl -fsSL https://github.com/YourNeelsOG/hopsync/releases/latest/download/install.sh | sh
#    or: wget -qO- https://github.com/YourNeelsOG/hopsync/releases/latest/download/install.sh | sh
#
# Root installs to /usr/local/bin, any other user to ~/.local/bin. It needs no root and installs no
# other packages. It starts nothing: `hopsync setup` does that, so it can ask its questions on your
# terminal, which a script read from a pipe cannot do reliably.
#
# Settings, all optional:
#   HOPSYNC_VERSION      a release tag such as v0.2.0, instead of the latest
#   HOPSYNC_INSTALL_DIR  where to put the binary
#   HOPSYNC_BASE_URL     a mirror holding the release files
#
# Everything is inside main, which runs on the last line: a download cut off halfway does nothing.

main() {
    set -eu
    repo=https://github.com/YourNeelsOG/hopsync

    # Colour only on a terminal, and never when NO_COLOR is set. Fixed strings go through printf %b.
    if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-dumb}" != dumb ]; then
        blue='\033[38;5;33m' green='\033[38;5;42m' red='\033[38;5;203m' dim='\033[2m' off='\033[0m'
    else
        blue='' green='' red='' dim='' off=''
    fi
    say() { printf '%b  %s%b\n' "$blue" "$1" "$off"; }
    done_step() { printf '%b  ok%b  %s\n' "$green" "$off" "$1"; }
    die() {
        printf '%b  hopsync install: %s%b\n' "$red" "$1" "$off" >&2
        exit 1
    }

    [ "$(uname -s)" = Linux ] || die "Hopsync runs on Linux; this system is $(uname -s)."
    machine=$(uname -m)
    case "$machine" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        armv7l | armv7*) arch=armv7 ;;
        *) die "there is no build for the CPU type $machine (x86_64, aarch64 and armv7 are built)." ;;
    esac
    # The release is linked against glibc. On musl systems such as Alpine it cannot start.
    if ldd --version 2>&1 | grep -qi musl; then
        die "this system uses musl (Alpine and similar). Hopsync needs glibc: Debian, Ubuntu, Fedora, RHEL and others."
    fi

    # Refuses a redirect to plain HTTP, unless the user chose a mirror, which may not have HTTPS.
    if command -v curl >/dev/null 2>&1; then
        if [ -n "${HOPSYNC_BASE_URL:-}" ]; then
            fetch() { curl -fsSL "$1" -o "$2"; }
        else
            fetch() { curl --proto '=https' --tlsv1.2 -fsSL "$1" -o "$2"; }
        fi
    elif command -v wget >/dev/null 2>&1; then
        if [ -n "${HOPSYNC_BASE_URL:-}" ]; then
            fetch() { wget -q "$1" -O "$2"; }
        else
            fetch() { wget -q --https-only "$1" -O "$2"; }
        fi
    else
        die "this needs curl or wget, and neither is installed."
    fi
    if command -v sha256sum >/dev/null 2>&1; then
        sha() { sha256sum "$1" | cut -d' ' -f1; }
    elif command -v shasum >/dev/null 2>&1; then
        sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
    else
        die "this needs sha256sum or shasum to check the download, and neither is installed."
    fi

    if [ -n "${HOPSYNC_BASE_URL:-}" ]; then
        base=$HOPSYNC_BASE_URL
    elif [ -n "${HOPSYNC_VERSION:-}" ]; then
        base=$repo/releases/download/$HOPSYNC_VERSION
    else
        base=$repo/releases/latest/download
    fi
    if [ -n "${HOPSYNC_INSTALL_DIR:-}" ]; then
        dir=$HOPSYNC_INSTALL_DIR
    elif [ "$(id -u)" -eq 0 ]; then
        dir=/usr/local/bin
    else
        [ -n "${HOME:-}" ] || die "HOME is not set; set HOPSYNC_INSTALL_DIR."
        dir=$HOME/.local/bin
    fi

    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT INT TERM HUP

    name=hopsync-$arch-linux
    say "Hopsync for $arch"
    fetch "$base/$name.tar.gz" "$work/$name.tar.gz" || die "could not download $base/$name.tar.gz"
    fetch "$base/sha256sums.txt" "$work/sums" || die "could not download $base/sha256sums.txt"
    done_step "downloaded"

    # Lines look like `HASH  ./name.tar.gz`; compare the whole name so no other file can match.
    want=$(awk -v f="$name.tar.gz" '{ n = $2; sub(/^\.\//, "", n); if (n == f) print $1 }' "$work/sums")
    [ -n "$want" ] || die "the checksum list does not mention $name.tar.gz; not installing."
    got=$(sha "$work/$name.tar.gz")
    [ "$want" = "$got" ] || die "checksum mismatch for $name.tar.gz; not installing. Expected $want, got $got."
    done_step "checksum matches"

    tar -xzf "$work/$name.tar.gz" -C "$work" "$name/hopsync" || die "could not unpack the archive."
    mkdir -p "$dir" || die "could not create $dir."
    # A rename in the same folder is atomic, and works while an old copy of the binary is running.
    cp "$work/$name/hopsync" "$dir/.hopsync.new" || die "could not write to $dir."
    chmod 0755 "$dir/.hopsync.new"
    mv -f "$dir/.hopsync.new" "$dir/hopsync"
    done_step "installed to $dir/hopsync"

    printf '\n'
    case ":$PATH:" in
        *":$dir:"*) run=hopsync ;;
        *)
            run=$dir/hopsync
            printf '%b  %s is not on your PATH. Add it with:%b\n' "$dim" "$dir" "$off"
            printf '    export PATH="%s:$PATH"\n\n' "$dir"
            ;;
    esac
    say "Next, start the service and web UI:"
    printf '    %s setup\n' "$run"
    if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
        printf '\n%b  No root is needed. With sudo, this runs it as a system service instead:%b\n' "$dim" "$off"
        printf '    sudo %s/hopsync setup\n' "$dir"
    fi
}

main "$@"
