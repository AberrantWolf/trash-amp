#!/bin/sh
# Import Apple Developer team credentials into an isolated runner keychain.
set -eu
umask 077
: "${RUNNER_TEMP:?GitHub runner temporary directory required}"
: "${GITHUB_ENV:?GitHub environment file required}"
: "${MACOS_CERT_P12_BASE64:?Missing MACOS_CERT_P12_BASE64}"
: "${MACOS_CERT_PASSWORD:?Missing MACOS_CERT_PASSWORD}"
: "${MACOS_KEYCHAIN_PASSWORD:?Missing MACOS_KEYCHAIN_PASSWORD}"
: "${MACOS_NOTARY_APPLE_ID:?Missing MACOS_NOTARY_APPLE_ID}"
: "${MACOS_NOTARY_PASSWORD:?Missing MACOS_NOTARY_PASSWORD}"
: "${MACOS_NOTARY_TEAM_ID:?Missing MACOS_NOTARY_TEAM_ID}"
directory="$RUNNER_TEMP/macos-signing"
mkdir "$directory"
certificate="$directory/certificate.p12"
keychain="$directory/signing.keychain-db"
trap 'rm -f "$certificate"' EXIT HUP INT TERM
python3 - "$certificate" <<'PY'
import base64, os, sys
from pathlib import Path
Path(sys.argv[1]).write_bytes(base64.b64decode(
    ''.join(os.environ['MACOS_CERT_P12_BASE64'].split()), validate=True
))
PY
security create-keychain -p "$MACOS_KEYCHAIN_PASSWORD" "$keychain"
security unlock-keychain -p "$MACOS_KEYCHAIN_PASSWORD" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security import "$certificate" -k "$keychain" -P "$MACOS_CERT_PASSWORD" \
    -T /usr/bin/codesign -T /usr/bin/security
security set-key-partition-list -S apple-tool:,apple:,codesign: \
    -s -k "$MACOS_KEYCHAIN_PASSWORD" "$keychain" >/dev/null
identity=$(security find-identity -v -p codesigning "$keychain" \
    | sed -n 's/.*"\(Developer ID Application:.*\)"/\1/p' | head -n 1)
case "$identity" in
    *"($MACOS_NOTARY_TEAM_ID)") ;;
    *) echo 'No Developer ID Application identity matching the notarization team.' >&2; exit 1 ;;
esac
xcrun notarytool store-credentials developer-id-notary --keychain "$keychain" \
    --apple-id "$MACOS_NOTARY_APPLE_ID" --password "$MACOS_NOTARY_PASSWORD" \
    --team-id "$MACOS_NOTARY_TEAM_ID"
{
    printf 'MACOS_SIGN_IDENTITY=%s\n' "$identity"
    printf 'MACOS_SIGNING_KEYCHAIN=%s\n' "$keychain"
    printf 'MACOS_SIGNING_TEAM_ID=%s\n' "$MACOS_NOTARY_TEAM_ID"
    printf 'MACOS_NOTARY_PROFILE=developer-id-notary\n'
} >> "$GITHUB_ENV"
