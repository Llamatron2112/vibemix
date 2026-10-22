#!/usr/bin/env bash
# Build the OBS source tarball for vibemix: vibemix-<version>.tar.gz.
#
# The tarball contains the full working tree, including the vendored VST3 SDK
# and CLAP submodules. Cargo crates are not vendored here; the OBS
# cargo_vendor service produces vendor.tar.zst from this tarball.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

if [ "$#" -ge 1 ]; then
    version="$1"
else
    version="$(grep -m1 '^version = ' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
fi

if [ -d .git ]; then
    git submodule update --init --recursive
fi

stage="vibemix-${version}"
rm -rf "dist/${stage}" "dist/${stage}.tar.gz"
mkdir -p "dist/${stage}"

tar -cf - \
    --exclude=.git \
    --exclude=target \
    --exclude=dist \
    . | tar -xf - -C "dist/${stage}"

tar -C dist -czf "dist/${stage}.tar.gz" "${stage}"
echo "dist/${stage}.tar.gz"
