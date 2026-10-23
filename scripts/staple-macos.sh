#!/bin/sh
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
validate_only=false
case "${1:-}" in --validate-only) validate_only=true; shift ;; esac
app=${1:?Usage: staple-macos.sh [--validate-only] APP}
"$root/scripts/verify-macos-signature.sh" "$app"
ticket_action() {
    attempt=1
    while true; do
        if xcrun stapler "$1" "$app"; then return; else status=$?; fi
        case "$status" in
            68|69|75) ;;
            *) exit "$status" ;;
        esac
        if [ "$attempt" -ge 3 ]; then
            echo 'Apple ticket service failed after three attempts; retain the signed app for retry.' >&2
            exit "$status"
        fi
        echo 'Apple ticket service is temporarily unavailable; retrying in 10 seconds.' >&2
        sleep 10
        attempt=$((attempt + 1))
    done
}
if [ "$validate_only" = false ]; then ticket_action staple; fi
ticket_action validate
spctl --assess --type execute --verbose=2 "$app"
