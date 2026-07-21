#!/usr/bin/env bash
# Build wandr-host for macOS desktop — Intel (x86_64) and/or Apple Silicon
# (aarch64), release, p3-async. Run on a Mac (needs Xcode + `brew install ffmpeg
# pkg-config`; skia-safe fetches its own prebuilts per target).
#
# IMPORTANT — cross-building is not proof it works:
#   From an Intel Mac you can build BOTH slices (the arm64 one is a cross-compile
#   — Apple's toolchain is universal, no extra linker/sysroot needed). But an
#   Intel Mac cannot RUN arm64 code. A passing arm64 build proves only that it
#   COMPILES + LINKS for Apple Silicon — NOT that it runs correctly there
#   (arm64's weak memory model vs x86 TSO, the Skia C++ shim / ffmpeg ABI,
#   CoreAudio/AVFoundation/Metal at runtime). Verify the arm64 binary on a real
#   Apple Silicon Mac (or a macos-14 CI runner) before trusting it — exactly like
#   we cross-build aarch64-linux-android but only trust it after running on-device.
#
# Targeting arm64 needs Xcode 12+ (⇒ macOS 11 Big Sur+); an older Mac stuck on
# Catalina/Mojave has no arm64 SDK and can only build x86_64.
#
# Usage:
#   build-host-macos.sh                 # both arches + a universal (fat) binary
#   ARCHS="aarch64" build-host-macos.sh # one arch only (x86_64 | aarch64)
#   UNIVERSAL=0 build-host-macos.sh     # skip the lipo universal binary
#   P3=0 build-host-macos.sh            # p2-only flavor (default is p3-async ON)
#
# Outputs:
#   target/<arch>-apple-darwin/release/wasm-android-host
#   target/wasm-android-host-universal   (when both built)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "build-host-macos.sh must run on macOS (uname=$(uname -s))." >&2
  exit 1
fi

# Task 117: ffmpeg is gone. The video codecs are compiled from source and linked
# statically — no Homebrew ffmpeg, no GPL exposure, no runtime .dylib:
#   • libvpx  (BSD-3, VP8/VP9) via wandr-vpx-sys — x86_64 slice needs `brew install nasm`
#   • libde265 (LGPL, H.265)   via libde265-sys `static` (cc)
#   • dav1d   (BSD-2, AV1)     via dav1d-sys internal meson build — `brew install meson ninja`
# SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always makes dav1d-sys build the vendored dav1d
# statically instead of resolving a system .dylib through pkg-config.
export SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always

FEATURES=()
[[ "${P3:-1}" == "1" ]] && FEATURES=(--features p3-async)

# Minimum macOS this binary claims to support. Applies to OUR code (rustc + the cc-rs C++
# shims); it does NOT change what the dependencies were built against. In particular a
# Homebrew ffmpeg is a per-OS bottle: one installed on macOS 13 carries `minos 13.0`, so a
# binary linked against it will not load on macOS 12 however this is set. That is why the
# CI artifacts are a BUILD CHECK — to RUN on an older macOS, build ON that macOS, where
# brew installs a matching bottle.
export MACOSX_DEPLOYMENT_TARGET="${MACOS_MIN:-12.0}"
echo "MACOSX_DEPLOYMENT_TARGET=$MACOSX_DEPLOYMENT_TARGET (override with MACOS_MIN=…)"

ARCHS="${ARCHS:-x86_64 aarch64}"
BUILT=()
for arch in $ARCHS; do
  target="${arch}-apple-darwin"
  rustup target add "$target" >/dev/null 2>&1 || true
  echo "Building wandr-host for $target (release${P3:+, p3-async=${P3}}) …"
  cargo build --release --target "$target" "${FEATURES[@]}"
  BUILT+=("target/$target/release/wasm-android-host")
done

# Universal (fat) binary when both slices were built.
if [[ "${UNIVERSAL:-1}" == "1" && ${#BUILT[@]} -eq 2 ]]; then
  OUT="target/wasm-android-host-universal"
  lipo -create "${BUILT[@]}" -output "$OUT"
  echo "Universal: $OUT"
  lipo -info "$OUT"
fi

for b in "${BUILT[@]}"; do
  echo "Built: $(du -sh "$b" | cut -f1)  $REPO_ROOT/$b"
done

echo
echo "NOTE: an arm64 build from an Intel Mac is COMPILE-verified only — run it on"
echo "      an Apple Silicon Mac to confirm it actually works."
