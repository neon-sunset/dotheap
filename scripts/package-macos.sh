#!/bin/sh
set -eu
cd "$(dirname "$0")/.."

output=${1:-dist}
app=${2:-Dotheap.app}
cli=${3:-target/release/dotheap}
mkdir -p "$output"
output=$(cd "$output" && pwd)
package=$(mktemp -d)
trap 'rm -rf "$package"' EXIT HUP INT TERM

ditto "$app" "$package/Dotheap.app"
cp "$cli" "$package/dotheap"
cp LICENSE THIRD-PARTY-NOTICES.txt "$package/"

COPYFILE_DISABLE=1 tar -czf "$output/dotheap-macos-arm64.tar.gz" \
    -C "$package" LICENSE THIRD-PARTY-NOTICES.txt dotheap
ditto -c -k --keepParent "$package/Dotheap.app" "$output/Dotheap-macos-arm64.zip"
COPYFILE_DISABLE=1 tar -czf "$output/dotheap-macos-arm64-bundle.tar.gz" \
    -C "$package" Dotheap.app dotheap LICENSE THIRD-PARTY-NOTICES.txt

version=$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)
python3 scripts/check-macos-package.py "$output/dotheap-macos-arm64-bundle.tar.gz" "$version"
