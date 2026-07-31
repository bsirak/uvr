#!/bin/sh
# Run uvr's full test suite natively inside one Linux distribution.
#
# Invoked by .github/workflows/distro-suite.yml, once per image in
# crates/uvr-core/tests/distro_matrix.json, via:
#
#   docker run --rm -v "$PWD:/work" -w /work <image> sh ci/distro-suite.sh
#
# `docker run` rather than a `container:` job on purpose: node-based actions
# need a glibc the minimal and musl images don't have, so the checkout happens
# on the host and the source tree is mounted in. One shape for every image.
#
# Everything is compiled and executed with the distro's own toolchain against
# its own libc — cross-compiled or statically linked binaries would test the
# build host, which is the opposite of the point.
#
# Env:
#   R_VERSIONS   space-separated R versions to install (default: 4.5.1)
#   SMOKE_PKG    package with an external system library (default: xml2)
#   SKIP_STAGES  space-separated stage names to skip (default: none)
set -eu

R_VERSIONS="${R_VERSIONS:-4.5.1}"
SMOKE_PKG="${SMOKE_PKG:-xml2}"
SKIP_STAGES="${SKIP_STAGES:-}"
R_LATEST="${R_VERSIONS##* }"
# Where the source tree is mounted; stages cd into scratch projects and back.
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

# Base prerequisites, per family:
#   compiler + make + pkg-config  build uvr and any source R package
#   ca-certificates               rustup, the R tarball, CRAN over TLS
#   fontconfig (+ a font on musl) R's graphics device probes these at startup
#   libxml2 runtime               load the binary SMOKE_PKG
# Deliberately absent: libxml2's *headers*. Their absence is what the sysreqs
# stage below is testing.
prereqs() {
    case "$PM" in
        apt-get) echo "build-essential pkg-config ca-certificates fontconfig libxml2 procps" ;;
        dnf|yum|microdnf) echo "gcc gcc-c++ make pkgconf-pkg-config which ca-certificates fontconfig libxml2 findutils" ;;
        zypper) echo "gcc gcc-c++ make pkg-config which ca-certificates fontconfig libxml2-2" ;;
        apk) echo "build-base musl-dev ca-certificates fontconfig ttf-dejavu libxml2" ;;
        pacman) echo "gcc make pkgconf which ca-certificates fontconfig libxml2" ;;
    esac
}

# curl and the tarball tools are requested by *command*, not by package: the
# RHEL images ship `curl-minimal`, and asking for `curl` there is a package
# conflict rather than a no-op. Only name a package when its command is
# genuinely absent.
tools() {
    for pair in "curl curl" "tar tar" "gzip gzip" "xz $(xz_pkg)"; do
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
    group "Install prerequisites ($PM)"
    pm_refresh
    # shellcheck disable=SC2046  # deliberate word splitting: a package list
    pm_install $(prereqs) $(tools)
    endgroup
fi

# ------------------------------------------------------------- stage: rust ---

if ! skipped rust; then
    group "Install Rust toolchain"
    if command -v cargo >/dev/null 2>&1; then
        note "cargo already present: $(cargo --version)"
    elif [ "$PM" = apk ]; then
        # rustup-init from the distro so the toolchain links against musl.
        apk add --no-cache rustup
        rustup-init -y --default-toolchain stable --profile minimal --no-modify-path
    else
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --default-toolchain stable --profile minimal --no-modify-path
    fi
    endgroup
fi

if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi
cargo --version || fail "no cargo on PATH after toolchain install"

# Keep each distro's build products apart: the mounted source tree is shared
# with the host and with every other distro in the matrix, and objects linked
# against one libc must never be picked up by another.
CARGO_TARGET_DIR="/tmp/uvr-target"
export CARGO_TARGET_DIR
UVR="$CARGO_TARGET_DIR/debug/uvr"

# ------------------------------------------------------ stage: build + test ---

# Unconditional: every stage below runs the binary this produces, so there is
# nothing left to skip to if it is missing.
group "cargo build --all"
cargo build --all
endgroup

if ! skipped test; then
    # The whole suite, natively. Most of it is distro-independent logic, but it
    # is a few seconds once the tree is built and it is the only way to catch
    # the parts that are not: package-manager probing (sysreqs::filter_missing),
    # shell activation, path and permission handling.
    #
    # The *_live.rs tests are #[ignore]d and stay that way here; catalog
    # conformance is its own job and must not make this one flaky.
    group "cargo test --all"
    cargo test --all
    endgroup
fi

# --------------------------------------------------------------- stage: R ---

if ! skipped r; then
    group "Install R ($R_VERSIONS)"
    for v in $R_VERSIONS; do
        "$UVR" r install "$v"
    done
    "$UVR" r list --all
    endgroup

    # Activation resolves an R interpreter, so these skipped in `cargo test`
    # above — R did not exist yet. Re-run them now that it does. Shell-specific
    # cases self-skip when their shell is absent, which is the common case in a
    # minimal image.
    group "Activation tests against installed R"
    rroot="$HOME/.uvr/r-versions/$R_LATEST"
    PATH="$rroot/bin:$PATH"
    export PATH
    R --version
    cargo test --test cli_tests activate
    endgroup
fi

# ---------------------------------------------------- stage: binary package ---

# #175: a binary from the wrong distro's repo installs happily and only fails
# at library() — so the assertion has to load the package, not just install it.
if ! skipped binary; then
    group "Binary package install ($SMOKE_PKG)"
    "$UVR" doctor || true
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
# build of xml2 cannot succeed unless uvr correctly resolves the distro's
# devel package and installs it. Nothing here greps a warning string: the
# source build either compiles or it does not, and it only compiles if every
# link in the chain — os-release parse, catalog naming, package-manager probe,
# installer dialect — is right for this distro.
#
# Which is exactly what #209 broke: RHEL asked the catalog under a name it
# does not publish, got nothing back, installed nothing, and the build failed.
if ! skipped sysreqs; then
    group "System dependency resolution ($SMOKE_PKG from source)"
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
