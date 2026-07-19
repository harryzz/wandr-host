@echo off
REM Build wandr-host for the Windows desktop backend (release, p3-async).
REM
REM p3-async is ON: current guests (Signal, audio.player) import WASI 0.3
REM (wasi:sockets/tls@0.3). A plain build omits the p3 host impl and the guest
REM panics at instantiate with "resource implementation is missing".
REM
REM Prereqs (override by setting the env var before calling; defaults below):
REM   VCVARS        vcvarsall.bat for the MSVC x64 toolchain
REM   FFMPEG_DIR    BtbN ffmpeg *-shared prebuilt (bin -> PATH; lib/include for ffmpeg-sys-next)
REM   LIBCLANG_PATH VS "C++ Clang tools" bin (ffmpeg-sys-next bindgen needs libclang)
REM
REM Output: runtime\wandr-host\target\release\wasm-android-host.exe
REM Run the exe with FFMPEG_DIR\bin on PATH (the avcodec-*.dll are load-time deps).
setlocal
if "%VCVARS%"==""        set "VCVARS=C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Auxiliary\Build\vcvarsall.bat"
if "%FFMPEG_DIR%"==""    set "FFMPEG_DIR=C:\wandr-win\ff\ffmpeg-n8.1-latest-win64-gpl-shared-8.1"
if "%LIBCLANG_PATH%"=="" set "LIBCLANG_PATH=C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Tools\Llvm\x64\bin"

call "%VCVARS%" x64 >nul
set "PATH=%FFMPEG_DIR%\bin;%PATH%"
cd /d "%~dp0..\..\runtime\wandr-host"

echo === cargo build --release --features p3-async (windows) ===
cargo build --release --features p3-async
echo === DONE exit %ERRORLEVEL% ===
endlocal
