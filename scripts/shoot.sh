#!/bin/sh
# Opens a new viewer with the given -key value arguments and a window of 1200 x 780 points (SIZE=<width>x<height> sets
# another), waits for the window and the analysis, saves a window screenshot and quits that viewer. Viewers that were
# already open are left alone. The viewer is a copy of Dotheap.app with another bundle identifier, so its window frame
# and settings go to defaults of their own.
# usage: scripts/shoot.sh <out.png> -dump <core> [-section <name>] [...]
set -eu
out=$1; shift
size=${SIZE:-1200x780}
here=$(cd "$(dirname "$0")/.." && pwd)
WINLIST="$here/.build/winlist"
[ -x "$WINLIST" ] || swiftc -O "$here/scripts/winlist.swift" -o "$WINLIST"
app="$here/.build/Dotheap Screenshots.app"
rm -rf "$app"
ditto "$here/Dotheap.app" "$app"
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier io.github.neon-sunset.dotheap.screenshots" "$app/Contents/Info.plist"
codesign --force --sign - "$app" >/dev/null 2>&1
before=$(pgrep -x DotheapViewer || true)
open -n "$app" --args -ApplePersistenceIgnoreState YES -windowSize "$size" "$@"
deadline=$(( $(date +%s) + 60 ))
pid=""
while :; do
    [ -n "$pid" ] || pid=$(pgrep -x DotheapViewer | grep -v -x -F "${before:-none}" | head -1 || true)
    if [ -n "$pid" ]; then
        win=$("$WINLIST" "$pid" | awk '$7 > 500 && $5 > 700 { print $1; exit }')
        cpu=$(ps -o %cpu= -p "$pid" | tr -d ' ' | cut -d. -f1)
        if [ -n "$win" ] && [ "${cpu:-100}" -lt 5 ]; then break; fi
    fi
    [ "$(date +%s)" -lt "$deadline" ] || { echo "timed out"; [ -z "$pid" ] || kill "$pid"; exit 1; }
    /bin/sleep 0.5
done
screencapture -x -o -l"$win" "$out" || { kill "$pid"; exit 1; }
kill "$pid"
echo "$out"
