# wandr-host

The **portable WASM UI runtime host** for [wandr](https://codeberg.org/harryzz/wandr) —
Rust + wasmtime (component model) + a skia-safe rendering core. It implements the
[`wandr-wit`](https://github.com/harryzz/wandr-wit) contracts and delegates only the
OS-specific bits to a per-platform backend.

A guest app — *any* language, *any* UI framework — is compiled once to a WASM component
against those contracts, and the same `.wasm` runs anywhere `wandr-host` has a backend.
Shipped guest frameworks: Compose Multiplatform, Slint, dioxus, Avalonia,
Swift/OpenSwiftUI.

| Platform | Status |
|---|---|
| **Android** (aarch64) | production backend — replaces ART end-to-end, real GPU via EGL + Skia, full `--no-art` native-service stack |
| **Linux** (x86_64) | desktop/dev backend — winit + glutin, JIT |
| **macOS** | desktop/dev backend |
| **Windows** | desktop/dev backend (MSVC) |

## Build

```bash
git clone --recurse-submodules https://github.com/harryzz/wandr-host
cd wandr-host
cargo build --release
```

`cargo build` targets the **host** platform — there is deliberately no default target in
`.cargo/config.toml`, so a fresh clone just works. If you cloned without submodules, a
desktop build needs only three:

```bash
git submodule update --init --depth 1 contracts crates/wandr-sensors-client vendor/skia-src
```

### Linux system packages

```bash
sudo apt-get install -y libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
  libwayland-dev libxkbcommon-dev libegl1-mesa-dev libgl1-mesa-dev \
  libasound2-dev libpulse-dev libfontconfig1-dev clang ninja-build nasm
```

`nasm` assembles libvpx's x86 SIMD — the video backend builds it from
`vendor/libvpx` on first `cargo build`. Without an assembler libvpx would silently
fall back to a pure-C build with badly degraded realtime encode, so the build fails
loudly instead. (No media *library* is needed: nothing links system ffmpeg any more.)

### Android (cross build)

Android additionally needs the NDK and the AOSP submodules, which `build.rs` reads to
generate AIDL bindings:

```bash
git submodule update --init --recursive        # ~2.3 GB (AOSP trees, shallow)
export ANDROID_NDK_HOME=/path/to/android-ndk
cargo install cargo-ndk
cargo ndk -t arm64-v8a -p 30 build --release
```

Convenience wrappers live in `scripts/` (`build-host-linux.sh`, `-macos.sh`,
`-android.sh`, `build-host-windows.bat`). CI builds every target — see
`.github/workflows/build.yml`.

### Artifact portability

Task 117 removed the FFmpeg dependency — video is now **libvpx, built from
`vendor/libvpx` and linked statically** — so the old "a binary is tied to the ffmpeg
the build machine had" caveat no longer applies on any platform. No `libav*.so`, no
`avcodec-*.dll`, no Homebrew bottle.

- **macOS — portable in practice.** The binary links **only system frameworks**
  (verified with `otool -L`: `libSystem`, `libc++`, AppKit/Metal/AVFoundation/…, and
  nothing else). `MACOSX_DEPLOYMENT_TARGET` (default 12.0, override with `MACOS_MIN`)
  now actually governs the floor, because nothing else pins a higher one.
  *Verified:* an x86_64 build reports `minos 12.0` and runs on macOS 12.7.6.
  *Not verified:* a CI-built **aarch64** artifact on an old macOS — plausible, but
  untested, and arm64 Macs never shipped below macOS 11 anyway.
- **Linux — still not portable, but for a different reason.** glibc and the X11 /
  Wayland / GL / audio system libraries remain, so an artifact built on a newer
  runner can still fail on an older distro. That is now the *only* blocker, and it is
  what task 118 (redistributable binaries) is about.
- **Windows** — no media DLL is needed at run time any more; nothing has to be put on
  `PATH` to launch the exe.

Building on the target machine is still the surest thing, and is required for
macOS x86_64 (CI only covers aarch64):

```bash
brew install nasm                               # libvpx's x86 SIMD assembler
ARCHS=x86_64 ./scripts/build-host-macos.sh      # MACOS_MIN=12.0 by default
```

For the deeper Android toolchain notes (API levels, `cargo apk`, device install), see
[BUILD.md](BUILD.md).

## Layout

| Path | Role |
|---|---|
| `src/lib.rs` | wasmtime `Engine`/`Store`/`Linker` setup, the WIT host implementations, winit `App` |
| `src/canvas_impl.rs`, `src/wasi_canvas_002_impl.rs` | Skia drawing — rects/paths/paragraphs/images, `wasi:canvas` host side |
| `src/egl.rs` | EGL context from the `ANativeWindow`, skia-safe `Surface` |
| `src/app_loader.rs` | installs/loads app packages, AOT `.cwasm` cache + cache-key validation |
| `src/input.rs`, `src/ime_impl.rs` | pointer/key events, IME |
| `src/sensors_impl.rs` | `wandr:device/sensors` — a thin adapter over `crates/wandr-sensors-client` |
| `src/standalone.rs`, `src/zygote.rs` | the `--no-art` / zygote-fork runtime paths |
| `src/bionic_compat.rs` | NDK linker shims for libc symbols Skia/wasmtime expect |
| `cpp/` | the C++ shims built by `build.rs` (`SkDrawable` with a swappable `SkPicture`, libgui/SurfaceFlinger) |
| `contracts/` | submodule → [wandr-wit](https://github.com/harryzz/wandr-wit) — the WIT contracts this host implements |
| `crates/wandr-sensors-client/` | submodule → [sensorservice client](https://github.com/harryzz/wandr-sensors-client) |
| `crates/rsbinder`, `crates/audioclient-rs` | submodules — Android binder / audio (Android-only) |
| `vendor/` | AOSP trees for AIDL codegen, plus `skia-src` headers for the C++ shims |
| `.cargo/config.toml` | ARMv8 crypto `--cfg` flags for the Android target. **No** forced target and **no** absolute NDK paths |

## How a frame happens

1. The backend delivers a redraw (winit `RedrawRequested`, or the standalone render loop).
2. The host calls the guest's `render-frame` WIT export.
3. The guest's UI framework traverses its layer tree and issues draw calls.
4. Each draw call arrives as a WIT import — e.g. `canvas.draw-rect(...)`.
5. The host translates it to `skia_safe` and the GPU rasterizes.
6. EGL/GL swaps buffers.

Rendering is on-demand: guests that export frame-pacing only redraw when dirty.

## Relationship to the wandr repo

[`wandr`](https://codeberg.org/harryzz/wandr) is the umbrella project — apps, guest
crates, the arbiter (window server / system coordinator), docs — and carries this repo as
a submodule at `runtime/wandr-host`. This repo is split out so the host can be cloned and
built on its own, per platform, without the monorepo.
