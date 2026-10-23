#!/bin/sh
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
app=${1:?Usage: sign-macos.sh APP}
identity=${MACOS_SIGN_IDENTITY:--}
if [ "$identity" = - ]; then
    codesign --force --sign - "$app"
    codesign --verify --strict "$app"
    exit
fi
case "$identity" in
    'Developer ID Application: '*) ;;
    *) echo 'MACOS_SIGN_IDENTITY must name a Developer ID Application certificate.' >&2; exit 1 ;;
esac
set -- --force --options runtime --timestamp --sign "$identity"
if [ -n "${MACOS_SIGNING_KEYCHAIN:-}" ]; then
    set -- "$@" --keychain "$MACOS_SIGNING_KEYCHAIN"
fi
# No JIT or unsigned executable-memory exceptions are granted by default.
codesign "$@" "$app/Contents/MacOS/trash-amp"
codesign "$@" "$app"
"$root/scripts/verify-macos-signature.sh" "$app"
