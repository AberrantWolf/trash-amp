#!/bin/sh
set -eu
app=${1:?Usage: verify-macos-signature.sh APP}
codesign --verify --strict "$app"
details=$(codesign --display --verbose=4 "$app" 2>&1)
printf '%s\n' "$details" | grep -q '^Authority=Developer ID Application: '
printf '%s\n' "$details" | grep -q 'flags=.*runtime'
printf '%s\n' "$details" | grep -q '^Timestamp='
printf '%s\n' "$details" | grep -Eq '^TeamIdentifier=[A-Z0-9]+$'
if [ -n "${MACOS_SIGNING_TEAM_ID:-}" ]; then
    printf '%s\n' "$details" | grep -Fxq "TeamIdentifier=$MACOS_SIGNING_TEAM_ID"
fi
printf 'Developer ID signature, Hardened Runtime and timestamp verified.\n'
