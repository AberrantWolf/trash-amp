#!/bin/sh
# Install a built .deb on this machine and check that the app starts.
# Needs root (sudo) for apt, and xvfb-run when no display is available.
set -eu
deb=${1:?Usage: verify-linux-package.sh PACKAGE.deb}
case "$deb" in /*) ;; *) deb=$PWD/$deb ;; esac

# apt resolves the declared dependencies, so a missing or misnamed one fails here.
sudo apt-get install -y "$deb"
for path in \
    /usr/bin/trash-amp \
    /usr/share/applications/org.aberrantwolf.TrashAmp.desktop \
    /usr/share/metainfo/org.aberrantwolf.TrashAmp.metainfo.xml \
    /usr/share/icons/hicolor/256x256/apps/org.aberrantwolf.TrashAmp.png \
    /usr/share/doc/trash-amp/THIRD_PARTY_LICENSES.html; do
    test -s "$path" || { echo "Missing from package: $path" >&2; exit 1; }
done

# Start the installed app with generated demo albums. Still running after
# 15 seconds (timeout's exit code 124) means it started without crashing.
data=$(mktemp -d)
log=$data/trash-amp.log
run=""
if [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then run="xvfb-run -a"; fi
status=0
TRASH_AMP_DATA_DIR=$data $run timeout 15 trash-amp --demo >"$log" 2>&1 || status=$?
if [ "$status" -ne 124 ]; then
    echo "trash-amp exited early with status $status:" >&2
    cat "$log" >&2
    exit 1
fi
echo "Installed $(dpkg-query -W -f='${Package} ${Version}' trash-amp) and started it."
