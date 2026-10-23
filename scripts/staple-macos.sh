#!/bin/sh
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
app=${1:?Usage: staple-macos.sh APP}
"$root/scripts/verify-macos-signature.sh" "$app"
attempt=1
while true; do
    if xcrun stapler staple "$app"; then break; else status=$?; fi
    case "$status" in
        68|69|75) ;;
        *) exit "$status" ;;
    esac
    if [ "$attempt" -ge 3 ]; then
        echo 'Ticket delivery failed after three attempts; retain the signed app for retry.' >&2
        exit "$status"
    fi
    echo 'Apple ticket delivery is temporarily unavailable; retrying in 10 seconds.' >&2
    sleep 10
    attempt=$((attempt + 1))
done
xcrun stapler validate "$app"
spctl --assess --type execute --verbose=2 "$app"
