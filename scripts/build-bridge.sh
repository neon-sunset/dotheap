#!/bin/sh
set -eu
cd "$(dirname "$0")/.."

profile=${1:-release}
target_dir=${CARGO_TARGET_DIR:-target}
python3 scripts/generate-bridge-models.py
case "$profile" in
    release) cargo build --locked --release -p heap-bridge ;;
    debug) cargo build --locked -p heap-bridge ;;
    *) echo "usage: $0 [release|debug]" >&2; exit 2 ;;
esac

mkdir -p .build/rust/lib
cp "$target_dir/$profile/libheap_bridge.a" .build/rust/lib/
