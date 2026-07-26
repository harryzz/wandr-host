# Building `wandr-host` on the desktop (Linux · Windows · macOS)

The desktop host builds with **one feature set on every OS**:

```
--features p3-async,gstreamer
```

- **`gstreamer`** = the one video **decode** backend (SW via `avdec_*`, HW via the
  VA / DXVA / VideoToolbox plugins), with per-OS zero-copy (dma-buf / D3D11 / IOSurface).
- **`libvpx`** = VP8/VP9 **encode** (Signal video calls). Always on — it's a non-optional
  dependency of the host, so it needs its toolchain on every platform (below).
- **`p3-async`** = the WASI 0.3 surface guests need; not in `default`, so always pass it.

The hand-written per-OS decoders (vaapi / d3d11 / videotoolbox / openh264 / libde265 /
oxideav / dav1d) were retired — GStreamer replaces all of them.

---

## 1. Clone

```bash
git clone https://github.com/harryzz/wandr-host.git
cd wandr-host
# Desktop submodules only (the AOSP vendor trees are Android-only and huge):
git submodule update --init --depth 1 \
  contracts crates/wandr-sensors-client crates/rsbinder crates/audioclient-rs \
  vendor/skia-src vendor/libvpx
```

You also need a Rust toolchain — install from <https://rustup.rs>.

---

## 2. Install dependencies

### Linux (Ubuntu / Debian)

```bash
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
  libwayland-dev libxkbcommon-dev libegl1-mesa-dev libgl1-mesa-dev \
  libasound2-dev libpulse-dev libfontconfig1-dev clang \
  pkg-config nasm \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav gstreamer1.0-vaapi
```

- `nasm` builds libvpx's x86 SIMD (from `vendor/libvpx`).
- The GStreamer packages give the dev headers + the decoders (`avdec_*` from `-libav`,
  HW from `-vaapi`).

### macOS

```bash
xcode-select --install                 # if you don't have the CLT
brew install nasm pkg-config gstreamer
```

`brew install gstreamer` pulls the full stack (base/good/bad/libav + `vtdec`), the dev
`.pc` files, and headers. `nasm` is only needed for an x86_64 build (arm64 uses NEON).

### Windows

You need **Visual Studio 2022** (Desktop C++ / MSVC), Rust (`x86_64-pc-windows-msvc`),
and two libraries — GStreamer and libvpx.

**GStreamer** (the MSVC build, ≥ 1.24):

```powershell
choco install gstreamer gstreamer-devel pkgconfiglite -y
```

This installs to `C:\gstreamer\1.0\msvc_x86_64` and sets `GSTREAMER_1_0_ROOT_MSVC_X86_64`.
(`pkgconfiglite` gives a working `pkg-config` so `gstreamer-sys` resolves.)

**libvpx** (via vcpkg — static lib + dynamic CRT, matching rustc-msvc):

```powershell
# from your vcpkg checkout, e.g. C:\Users\<you>\vcpkg
.\vcpkg install libvpx[core,realtime]:x64-windows-static-md
```

Then set these **once**, as persistent user env vars, so every build finds libvpx:

```powershell
$vpx = "C:\Users\<you>\vcpkg\installed\x64-windows-static-md"
[Environment]::SetEnvironmentVariable('VPX_LIB_DIR',     "$vpx\lib",     'User')
[Environment]::SetEnvironmentVariable('VPX_INCLUDE_DIR', "$vpx\include", 'User')
```

> `wandr-vpx-sys` uses `VPX_LIB_DIR` (mode 1) to link the prebuilt `vpx.lib`. Without it,
> it falls back to building `vendor/libvpx` from source, which on Windows needs the full
> POSIX/vcpkg dance — so the env var above is the supported Windows path.

---

## 3. Build

### Linux

```bash
scripts/build-host-linux.sh
# → target/x86_64-unknown-linux-gnu/release/wasm-android-host
```

The script probes for GStreamer (`GST=1` forces it on, `GST=0` off) and passes
`p3-async`. Equivalent by hand:

```bash
cargo build --release --features p3-async,gstreamer
```

### macOS

```bash
scripts/build-host-macos.sh          # builds x86_64 + aarch64 by default
# or a single arch:
ARCHS=x86_64 scripts/build-host-macos.sh
```

Equivalent by hand:

```bash
cargo build --release --features p3-async,gstreamer
```

### Windows

From a **Developer** shell (or after `call vcvars64.bat`), with the GStreamer +
`VPX_LIB_DIR` env from step 2 in place:

```bat
call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
set GST_ROOT=C:\gstreamer\1.0\msvc_x86_64
set PKG_CONFIG_PATH=%GST_ROOT%\lib\pkgconfig
set PATH=%GST_ROOT%\bin;%PATH%
cargo build --release --features p3-async,gstreamer
```

> ⚠️ Build from a **native Windows shell**, not a WSL shell. A `cmd`/`pwsh` launched
> from WSL starts in a `\\wsl.localhost\…` UNC path and does **not** inherit your
> Windows user env vars (GStreamer / `VPX_LIB_DIR`) — builds then fail to find libvpx.

The binary is `target\release\wasm-android-host.exe`. At runtime it needs the GStreamer
DLLs on `PATH` (`%GST_ROOT%\bin`) and its plugins (`GST_PLUGIN_SYSTEM_PATH_1_0` =
`%GST_ROOT%\lib\gstreamer-1.0`).

---

## 4. Sanity check the codec stack

Headless, no display needed — decodes a committed H.264 fixture through the GStreamer
backend (SW + HW lanes):

```bash
cd crates/wandr-video
cargo test --release --features gstreamer --test gstreamer_decode -- --nocapture
```

Expect `gstreamer-sw` and `gstreamer-hw` each decoding `250 frames at (320, 240)`
(the HW case uses your GPU's VA / DXVA / VideoToolbox decoder).
