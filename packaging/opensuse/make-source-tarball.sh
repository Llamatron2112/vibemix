#!/usr/bin/env bash
# Build the source tarball for a release: dist/vibemix-<version>.tar.gz.
#
# The tree is a fresh clone of the given git ref (default: HEAD), with the
# submodules (third_party/vst3sdk, third_party/clap) checked out at the commits
# the ref pins. Untracked and stale files in the working copy never end up in
# the tarball. Cargo crates are not vendored here; make-vendor-tarball.sh does
# that.
#
# Usage: make-source-tarball.sh [git-ref]
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
ref="${1:-HEAD}"

if ! git -C "$root" rev-parse --verify --quiet "${ref}^{commit}" >/dev/null; then
    echo "error: unknown git ref: $ref" >&2
    exit 1
fi

version="$(git -C "$root" show "${ref}:Cargo.toml" | grep -m1 '^version = ' | sed 's/.*"\(.*\)".*/\1/')"
stage="vibemix-${version}"

work="$(mktemp -d "${TMPDIR:-/tmp}/vibemix-src.XXXXXX")"
trap 'rm -rf "$work"' EXIT

git clone --quiet --no-checkout "$root" "$work/$stage"
git -C "$work/$stage" checkout --quiet "$ref"
git -C "$work/$stage" submodule update --init --recursive --quiet

# Every submodule must be populated, or the build fails much later.
while read -r _ path; do
    if [ -z "$(ls -A "$work/$stage/$path")" ]; then
        echo "error: submodule $path is empty" >&2
        exit 1
    fi
done < <(git -C "$work/$stage" config -f .gitmodules --get-regexp '^submodule\..*\.path$')

find "$work/$stage" -name .git -prune -exec rm -rf {} +

mkdir -p "$root/dist"
tar -C "$work" -czf "$root/dist/${stage}.tar.gz" "$stage"
echo "$root/dist/${stage}.tar.gz"
