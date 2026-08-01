#!/bin/sh
# Capture the verbatim /etc/os-release identity from every image in the distro
# matrix.
#
# The whole point of the matrix is that uvr's idea of what a host reports must
# match what it actually reports — #209 was `VERSION_ID=8.10` where uvr assumed
# `8`. So these values are never hand-written: they are captured from the real
# image, here and in the nightly drift job.
#
# Usage:
#   ci/capture-os-release.sh                    # every image in the matrix
#   ci/capture-os-release.sh rhel-8 rocky-9     # only these keys
#
# Prints one JSON object per line: {"key":..,"image":..,"id":..,"version_id":..}
# Images pulled by this script are removed again; images already present are
# left alone.
set -eu

matrix="$(dirname "$0")/../crates/uvr-core/tests/distro_matrix.json"
runtime="${CONTAINER_RUNTIME:-docker}"

keys="$*"

jq -r '.distros[] | "\(.key)\t\(.image)"' "$matrix" | while IFS="$(printf '\t')" read -r key image; do
    if [ -n "$keys" ]; then
        case " $keys " in
            *" $key "*) ;;
            *) continue ;;
        esac
    fi

    had_image=yes
    "$runtime" image inspect "$image" >/dev/null 2>&1 || had_image=no

    if ! out="$("$runtime" run --rm --entrypoint "" "$image" cat /etc/os-release 2>/dev/null)"; then
        printf '{"key":"%s","image":"%s","error":"could not read /etc/os-release"}\n' "$key" "$image"
        [ "$had_image" = no ] && "$runtime" rmi -f "$image" >/dev/null 2>&1 || true
        continue
    fi

    # Same parse as crate::os_release: last wins, quotes stripped.
    id="$(printf '%s\n' "$out" | sed -n 's/^ID=//p' | tail -1 | tr -d '"'"'"'')"
    version_id="$(printf '%s\n' "$out" | sed -n 's/^VERSION_ID=//p' | tail -1 | tr -d '"'"'"'')"
    printf '{"key":"%s","image":"%s","id":"%s","version_id":"%s"}\n' \
        "$key" "$image" "$id" "$version_id"

    [ "$had_image" = no ] && "$runtime" rmi -f "$image" >/dev/null 2>&1 || true
done
