#!/bin/sh
# Exercise a *released* uvr binary in one Linux distribution.
#
# Invoked by .github/workflows/distro-suite.yml, once per image in
# crates/uvr-core/tests/distro_matrix.json, via:
#
#   docker run --rm -v "$PWD:/work" -w /work <image> sh ci/distro-suite.sh
#
# `docker run` rather than a `container:` job on purpose: node-based actions
# need a glibc the minimal and musl images don't have, so the checkout happens
# on the host and the tree is mounted in. One shape for every image.
#
# This deliberately does not build uvr here. Users don't cargo build it —
# install.sh drops a release binary onto their machine — so the artifact under
# test is the one that ships, selected by the same libc rule install.sh uses.
# Building from source in each container would paper over exactly the failures
# that matter: a gnu binary whose glibc floor is above what the distro ships,
# or a musl binary picked on a glibc host.
#
# Env:
#   DIST         directory of release builds (default: ./dist)
#   R_VERSIONS   space-separated R versions to install (default: 4.5.1)
#   SMOKE_PKG    package with an external system library (default: xml2)
#   SKIP_STAGES  space-separated stage names to skip (default: none)
set -eu

DIST="${DIST:-./dist}"
R_VERSIONS="${R_VERSIONS:-4.5.1}"
SMOKE_PKG="${SMOKE_PKG:-xml2}"
SKIP_STAGES="${SKIP_STAGES:-}"
R_LATEST="${R_VERSIONS##* }"
# Where the tree is mounted; stages cd into scratch projects and back.
ROOT="$(pwd)"

# ---------------------------------------------------------------- helpers ---

group() { printf '::group::%s\n' "$*"; }
endgroup() { printf '::endgroup::\n'; }
note() { printf '\n>>> %s\n' "$*"; }
fail() { printf '\n!!! FAIL: %s\n' "$*" >&2; exit 1; }

skipped() {
    for s in $SKIP_STAGES; do
        [ "$s" = "$1" ] && return 0
    done
    return 1
}

# --------------------------------------------------------- distro plumbing ---

# The one place that knows package-manager dialects. Adding a distro to the
# matrix must not mean adding a line of YAML — if its package manager is
# already known here, the image just works.
detect_pm() {
    for pm in apt-get dnf microdnf zypper apk pacman yum; do
        if command -v "$pm" >/dev/null 2>&1; then
            echo "$pm"
            return 0
        fi
    done
    fail "no supported package manager found in this image"
}

PM="$(detect_pm)"
note "package manager: $PM"

pm_install() {
    case "$PM" in
        apt-get) DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$@" ;;
        dnf|yum) "$PM" -y install "$@" ;;
        microdnf) microdnf -y install "$@" ;;
        zypper) zypper --non-interactive --gpg-auto-import-keys install -y "$@" ;;
        apk) apk add --no-cache "$@" ;;
        pacman) pacman -S --noconfirm --needed "$@" ;;
    esac
}

pm_refresh() {
    case "$PM" in
        apt-get) apt-get update ;;
        zypper) zypper --non-interactive --gpg-auto-import-keys refresh ;;
        pacman) pacman -Sy --noconfirm ;;
        *) : ;;
    esac
}

# Runtime only — what a user has before they compile anything:
#   ca-certificates                TLS to the CDN and CRAN
#   fontconfig (+ a font on musl)  R probes these at startup
#   which                          R's `utils` calls system(which ...) in
#                                  .onLoad and fails to load without it; the
#                                  minimal RPM images ship no `which`, Debian
#                                  and busybox do
#   libxml2 runtime                load the binary SMOKE_PKG
# Deliberately absent: a compiler, and libxml2's *headers*. The stages below
# run on a bare image precisely to prove the shipped binary needs neither.
runtime_prereqs() {
    case "$PM" in
        apt-get) echo "ca-certificates fontconfig libxml2" ;;
        dnf|yum|microdnf|pacman) echo "ca-certificates fontconfig libxml2 which" ;;
        zypper) echo "ca-certificates fontconfig libxml2-2 which" ;;
        apk) echo "ca-certificates fontconfig ttf-dejavu libxml2" ;;
    esac
}

# Only for the source-build stage: R compiles C for xml2.
build_prereqs() {
    case "$PM" in
        apt-get) echo "build-essential pkg-config" ;;
        dnf|yum|microdnf) echo "gcc gcc-c++ make pkgconf-pkg-config" ;;
        zypper) echo "gcc gcc-c++ make pkg-config" ;;
        # build-base does not pull pkgconf on Alpine, and R's anticonf
        # configure scripts need it to find libxml-2.0.
        apk) echo "build-base musl-dev pkgconf" ;;
        pacman) echo "gcc make pkgconf" ;;
    esac
}

# The tarball tools are requested by *command*, not by package: the RHEL images
# ship curl-minimal, and asking for `curl` there is a package conflict rather
# than a no-op. Only name a package when its command is genuinely absent.
tools() {
    for pair in "tar tar" "gzip gzip" "xz $(xz_pkg)"; do
        cmd=${pair%% *}
        pkg=${pair#* }
        command -v "$cmd" >/dev/null 2>&1 || printf '%s ' "$pkg"
    done
}

xz_pkg() {
    case "$PM" in
        apt-get) echo "xz-utils" ;;
        *) echo "xz" ;;
    esac
}

# uvr's auto-installer knows apk/dnf/apt-get (sync.rs::pick_sysreqs_installer).
# On zypper/pacman hosts it would shell out to a package manager that isn't
# there, so those distros assert the *diagnosis* instead of the install.
sysreqs_autoinstall_supported() {
    case "$PM" in
        apt-get|dnf|yum|apk) return 0 ;;
        *) return 1 ;;
    esac
}

# ------------------------------------------------------------- stage: deps ---

if ! skipped prereqs; then
    group "Install runtime prerequisites ($PM)"
    pm_refresh
    # shellcheck disable=SC2046  # deliberate word splitting: a package list
    pm_install $(runtime_prereqs) $(tools)
    endgroup
fi

# ------------------------------------------------- stage: pick the artifact ---

# The same rule install.sh applies, so the matrix exercises the binary a user
# on this distro would actually receive — including the choice itself.
libc=gnu
if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
    libc=musl
elif [ -f /etc/alpine-release ]; then
    libc=musl
fi
arch="$(uname -m)"
[ "$arch" = arm64 ] && arch=aarch64
TARGET="${arch}-unknown-linux-${libc}"
UVR="$DIST/uvr-$TARGET/uvr"

note "install.sh would fetch: uvr-$TARGET.tar.gz"
[ -x "$UVR" ] || fail "no release build for $TARGET under $DIST"

# `--version` on a bare image is the whole glibc-floor question in one line: a
# gnu binary linked against a newer glibc than this distro ships dies right
# here, which is what a user hits seconds after running install.sh.
group "uvr --version ($TARGET)"
"$UVR" --version || fail "the $TARGET release binary does not run on this distro"
"$UVR" --help > /dev/null
endgroup

group "uvr doctor"
"$UVR" doctor || true
endgroup

# --------------------------------------------------------------- stage: R ---

if ! skipped r; then
    group "Install R ($R_VERSIONS)"
    for v in $R_VERSIONS; do
        "$UVR" r install "$v"
    done
    "$UVR" r list --all
    endgroup
fi

# ---------------------------------------------------- stage: binary package ---

# #175: a binary from the wrong distro's repo installs happily and only fails
# at library() — so the assertion has to load the package, not just install it.
# There is still no compiler on this image, so a "binary" install that quietly
# falls back to a source build fails here instead of passing for the wrong
# reason.
#
# musl hosts sit this one out: P3M publishes no musl repo and the portable
# manylinux fallback needs glibc, so there is no binary to select and `uvr add`
# correctly compiles from source — which is the *next* stage's subject, on an
# image that by then has a compiler. Alpine's coverage is the sysreqs path
# (#30), not this one.
if [ "$libc" = musl ]; then
    note "skipping the binary stage: no binary repo exists for musl"
elif ! skipped binary; then
    group "Binary package install ($SMOKE_PKG)"
    work=/tmp/binary-smoke
    rm -rf "$work" && mkdir -p "$work" && cd "$work"
    "$UVR" init --here binary-smoke --r-version "$R_LATEST"
    printf 'library(%s); cat("binary-smoke-ok\\n")\n' "$SMOKE_PKG" > check.R
    "$UVR" add "$SMOKE_PKG"
    "$UVR" run check.R
    cd "$ROOT"
    endgroup
fi

# --------------------------------------------------------- stage: sysreqs ---

# The #209 regression test, at the real end of the chain.
#
# The image has libxml2's runtime library but not its headers, so a source
# build of xml2 cannot succeed unless uvr resolves this distro's devel package
# and installs it. Nothing here greps a warning string: the build either
# compiles or it does not, and it only compiles if every link — os-release
# parse, catalog naming, package-manager probe, installer dialect — is right
# for this distro.
#
# Which is exactly what #209 broke: RHEL asked the catalog under a name it does
# not publish, got nothing back, installed nothing, and the build failed. A
# pre-fix binary fails this stage with `libxml/tree.h: No such file or
# directory` having printed no warning at all — which is why the assertion is
# "the build works" rather than "the warning is absent".
if ! skipped sysreqs; then
    group "System dependency resolution ($SMOKE_PKG from source)"
    # shellcheck disable=SC2046  # deliberate word splitting: a package list
    pm_install $(build_prereqs)

    work=/tmp/sysreqs-smoke
    rm -rf "$work" && mkdir -p "$work" && cd "$work"
    "$UVR" init --here sysreqs-smoke --r-version "$R_LATEST"
    printf 'library(%s); cat("sysreqs-smoke-ok\\n")\n' "$SMOKE_PKG" > check.R

    if sysreqs_autoinstall_supported; then
        UVR_INSTALL_SYSREQS=1 "$UVR" add "$SMOKE_PKG" --no-binary 2>&1 | tee add.log
        "$UVR" run check.R
    else
        # zypper/pacman: uvr cannot run the install itself, so assert it at
        # least named the package a human would then install.
        "$UVR" add "$SMOKE_PKG" --no-binary > add.log 2>&1 || true
        grep -Eq 'libxml2-dev(el)?' add.log \
            || fail "uvr did not name libxml2's devel package; see add.log"
    fi

    grep -q 'System dependency check skipped' add.log \
        && fail "uvr skipped the sysreqs check on this distro (#209 shape)"

    cd "$ROOT"
    endgroup
fi

note "distro suite complete"
