#!/bin/sh
# Build a glibc Linux release binary with an explicit glibc floor.
#
#   ci/build-linux-gnu.sh x86_64-unknown-linux-gnu
#   ci/build-linux-gnu.sh aarch64-unknown-linux-gnu
#
# Why not just `cargo build`: a binary built on the runner inherits *that*
# machine's glibc as its floor. Building on ubuntu-22.04 produced a binary
# requiring GLIBC_2.34, which does not start on RHEL 8, Debian 11 or Ubuntu
# 20.04 — all distros uvr otherwise supports, and all of which uvr can still
# be useful on, since it finds and manages a system R when the portable builds
# won't run there.
#
# The old fix for this was to build on an older runner, but GitHub retired
# ubuntu-20.04 and there is nothing older to move to. zig ships every glibc
# version's stubs, so `cargo zigbuild --target <triple>.<glibc>` pins the floor
# regardless of the host — and cross-compiles aarch64 in the bargain, which
# replaces the gcc-aarch64-linux-gnu sysroot that had the same problem.
set -eu

TARGET="${1:?usage: ci/build-linux-gnu.sh <target-triple>}"

# Matches PPM_MANYLINUX_GLIBC_MIN in r_version/downloader.rs — the glibc below
# which uvr can offer no binary R packages at all. There is no point running
# on anything older, and no reason to exclude anything newer.
GLIBC_FLOOR=2.28

ZIG_VERSION=0.13.0
CACHE="${ZIG_CACHE_DIR:-${HOME}/.cache/uvr-ci}"

# Pinned from https://ziglang.org/download/index.json. This toolchain links the
# binaries users install, so it is checked before it is run: a compiler fetched
# over the network and executed unverified is a straight path from a CDN
# compromise into a shipped artifact. Bump both together.
zig_sha256() {
    case "$(uname -m)" in
        x86_64)  echo "d45312e61ebcc48032b77bc4cf7fd6915c11fa16e4aad116b66c9468211230ea" ;;
        aarch64) echo "041ac42323837eb5624068acd8b00cd5777dac4cf91179e8dad7a7e90dd0c556" ;;
        *) echo "" ;;
    esac
}

if ! command -v zig >/dev/null 2>&1; then
    zig_dir="$CACHE/zig-linux-$(uname -m)-$ZIG_VERSION"
    if [ ! -x "$zig_dir/zig" ]; then
        want="$(zig_sha256)"
        [ -n "$want" ] || { echo "no pinned zig checksum for $(uname -m)" >&2; exit 1; }
        echo ">>> fetching zig $ZIG_VERSION"
        mkdir -p "$CACHE"
        tarball="$CACHE/zig-$ZIG_VERSION-$(uname -m).tar.xz"
        curl --proto '=https' --tlsv1.2 -sSfL -o "$tarball" \
            "https://ziglang.org/download/$ZIG_VERSION/zig-linux-$(uname -m)-$ZIG_VERSION.tar.xz"
        # Verify before unpacking, not after: tar runs code paths on the
        # archive either way, and an unpacked tree is already a foothold.
        got="$(sha256sum "$tarball" | cut -d' ' -f1)"
        if [ "$got" != "$want" ]; then
            rm -f "$tarball"
            echo "!!! zig $ZIG_VERSION checksum mismatch" >&2
            echo "    expected $want" >&2
            echo "    got      $got" >&2
            exit 1
        fi
        tar -xJf "$tarball" -C "$CACHE"
        rm -f "$tarball"
    fi
    PATH="$zig_dir:$PATH"
    export PATH
fi

command -v cargo-zigbuild >/dev/null 2>&1 || cargo install cargo-zigbuild --locked
rustup target add "$TARGET"

# Every C dependency here (lzma, bzip2, zstd, ring) vendors its source and only
# prefers a system library when pkg-config finds one. That makes the artifact
# depend on which machine built it: a builder with libbz2-dev installed emits a
# binary needing libbz2.so.1.0, a Debian SONAME that RHEL does not ship. Take
# the system out of the decision.
LZMA_API_STATIC=1
PKG_CONFIG_LIBDIR=/nonexistent
PKG_CONFIG_PATH=/nonexistent
export LZMA_API_STATIC PKG_CONFIG_LIBDIR PKG_CONFIG_PATH

echo ">>> building $TARGET against glibc $GLIBC_FLOOR"
cargo zigbuild --release --target "$TARGET.$GLIBC_FLOOR" --bin uvr

out="target/$TARGET/release/uvr"
[ -f "$out" ] || { echo "expected $out" >&2; exit 1; }

# The floor is the whole point of this script, so assert it rather than trust
# it: one dropped flag and the binary silently goes back to needing whatever
# the runner had.
max="$(objdump -T "$out" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -1)"
if [ "$(printf '%s\n%s\n' "$GLIBC_FLOOR" "$max" | sort -V | tail -1)" != "$GLIBC_FLOOR" ]; then
    echo "!!! $out requires GLIBC_$max, above the $GLIBC_FLOOR floor" >&2
    exit 1
fi

echo ">>> $out requires at most GLIBC_$max"

# Same reasoning one level up: a needed library the target distro names
# differently is as fatal as a glibc symbol it lacks, and just as invisible on
# the machine that built it. Only the C runtime is allowed.
needed="$(objdump -p "$out" | awk '/NEEDED/ {print $2}')"
for lib in $needed; do
    case "$lib" in
        libc.so.6|libm.so.6|libdl.so.2|libpthread.so.0|librt.so.1|ld-linux*) ;;
        *) echo "!!! $out needs $lib, which is not part of the C runtime" >&2; exit 1 ;;
    esac
done
echo ">>> needs only: $(echo "$needed" | tr '\n' ' ')"
