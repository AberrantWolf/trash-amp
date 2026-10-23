#!/bin/sh
# Called only after signed package verification and downloaded-artifact checksum checks.
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tag=${1:?Usage: publish-prerelease.sh TAG ARTIFACT_DIRECTORY}
directory=${2:?Usage: publish-prerelease.sh TAG ARTIFACT_DIRECTORY}
case "$tag" in v*) ;; *) echo 'Release tags must begin with v.' >&2; exit 1 ;; esac
version=${tag#v}
python3 "$root/scripts/macos_version.py" "$version" >/dev/null
notes="$root/docs/releases/$version.md"
archive="$directory/trash-amp-$version-macos-arm64.zip"
test -s "$notes"
test -s "$archive"
test -s "$archive.sha256"
python3 - "$archive" <<'PY'
import hashlib
from pathlib import Path
import sys
archive = Path(sys.argv[1])
checksum = Path(str(archive) + '.sha256').read_text().split()
with archive.open('rb') as source:
    digest = hashlib.file_digest(source, 'sha256').hexdigest()
if checksum != [digest, archive.name]:
    raise SystemExit('Release archive checksum does not match; refusing to publish.')
PY
lookup_error=$(mktemp)
trap 'rm -f "$lookup_error"' EXIT HUP INT TERM
if draft=$(gh release view "$tag" --json isDraft --jq .isDraft 2>"$lookup_error"); then
    case "$draft" in
        false)
            echo "Release $tag is already published; leaving its notes and assets unchanged."
            exit 0
            ;;
        true) ;;
        *) echo 'Unexpected release lookup result; refusing to modify the release.' >&2; exit 1 ;;
    esac
    # An interrupted publication can leave a draft. Finish uploading before making it visible.
    gh release upload "$tag" "$archive" "$archive.sha256" --clobber
    gh release edit "$tag" --verify-tag --notes-file "$notes" \
        --draft=false --prerelease --latest=false
else
    # A service/authentication failure is not evidence that a release is absent.
    if ! python3 - "$lookup_error" <<'PY'
from pathlib import Path
import sys
sys.exit(0 if Path(sys.argv[1]).read_text().strip() == 'release not found' else 1)
PY
    then
        cat "$lookup_error" >&2
        exit 1
    fi
    # gh uploads assets before publishing; a failed upload leaves an unpublished draft.
    gh release create "$tag" "$archive" "$archive.sha256" --verify-tag \
        --prerelease --latest=false --title "Trash Amp $tag" --notes-file "$notes"
fi
