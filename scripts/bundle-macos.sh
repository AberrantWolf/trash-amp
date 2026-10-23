#!/bin/sh
# Build a Finder/Dock bundle; use Developer ID when MACOS_SIGN_IDENTITY is set.
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
profile=release
launch=false
notices=
for arg in "$@"; do
    case "$arg" in
        --debug) profile=debug ;;
        --release) profile=release ;;
        --open) launch=true ;;
        --distribution) notices=distribution ;;
        *) echo "Usage: $0 [--debug|--release] [--open] [--distribution]" >&2; exit 2 ;;
    esac
done
if [ "$(uname -s)" != Darwin ]; then
    echo "macOS is required to build the app bundle." >&2
    exit 1
fi
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-15.0}"
case "$CARGO_TARGET_DIR" in /*) ;; *) CARGO_TARGET_DIR="$PWD/$CARGO_TARGET_DIR" ;; esac
version=$(cargo metadata --manifest-path "$root/Cargo.toml" --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "trash-amp"))')
bundle_version=$(python3 "$root/scripts/macos_version.py" "$version")
short_version=${bundle_version% *}
build_version=${bundle_version#* }
if [ "$profile" = release ]; then
    cargo build --manifest-path "$root/Cargo.toml" -p trash-amp --release --locked
else
    cargo build --manifest-path "$root/Cargo.toml" -p trash-amp --locked
fi
app="$CARGO_TARGET_DIR/$profile/Trash Amp.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$CARGO_TARGET_DIR/$profile/trash-amp" "$app/Contents/MacOS/trash-amp"
cp "$root/assets/macos/TrashAmp.icns" "$app/Contents/Resources/TrashAmp.icns"
if [ "$notices" = distribution ]; then
    python3 "$root/scripts/generate-notices.py" "$app/Contents/Resources/licenses"
fi
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>CFBundleName</key><string>Trash Amp</string>
    <key>CFBundleDisplayName</key><string>Trash Amp</string>
    <key>CFBundleIdentifier</key><string>org.aberrantwolf.trash-amp</string>
    <key>CFBundleExecutable</key><string>trash-amp</string>
    <key>CFBundleIconFile</key><string>TrashAmp</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$short_version</string>
    <key>CFBundleVersion</key><string>$build_version</string>
    <key>TrashAmpVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>$MACOSX_DEPLOYMENT_TARGET</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
EOF
plutil -lint "$app/Contents/Info.plist"
"$root/scripts/sign-macos.sh" "$app"
printf '%s\n' "$app"
if [ "$launch" = true ]; then open "$app"; fi
