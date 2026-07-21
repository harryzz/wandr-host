@echo off
REM Build wandr-host for the Windows desktop backend (release, p3-async).
REM
REM p3-async is ON: current guests (Signal, audio.player) import WASI 0.3
REM (wasi:sockets/tls@0.3). A plain build omits the p3 host impl and the guest
REM panics at instantiate with "resource implementation is missing".
REM
REM Task 117: ffmpeg is GONE. Video is libvpx (BSD-3) linked STATICALLY, so there
REM is no longer any DLL to put on PATH at run time.
REM
REM Prereqs (override by setting the env var before calling; defaults below):
REM   VCVARS        vcvarsall.bat for the MSVC x64 toolchain
REM   VCPKG_ROOT    vcpkg checkout. Install libvpx first:
REM                   vcpkg install libvpx[core,realtime]:x64-windows-static-md
REM                 The triplet matters: -static-md = static lib + DYNAMIC CRT
REM                 (/MD), matching rustc's x86_64-pc-windows-msvc. Plain -static
REM                 (/MT) gives LNK4098; plain x64-windows gives a vpx.dll and
REM                 reintroduces the runtime-DLL problem this task removed.
REM   LIBCLANG_PATH VS "C++ Clang tools" bin (wandr-vpx-sys bindgen needs libclang)
REM
REM Output: target\release\wasm-android-host.exe
setlocal
if "%VCVARS%"==""        set "VCVARS=C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Auxiliary\Build\vcvarsall.bat"
if "%VCPKG_ROOT%"==""    set "VCPKG_ROOT=C:\vcpkg"
if "%LIBCLANG_PATH%"=="" set "LIBCLANG_PATH=C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Tools\Llvm\x64\bin"

REM On Linux/macOS wandr-vpx-sys compiles vendor/libvpx itself. Windows can't run
REM libvpx's POSIX configure, so point it at vcpkg's prebuilt static lib instead.
set "VPX_ROOT=%VCPKG_ROOT%\installed\x64-windows-static-md"
if not exist "%VPX_ROOT%\lib\vpx.lib" (
  echo ERROR: libvpx not found at %VPX_ROOT%\lib\vpx.lib
  echo Run: vcpkg install libvpx[core,realtime]:x64-windows-static-md
  exit /b 1
)
set "VPX_LIB_DIR=%VPX_ROOT%\lib"
set "VPX_INCLUDE_DIR=%VPX_ROOT%\include"

REM dav1d (AV1) builds from source via meson, which is MSVC-native (no POSIX
REM configure), so it needs no vcpkg — just meson (pip install meson) + ninja +
REM nasm on PATH. Build it statically from source instead of resolving a system lib.
set "SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always"

call "%VCVARS%" x64 >nul
cd /d "%~dp0.."

echo === cargo build --release --features p3-async (windows) ===
cargo build --release --features p3-async
echo === DONE exit %ERRORLEVEL% ===
endlocal
