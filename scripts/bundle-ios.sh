#!/bin/bash
# Build the shared Slint UI as an iOS app. No Apple login is needed for simulator builds.
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
mode=simulator
profile=debug
xcode=false
for arg in "$@"; do
    case "$arg" in
        --simulator) mode=simulator ;;
        --device) mode=device ;;
        --release) profile=release ;;
        --xcode) xcode=true ;;
        --help) echo 'Usage: scripts/bundle-ios.sh [--simulator|--device] [--release]'; exit 0 ;;
        *) echo "Unknown argument: $arg" >&2; exit 2 ;;
    esac
done
if [ "$xcode" = true ]; then
    case "${PLATFORM_NAME:?Run --xcode from Xcode}" in
        iphonesimulator) mode=simulator ;;
        iphoneos) mode=device ;;
        *) echo "Unsupported Xcode platform: $PLATFORM_NAME" >&2; exit 2 ;;
    esac
    if [ "${CONFIGURATION:-Debug}" != Debug ]; then profile=release; fi
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${DERIVED_FILE_DIR:?}/cargo}"
fi
if [ "$mode" = simulator ]; then
    target=aarch64-apple-ios-sim
    sdk=iphonesimulator
else
    target=aarch64-apple-ios
    sdk=iphoneos
fi
export IPHONEOS_DEPLOYMENT_TARGET=15.0
export SDKROOT="$(xcrun --sdk "$sdk" --show-sdk-path)"
build_args=(build --locked -p trash-amp-ios --target "$target")
if [ "$profile" = release ]; then
    build_args+=(--release)
    export CARGO_PROFILE_RELEASE_DEBUG=1
fi
cargo "${build_args[@]}"
binary="${CARGO_TARGET_DIR:-$root/target}/$target/$profile/trash-amp-ios"
if [ "$xcode" = true ]; then
    mkdir -p "$TARGET_BUILD_DIR/$(dirname "$EXECUTABLE_PATH")"
    cp "$binary" "$TARGET_BUILD_DIR/$EXECUTABLE_PATH"
    if [ "$profile" = release ]; then
        xcrun dsymutil "$binary" -o "$DWARF_DSYM_FOLDER_PATH/$DWARF_DSYM_FILE_NAME"
    fi
    exit 0
fi
app="$root/target/$target/$profile/Trash Amp.app"
mkdir -p "$app"
cp "$binary" "$app/trash-amp-ios"
icon_info=$(mktemp "${TMPDIR:-/tmp}/trash-amp-icon.XXXXXX")
trap 'rm -f "$icon_info"' EXIT
xcrun actool "$root/assets/ios/Assets.xcassets" \
    --compile "$app" --platform "$sdk" --minimum-deployment-target 15.0 \
    --app-icon AppIcon --target-device iphone --target-device ipad \
    --output-partial-info-plist "$icon_info" --output-format human-readable-text
python3 - "$root" "$app" "$mode" "$icon_info" <<'PY'
from pathlib import Path
import plistlib
import sys
import tomllib
root, app = map(Path, sys.argv[1:3])
with (root / "assets/ios/Info.plist").open("rb") as source:
    info = plistlib.load(source)
with Path(sys.argv[4]).open("rb") as source:
    info.update(plistlib.load(source))
version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
info.update(
    CFBundleIdentifier="org.aberrantwolf.TrashAmp.iOS",
    CFBundleExecutable="trash-amp-ios",
    CFBundleShortVersionString=version.split("-")[0],
    CFBundleVersion="1",
    MinimumOSVersion="15.0",
    UIDeviceFamily=[1, 2],
    CFBundleSupportedPlatforms=["iPhoneSimulator" if sys.argv[3] == "simulator" else "iPhoneOS"],
)
with (app / "Info.plist").open("wb") as output:
    plistlib.dump(info, output)
PY
if [ "$mode" = simulator ]; then
    # Simulator entitlements are embedded by amp-ios/build.rs. Putting iOS
    # entitlements in this macOS ad-hoc signature prevents the binary launching.
    codesign --force --sign - "$app"
fi
printf '%s\n' "$app"
