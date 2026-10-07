#!/bin/sh
# Regenerate the macOS multi-resolution icon from the checked-in master PNG.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
icon_work=$(mktemp -d)
trap 'rm -rf "$icon_work"' EXIT HUP INT TERM
iconset="$icon_work/TrashAmp.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$root/assets/macos/trash-amp.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    retina=$((size * 2))
    sips -z "$retina" "$retina" "$root/assets/macos/trash-amp.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$root/assets/macos/TrashAmp.icns"
