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

# Task 117: video codecs are statically linked, not ffmpeg. All three build from
# source on the first build, so there is nothing to install here — only the
# toolchains on PATH:
#   • libvpx  (BSD-3, VP8/VP9)  — wandr-vpx-sys compiles vendor/libvpx  → needs nasm
#   • libde265 (LGPL, H.265)    — libde265-sys `static` compiles with cc → needs a C compiler
#   • dav1d   (BSD-2, AV1)      — dav1d-sys builds it with meson/ninja   → needs meson, ninja, nasm
# SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always tells dav1d-sys to build the vendored
# dav1d statically instead of looking for a system .so via pkg-config.
export SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always

# Task 117 M2 stage 3: HW H.264 decode via VA-API. Unlike the codecs above this
# one links the SYSTEM libva, so it is enabled only when libva is actually
# installed — PROBED, never assumed, so a box without it still gets a working
# (software-decoding) host instead of a build failure. VAAPI=0 forces it off;
# VAAPI=1 forces it on and lets the build fail loudly if libva is missing.
case "${VAAPI:-auto}" in
  0) echo "VA-API HW decode: disabled (VAAPI=0)" ;;
  1) FEATURES+=(--features vaapi); echo "VA-API HW decode: forced ON (VAAPI=1)" ;;
  *) if pkg-config --exists libva libva-drm 2>/dev/null; then
       FEATURES+=(--features vaapi)
       echo "VA-API HW decode: ENABLED (libva $(pkg-config --modversion libva) found)"
     else
       echo "VA-API HW decode: skipped (no libva — install libva-dev to enable)"
     fi ;;
esac

echo "Building wandr-host for $TARGET (release${P3:+, p3-async=$P3}) …"
cargo build --release --target "$TARGET" "${FEATURES[@]}"

OUT="$REPO_ROOT/target/$TARGET/release/wasm-android-host"
echo "Built: $(du -sh "$OUT" | cut -f1)  $OUT"
