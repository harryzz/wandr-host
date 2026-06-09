# wandr-host

The Rust + wasmtime + skia-safe host that **replaces Android's ART runtime**
with a WebAssembly Component runtime. It loads a Kotlin/Compose application
compiled to WASM (the `.cwasm` file produced by [wandr-app](../wandr-app/))
and renders it on real GPU hardware via Skia + EGL.

## What this app is for

The full design goal of the `wandr` project is:

> *Run Kotlin/Compose apps on Android **without ART** — compile them to WASM
> components and execute them under wasmtime, rendering with native Skia.*

`wandr-host` is the part that runs **on** the Android device:

- A `cdylib` (`libwasm_android_host.so`) packaged into an APK with a
  `NativeActivity` entry point — no Java / no ART for app logic.
- Boots wasmtime, loads the precompiled `.cwasm` from app-private storage,
  instantiates the WASM component, and dispatches WIT-imported calls to
  native implementations.
- Owns the EGL context, the Skia `Surface`, the winit event loop, and all
  Android-side state. The WASM guest only talks to the host via WIT
  interfaces.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Android device (Pixel 2 XL, Android 15 / API 35)           │
│                                                             │
│  ┌──────────────── wandr-host APK ───────────────────────┐   │
│  │  NativeActivity → android_main (wandr-host cdylib)    │   │
│  │     │                                                │   │
│  │     ├─ winit event loop  (window, input, lifecycle)  │   │
│  │     ├─ EGL context       (egl.rs)                    │   │
│  │     ├─ skia-safe GPU canvas                          │   │
│  │     │                                                │   │
│  │     └─ wasmtime runtime                              │   │
│  │           │                                          │   │
│  │           ├─ loads skiko-component.cwasm             │   │
│  │           │     (AOT-compiled for aarch64-android)   │   │
│  │           │                                          │   │
│  │           └─ implements WIT imports:                 │   │
│  │                canvas_impl.rs                        │   │
│  │                paragraph_impl.rs                     │   │
│  │                window_impl.rs                        │   │
│  │                scheduler_impl.rs                     │   │
│  │                input.rs                              │   │
│  │                lifecycle_impl.rs                     │   │
│  │                clipboard_impl.rs                     │   │
│  │                haptics_impl.rs                       │   │
│  │                pointer_icon_impl.rs                  │   │
│  │                text_segmentation_impl.rs             │   │
│  │                locale_impl.rs                        │   │
│  │                                                      │   │
│  └──────────────────────────────────────────────────────┘   │
│           │                              │                  │
│           ▼ EGL/GL                       ▼ Bionic libc      │
│       GPU surface                    Android NDK            │
└─────────────────────────────────────────────────────────────┘
```

## Source layout

| File | Role |
|------|------|
| `src/lib.rs` | Entry point — registers `android_main`, owns winit `App`, sets up wasmtime `Engine` + `Store` + `Linker` |
| `src/main.rs` | Desktop binary stub (Android uses `android_main` in the cdylib) |
| `src/egl.rs` | EGL context creation from the `ANativeWindow`, skia-safe `Surface` setup |
| `src/canvas_impl.rs` | Skia drawing primitives — `draw_rect`, `draw_path`, `make_paint`, text blob registry, host-side `WasiDrawable` (the C++ shim in `cpp/wasi_drawable.cpp`) |
| `src/paragraph_impl.rs` | Skia paragraph layout — `getRectsForRange` / `getGlyphPositionAtCoordinate` / `getWordBoundary` |
| `src/window_impl.rs` | Window metrics (size, scale, insets) exposed via WIT |
| `src/scheduler_impl.rs` | Host-side frame scheduling, `withFrameNanos` driver, delay() |
| `src/input.rs` | winit `WindowEvent` → WIT exports (`on-pointer-event`, `on-key-event-v2`) |
| `src/lifecycle_impl.rs` | Activity lifecycle proxy events (focused/unfocused → Started/Stopped) |
| `src/clipboard_impl.rs` | Cut/copy/paste plumbing |
| `src/haptics_impl.rs` | Haptic feedback stubs |
| `src/pointer_icon_impl.rs` | Cursor icon changes |
| `src/text_segmentation_impl.rs` | ICU grapheme/word/line break for cursor stepping |
| `src/locale_impl.rs` | Locale provider |
| `src/bionic_compat.rs` | NDK linker shims — symbol redirects for older libc functions Skia/wasmtime expect |
| `cpp/wasi_drawable.cpp` | `SkDrawable` subclass with a *mutable* `sk_sp<SkPicture>` so child layers swap pictures without invalidating parent recordings |
| `cpp/wasi_drawable.h` | C ABI for the C++ shim |
| `build.rs` | Builds `wasi_drawable.cpp` against vendored Skia headers, links against skia-bindings' static lib |
| `assets/skiko-component.cwasm` | The AOT-compiled WASM component (built from [wandr-app](../wandr-app/)). Embedded into the APK; also overridable from app-private external storage |
| `vendor/skia-src/` | Vendored Skia headers (at the skia-bindings 0.93.1 commit) for the C++ shim's `#include`s |
| `.cargo/config.toml` | Cross-compile config — sets aarch64-linux-android target, NDK linker, sysroot |

## How a frame happens

1. winit delivers `WindowEvent::RedrawRequested` to `App::window_event`.
2. `App` calls the WIT export `render-frame(width, height, scale)` on the guest.
3. Guest's Compose runtime traverses its layer tree, building up `WasiDrawable`
   pictures.
4. For each draw call (rect/path/text/image/...), the guest invokes a WIT
   import like `canvas.draw-rect(x, y, w, h, paint-attrs)`.
5. The host implementation in `canvas_impl.rs` translates `paint-attrs` to a
   `skia_safe::Paint`, calls `canvas.draw_rect(rect, &paint)`, and the GPU does
   the actual rasterization.
6. Frame ends, EGL swaps buffers.

## Where the `.cwasm` comes from

`assets/skiko-component.cwasm` is the **default** component embedded at APK
build time. To override on a running device (no APK rebuild):

```bash
adb push /tmp/wandr-aot/skiko-component.cwasm \
    /sdcard/Android/data/com.example.wasmruntime/files/skiko-component.cwasm
```

The host checks app-private external storage first; if a `.cwasm` is there it
wins over the embedded asset. Hot-reload cycle is ~5 seconds.

See `wandr-app/BUILD.md` for how to produce `skiko-component.cwasm` from the
Kotlin/Compose source.

## Build & install

See [BUILD.md](BUILD.md) for the full toolchain setup, NDK env vars, target API
levels, and the `cargo apk build` + `adb install` cycle.
