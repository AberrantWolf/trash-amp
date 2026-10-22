#!/bin/sh
set -eu
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
test -s "$app/Contents/Resources/licenses/native-licenses/skia-0.99.0/SOURCES.txt"
test "$(lipo -archs "$binary")" = arm64
plutil -lint "$app/Contents/Info.plist"
codesign --verify --strict "$app"
python3 - "$app" <<'PY'
import plistlib, re, sys
from pathlib import Path
app = Path(sys.argv[1])
info = plistlib.loads((app / 'Contents/Info.plist').read_bytes())
assert re.fullmatch(r'\d+\.\d+\.\d+', info['CFBundleShortVersionString'])
assert re.fullmatch(r'\d+(\.\d+){0,2}', info['CFBundleVersion'])
assert info['TrashAmpVersion'].startswith(info['CFBundleShortVersionString'] + '-alpha.')
assert info['LSMinimumSystemVersion'] == '15.0'
notices = (app / 'Contents/Resources/licenses/THIRD_PARTY_LICENSES.html').read_text()
assert 'Slint Royalty-free Desktop, Mobile, and Web Applications License' in notices
print('Alpha metadata and notices verified.')
PY
# Shipping binaries must not depend on Homebrew or a developer's checkout.
otool -L "$binary" | tail -n +2 | while IFS= read -r line; do
    case "$line" in
        *'/System/Library/'*|*'/usr/lib/'*) ;;
        *) echo "Unexpected dynamic dependency: $line" >&2; exit 1 ;;
    esac
done
"$binary" --help
