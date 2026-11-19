#!/bin/sh
# Build the Linux .deb package into target/dist, with a SHA-256 checksum.
# Needs cargo-deb and cargo-about; see the Linux section of README.md.
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
version=$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "trash-amp"))')
if [ -n "${RELEASE_TAG:-}" ] && [ "$RELEASE_TAG" != "v$version" ]; then
    echo "Release tag $RELEASE_TAG does not match Cargo version v$version." >&2
    exit 1
fi

target=${CARGO_TARGET_DIR:-target}
cargo build --release --locked -p trash-amp
# cargo-deb packages these from target/release/notices.
rm -rf "$target/release/notices"
python3 scripts/generate-notices.py "$target/release/notices" >/dev/null

dist=$target/dist
mkdir -p "$dist"
deb=$(cargo deb -p trash-amp --no-build --output "$dist" | tail -n 1)
(cd "$dist" && sha256sum "$(basename "$deb")" > "$(basename "$deb").sha256")
printf '%s\n' "$deb" "$deb.sha256"
