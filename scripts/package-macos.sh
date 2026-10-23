#!/bin/sh
# Package an Apple Silicon prerelease, preserving the executable and signature.
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
signed=${REQUIRE_MACOS_NOTARIZATION:-false}
case "${1:-}" in
    --signed) signed=true; shift ;;
esac
if [ "$#" -ne 0 ]; then echo "Usage: $0 [--signed]" >&2; exit 2; fi
if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
    echo "The prerelease package requires Apple Silicon macOS." >&2
    exit 1
fi
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
case "$CARGO_TARGET_DIR" in /*) ;; *) CARGO_TARGET_DIR="$PWD/$CARGO_TARGET_DIR" ;; esac
version=$(cargo metadata --manifest-path "$root/Cargo.toml" --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "trash-amp"))')
python3 "$root/scripts/macos_version.py" "$version" >/dev/null
if [ -n "${RELEASE_TAG:-}" ] && [ "$RELEASE_TAG" != "v$version" ]; then
    echo "Release tag $RELEASE_TAG does not match Cargo version v$version." >&2
    exit 1
fi
if [ -n "${RELEASE_TAG:-}" ]; then signed=true; fi
case "$signed" in
    true)
        : "${MACOS_SIGN_IDENTITY:?Tagged/signed packages require a Developer ID identity}"
        : "${MACOS_NOTARY_PROFILE:?Tagged/signed packages require a notarization profile}"
        ;;
    false) ;;
    *) echo 'REQUIRE_MACOS_NOTARIZATION must be true or false.' >&2; exit 2 ;;
esac
test -f "$root/docs/releases/$version.md"
"$root/scripts/bundle-macos.sh" --release --distribution
if [ "$signed" = true ]; then
    "$root/scripts/notarize-macos.sh" "$CARGO_TARGET_DIR/release/Trash Amp.app" \
        "$CARGO_TARGET_DIR/notarization/$version"
fi
dist="$CARGO_TARGET_DIR/dist"
mkdir -p "$dist"
stage=$(mktemp -d "$dist/stage.XXXXXX")
trap 'rm -rf "$stage"' EXIT HUP INT TERM
cp "$root/docs/releases/$version.md" "$stage/READ-ME.md"
ditto "$CARGO_TARGET_DIR/release/Trash Amp.app" "$stage/Trash Amp.app"
archive="trash-amp-$version-macos-arm64.zip"
ditto -c -k --sequesterRsrc "$stage" "$dist/$archive"
(cd "$dist" && shasum -a 256 "$archive" > "$archive.sha256")
printf '%s\n' "$dist/$archive" "$dist/$archive.sha256"
