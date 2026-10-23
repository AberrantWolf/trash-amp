#!/bin/sh
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
app=${1:?Usage: notarize-macos.sh APP LOG_DIRECTORY}
logs=${2:?Usage: notarize-macos.sh APP LOG_DIRECTORY}
: "${MACOS_NOTARY_PROFILE:?Set MACOS_NOTARY_PROFILE to a notarytool Keychain profile}"
"$root/scripts/verify-macos-signature.sh" "$app"
mkdir -p "$logs"
logs=$(CDPATH='' cd -- "$logs" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
ditto -c -k --sequesterRsrc --keepParent "$app" "$work/submission.zip"
set -- --keychain-profile "$MACOS_NOTARY_PROFILE"
if [ -n "${MACOS_SIGNING_KEYCHAIN:-}" ]; then
    set -- "$@" --keychain "$MACOS_SIGNING_KEYCHAIN"
fi
submitted=false
if xcrun notarytool submit "$work/submission.zip" "$@" \
    --wait --timeout 30m --output-format json > "$logs/submission.json"; then
    submitted=true
fi
if [ "$submitted" = true ] && python3 - "$logs/submission.json" <<'PY'
import json, sys
with open(sys.argv[1]) as file:
    result = json.load(file)
sys.exit(0 if result.get('status') == 'Accepted' else 1)
PY
then
    xcrun stapler staple "$app"
    xcrun stapler validate "$app"
    spctl --assess --type execute --verbose=2 "$app"
else
    submission=$(python3 - "$logs/submission.json" <<'PY'
import json, sys
try:
    with open(sys.argv[1]) as file:
        print(json.load(file).get('id', ''))
except (OSError, ValueError):
    pass
PY
)
    if [ -n "$submission" ]; then
        xcrun notarytool log "$submission" "$@" "$logs/notary-log.json" || true
    fi
    echo "Notarization was not accepted. Inspect $logs before retrying." >&2
    exit 1
fi
