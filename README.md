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
  libasound2-dev libpulse-dev libfontconfig1-dev clang ninja-build
```

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

### CI artifacts are a build check, not portable binaries

The desktop host links **system ffmpeg**, so a binary is tied to the ffmpeg the build
machine had:

- **Linux** — the artifact wants the runner's soname (e.g. `libavutil.so.58`) and fails
  with `error while loading shared libraries` on a machine with a different ffmpeg.
- **macOS** — Homebrew ships per-OS bottles, so an artifact built on the `macos-13`
  runner carries `minos 13.0` and will not load on macOS 12. (GitHub retired the
  `macos-12` runners, so this cannot be matched in CI.) `MACOSX_DEPLOYMENT_TARGET`
  defaults to 12.0 for *our* code, but it cannot lower ffmpeg's floor.

**To run it, build it on the machine you want to run it on** — brew/apt then install a
matching ffmpeg. For an older macOS:

```bash
brew install ffmpeg pkg-config
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
