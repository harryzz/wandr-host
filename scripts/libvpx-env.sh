#!/usr/bin/env bash
# Shared: build the vendored static libvpx if needed, then export the variables
# env-libvpx-sys reads. SOURCE this (don't execute it) from a build-host-* script:
#
#   . "$(dirname "$0")/libvpx-env.sh"
#
# It exists so the env plumbing lives in ONE place. There are two copies of the
# build-host-* scripts (runtime/wandr-host/scripts/ and tools/scripts/, which have
# already drifted apart), and duplicating four exports across both would make that
# worse. Windows is not covered here — it consumes vcpkg's
# libvpx[core,realtime]:x64-windows-static-md (see build-host-windows.bat).
#
# Set VPX_SKIP_BUILD=1 to only export (assumes the lib is already built).

_vpx_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
_vpx_triple="${TRIPLE:-$(rustc -vV | awk '/^host:/{print $2}')}"
_vpx_out="$_vpx_root/target/libvpx/$_vpx_triple"

if [[ "${VPX_SKIP_BUILD:-0}" != "1" && ! -f "$_vpx_out/lib/libvpx.a" ]]; then
  echo "libvpx: not built for $_vpx_triple — building …"
  TRIPLE="$_vpx_triple" "$_vpx_root/scripts/build-libvpx.sh"
fi

export VPX_LIB_DIR="$_vpx_out/lib"
export VPX_INCLUDE_DIR="$_vpx_out/include"
export VPX_VERSION=1.16.0
export VPX_STATIC=1
echo "libvpx: static, $VPX_LIB_DIR"
