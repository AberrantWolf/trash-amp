#!/bin/sh
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
signed=${REQUIRE_MACOS_NOTARIZATION:-false}
case "${1:-}" in
    --signed) signed=true; shift ;;
esac
case "$signed" in true|false) ;; *) echo 'REQUIRE_MACOS_NOTARIZATION must be true or false.' >&2; exit 2 ;; esac
archive=${1:?Usage: verify-macos-package.sh ZIP}
case "$archive" in /*) ;; *) archive="$PWD/$archive" ;; esac
directory=$(dirname "$archive")
filename=$(basename "$archive")
(cd "$directory" && shasum -a 256 -c "$filename.sha256")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
ditto -x -k "$archive" "$work"
app="$work/Trash Amp.app"
binary="$app/Contents/MacOS/trash-amp"
test -x "$binary"
test -f "$work/READ-ME.md"
test -s "$app/Contents/Resources/licenses/THIRD_PARTY_LICENSES.html"
test -s "$app/Contents/Resources/licenses/SLINT-LICENSE.md"
test -s "$app/Contents/Resources/licenses/native-licenses/skia-0.153.3/SOURCES.txt"
test "$(lipo -archs "$binary")" = arm64
plutil -lint "$app/Contents/Info.plist"
codesign --verify --strict "$app"
if [ "$signed" = true ]; then
    "$root/scripts/staple-macos.sh" --validate-only "$app"
fi
PYTHONPATH="$root/scripts" python3 - "$app" "$filename" <<'PY'
import plistlib, sys
from pathlib import Path
from macos_version import bundle_versions
app = Path(sys.argv[1])
info = plistlib.loads((app / 'Contents/Info.plist').read_bytes())
version = info['TrashAmpVersion']
short_version, build_version = bundle_versions(version)
assert info['CFBundleShortVersionString'] == short_version
assert info['CFBundleVersion'] == build_version
assert sys.argv[2] == f'trash-amp-{version}-macos-arm64.zip'
assert info['LSMinimumSystemVersion'] == '15.0'
notices = (app / 'Contents/Resources/licenses/THIRD_PARTY_LICENSES.html').read_text()
assert 'Slint Royalty-free Desktop, Mobile, and Web Applications License' in notices
print(f'Prerelease {version} metadata and notices verified.')
PY
# Shipping binaries must not depend on Homebrew or a developer's checkout.
otool -L "$binary" | tail -n +2 | while IFS= read -r line; do
    case "$line" in
        *'/System/Library/'*|*'/usr/lib/'*) ;;
        *) echo "Unexpected dynamic dependency: $line" >&2; exit 1 ;;
    esac
done
"$binary" --help
