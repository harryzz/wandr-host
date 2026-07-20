#!/usr/bin/env bash
# Build wandr-host for the Linux desktop backend — x86_64-unknown-linux-gnu, release.
#
# p3-async is ON by default: every current guest (Signal, audio.player) imports
# WASI 0.3 (wasi:sockets/tls@0.3). A plain build silently omits the p3 host impl
# and the guest panics at instantiate with:
#   "component imports instance `wasi:sockets/types@0.3.0` … resource implementation is missing"
# Set P3=0 to build the p2-only flavor.
#
# Output: target/x86_64-unknown-linux-gnu/release/wasm-android-host
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET=x86_64-unknown-linux-gnu
FEATURES=()
[[ "${P3:-1}" == "1" ]] && FEATURES=(--features p3-async)

cd "$REPO_ROOT"

# Task 117: static libvpx (BSD-3) replaced ffmpeg — builds it on first run.
. "$REPO_ROOT/scripts/libvpx-env.sh"

echo "Building wandr-host for $TARGET (release${P3:+, p3-async=$P3}) …"
cargo build --release --target "$TARGET" "${FEATURES[@]}"

OUT="$REPO_ROOT/target/$TARGET/release/wasm-android-host"
echo "Built: $(du -sh "$OUT" | cut -f1)  $OUT"
