#!/bin/sh
set -eu
cd "$(dirname "$0")/.."

if ! xcrun --find swift >/dev/null 2>&1; then
    echo "Install Xcode 16 or later and select it with xcode-select before installing Dotheap." >&2
    exit 1
fi

cargo build --release --locked -p dotheap
codesign --force --sign - --entitlements assets/Debugger.entitlements target/release/dotheap
codesign --verify --strict target/release/dotheap
scripts/make-app.sh --disable-sandbox
mkdir -p bin
cp target/release/dotheap bin/
