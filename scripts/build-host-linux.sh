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

# The one remaining codec toolchain: libvpx (BSD-3, VP8/VP9 ENCODE — Signal video
# calls) is compiled from vendor/libvpx by wandr-vpx-sys → needs nasm on PATH. All
# DECODE now goes through GStreamer (below); the hand-written decoders
# (openh264/libde265/dav1d/vaapi) are retired and no longer built.

# GStreamer decode backend (gstreamer-hw / gstreamer-sw) — the sole decode path. It
# links a SYSTEM library, so PROBE for it. GST=0 forces off; GST=1 forces on (build
# fails loudly if the dev packages are missing).
case "${GST:-auto}" in
  0) echo "GStreamer decode: disabled (GST=0)" ;;
  1) FEATURES+=(--features gstreamer); echo "GStreamer decode: forced ON (GST=1)" ;;
  *) if pkg-config --exists gstreamer-1.0 gstreamer-app-1.0 2>/dev/null; then
       FEATURES+=(--features gstreamer)
       echo "GStreamer decode: ENABLED (gstreamer $(pkg-config --modversion gstreamer-1.0) found)"
     else
       echo "GStreamer decode: skipped (no gstreamer-1.0 — install libgstreamer1.0-dev + libgstreamer-plugins-base1.0-dev to enable)"
     fi ;;
esac

echo "Building wandr-host for $TARGET (release${P3:+, p3-async=$P3}) …"
cargo build --release --target "$TARGET" "${FEATURES[@]}"

OUT="$REPO_ROOT/target/$TARGET/release/wasm-android-host"
echo "Built: $(du -sh "$OUT" | cut -f1)  $OUT"
