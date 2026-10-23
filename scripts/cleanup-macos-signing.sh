#!/bin/sh
set -eu
: "${RUNNER_TEMP:?GitHub runner temporary directory required}"
directory="$RUNNER_TEMP/macos-signing"
result=0
if [ -f "$directory/search-list.json" ]; then
    python3 - "$directory/search-list.json" <<'PY' || result=$?
import json, subprocess, sys
with open(sys.argv[1]) as file:
    original = json.load(file)
subprocess.run(['security', 'list-keychains', '-d', 'user', '-s', *original], check=True)
PY
fi
keychain="$directory/signing.keychain-db"
if [ -f "$keychain" ]; then
    security delete-keychain "$keychain" || result=$?
fi
rm -rf -- "$directory"
exit "$result"
