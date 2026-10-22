#!/usr/bin/env bash
# Build the offline cargo vendor tarball for the OBS package: vendor.tar.zst.
#
# Replaces the cargo_vendor source service for local builds. It runs `cargo
# vendor` against the source tarball produced by make-source-tarball.sh, in a
# scratch copy, so the repository is never touched.
#
# The archive holds vendor/ (the crates) and .cargo/config.toml, which the spec
# unpacks next to the source with `%autosetup -a1`.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

if [ "$#" -ge 1 ]; then
    version="$1"
else
    version="$(grep -m1 '^version = ' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
fi

source_tar="dist/vibemix-${version}.tar.gz"
if [ ! -f "$source_tar" ]; then
    echo "error: $source_tar not found; run packaging/opensuse/make-source-tarball.sh first" >&2
    exit 1
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/vibemix-vendor.XXXXXX")"
trap 'rm -rf "$work"' EXIT

tar -xzf "$source_tar" -C "$work"
tree="$work/vibemix-${version}"

(
    cd "$tree"
    mkdir -p .cargo
    cargo vendor --locked --versioned-dirs vendor > .cargo/config.toml
    tar --zstd -cf "$root/dist/vendor.tar.zst" vendor .cargo/config.toml
)

echo "dist/vendor.tar.zst"
