#!/usr/bin/env bash
# Build the vendored libvpx (vendor/libvpx, pinned v1.16.0) as a STATIC library —
# the replacement for the ffmpeg/libvpx dynamic dependency (task 117).
#
# WHY static+vendored: distro ffmpeg is built --enable-gpl (verified), which makes
# that build GPL while wandr is Apache-2.0; and linking system ffmpeg pins the
# binary to one soname. libvpx is BSD-3 and covers the entire codec surface we
# actually use (VP8/VP9 encode+decode). Colorspace/resize moved to pure-Rust
# crates (`yuv`, `fast_image_resize`), so libvpx is the only C dependency left.
#
# Output (consumed by env-libvpx-sys via VPX_LIB_DIR/VPX_INCLUDE_DIR — see
# scripts/build-host-linux.sh):
#   target/libvpx/<triple>/lib/libvpx.a
#   target/libvpx/<triple>/include/vpx/*.h
#
# Windows is NOT built here — it uses vcpkg
# (`vcpkg install libvpx[core,realtime]:x64-windows-static-md`), because vcpkg's
# port drives the configure -> vpx.sln -> msbuild dance and fetches NASM itself.
# See scripts/build-host-windows.bat.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$REPO_ROOT/vendor/libvpx"
TRIPLE="${TRIPLE:-$(rustc -vV | awk '/^host:/{print $2}')}"
OUT="$REPO_ROOT/target/libvpx/$TRIPLE"
BUILD="$OUT/build"

if [[ ! -f "$SRC/configure" ]]; then
  echo "error: $SRC/configure missing — run: git submodule update --init vendor/libvpx" >&2
  exit 1
fi

# x86 needs an assembler or configure silently produces a pure-C build, which
# guts realtime encode performance. Fail loudly instead. (arm64/NEON goes
# through the C compiler, so no external assembler is required there.)
AS_FLAG=()
case "$TRIPLE" in
  x86_64-*|i686-*)
    if   command -v nasm >/dev/null 2>&1; then AS_FLAG=(--as=nasm)
    elif command -v yasm >/dev/null 2>&1; then AS_FLAG=(--as=yasm)
    else
      echo "error: neither nasm nor yasm found; libvpx would fall back to a" >&2
      echo "       pure-C build with badly degraded realtime encode perf." >&2
      echo "       install one:  sudo apt install nasm" >&2
      exit 1
    fi
    ;;
esac

# --enable-pic          : rustc links a PIE; a non-PIC libvpx.a fails with
#                         "relocation R_X86_64_32S ... cannot be used when making a PIE object"
# --enable-realtime-only: drops the good/best deadline paths. Correct for a call-only
#                         backend, but it makes VPX_DL_GOOD_QUALITY behave as realtime —
#                         revisit if offline encoding is ever wanted.
# --disable-vp9-highbitdepth: halves the VP9 decoder; drops profile 2. WebRTC VP9 is
#                         profile 0 in practice. Pairs with the I420-only decode path.
# --disable-webm-io --disable-libyuv: container + scaling helpers we deliberately
#                         replaced with pure-Rust crates.
CONFIGURE_FLAGS=(
  --prefix="$OUT"
  --disable-shared --enable-static --enable-pic
  --disable-examples --disable-tools --disable-docs --disable-unit-tests
  --enable-vp8 --enable-vp8-encoder --enable-vp8-decoder
  --enable-vp9 --enable-vp9-encoder --enable-vp9-decoder
  --disable-webm-io --disable-libyuv --disable-postproc
  --enable-runtime-cpu-detect --enable-realtime-only --disable-vp9-highbitdepth
  "${AS_FLAG[@]}"
)

# libvpx supports out-of-tree builds; keep the submodule worktree pristine.
rm -rf "$BUILD"
mkdir -p "$BUILD"
cd "$BUILD"

echo "Configuring libvpx $(git -C "$SRC" describe --tags 2>/dev/null || echo '?') for $TRIPLE …"
"$SRC/configure" "${CONFIGURE_FLAGS[@]}"

echo "Building …"
make -j"$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"
make install

echo
echo "Built: $(du -sh "$OUT/lib/libvpx.a" | cut -f1)  $OUT/lib/libvpx.a"
echo
echo "Consume it with:"
echo "  export VPX_LIB_DIR=$OUT/lib"
echo "  export VPX_INCLUDE_DIR=$OUT/include"
echo "  export VPX_VERSION=1.16.0"
echo "  export VPX_STATIC=1"
