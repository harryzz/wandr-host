# wandr-host — Build & Deploy

Cross-compile the Rust host to `aarch64-linux-android`, wrap it in an APK,
embed the WASM component, install on device.

> **TL;DR for a day-to-day rebuild:**
> ```bash
> cd ~/wandr/wandr-host && \
>     ANDROID_HOME=~/android-sdk \
>     ANDROID_NDK_HOME=~/android-ndk-r27d \
>     CARGO_BUILD_JOBS=2 \
>     cargo apk build --release
> adb install -r target/release/apk/wasm_android_host.apk
> ```
> After the first cold build (~11 min for skia-bindings), incremental
> rebuilds are <1 min if you don't touch deps.

---

## 0. Prerequisites

| Tool | Path / note |
|------|-------------|
| Rust toolchain | stable, with `aarch64-linux-android` target installed: `rustup target add aarch64-linux-android` |
| `cargo-apk` | `cargo install cargo-apk` — installs to `~/.cargo/bin/cargo-apk` |
| Android NDK r27d | `~/android-ndk-r27d` (or set `ANDROID_NDK_HOME`) |
| Android SDK | `~/android-sdk` with `build-tools/` and `platform-tools/` (or set `ANDROID_HOME`) |
| `adb` | from `~/android-sdk/platform-tools/` |
| debug keystore | `~/.android/debug.keystore` (auto-created by `adb` or `keytool`) |
| Connected device | aarch64, **Android 10+ (API 29+)** — see §1 for API level details |

---

## 1. Target Android version

The minimum and target API levels are declared in `Cargo.toml`:

```toml
[package.metadata.android.sdk]
min_sdk_version    = 29        # Android 10 (2019) — what the APK will install on
target_sdk_version = 34        # Android 14 (2023) — what the app was tested against
```

### API level → Android version

| API | Android | Year |
|-----|---------|------|
| 29 | Android 10 | 2019 |
| 30 | Android 11 | 2020 |
| 31/32 | Android 12 | 2021 |
| 33 | Android 13 | 2022 |
| 34 | Android 14 | 2023 |
| **35** | **Android 15** | **2024** |
| 36 | Android 16 | 2025 |

**Currently configured: min API 29 / target API 34.** The APK installs on
Android 10 and newer. To bump:

- Edit `[package.metadata.android.sdk]` in `Cargo.toml` (`min_sdk_version`,
  `target_sdk_version`).
- Edit `.cargo/config.toml`'s `linker = .../aarch64-linux-android<API>-clang`
  to match the new `min_sdk_version` — **the linker API level must match
  `min_sdk_version`**, otherwise `cargo build` and `cargo apk build` use
  different toolchains, cargo caches twice, and you pay 11 min of skia
  recompile every time you switch tools.

### Verify your device's API level

```bash
adb shell getprop ro.build.version.sdk      # → 35
adb shell getprop ro.build.version.release  # → 15
adb shell getprop ro.product.model          # → Pixel 2 XL
```

---

## 2. Embed the `.cwasm` component

The Rust host expects a WASM component called `skiko-component.cwasm` at
either:

- `assets/skiko-component.cwasm` (embedded into the APK at build time), or
- `/sdcard/Android/data/com.example.wasmruntime/files/skiko-component.cwasm`
  (overrides the embedded one at runtime — used for hot-reload).

### Produce the .cwasm

Follow [`~/wandr/wandr-app/BUILD.md`](../wandr-app/BUILD.md) — that's the Kotlin
→ WASM Component → AOT pipeline. End artifact:

```
/tmp/wandr-aot/skiko-component.cwasm   (~63 MB)
```

### Copy into the host's assets

To make the next `cargo apk build` ship a new default component baked into
the APK:

```bash
cp /tmp/wandr-aot/skiko-component.cwasm ~/wandr/wandr-host/assets/skiko-component.cwasm
```

`Cargo.toml` declares:

```toml
[package.metadata.android]
assets = "assets"
```

so cargo-apk picks up everything in that dir and packs it into
`assets/` inside the APK.

### Hot-reload without rebuilding the APK

Push a new `.cwasm` to app-private storage and restart the activity:

```bash
adb push /tmp/wandr-aot/skiko-component.cwasm \
    /sdcard/Android/data/com.example.wasmruntime/files/skiko-component.cwasm
adb shell am force-stop com.example.wasmruntime
adb shell am start -n com.example.wasmruntime/android.app.NativeActivity
```

> Do **not** push to `/sdcard/Download/` — scoped storage blocks reads of
> Download/ from non-MediaStore apps. App-private external storage (the path
> above) needs no permission and is the right place.

---

## 3. Build the APK

```bash
cd ~/wandr/wandr-host
NDK=~/android-ndk-r27d/toolchains/llvm/prebuilt/linux-x86_64
ANDROID_HOME=~/android-sdk \
ANDROID_NDK_HOME=~/android-ndk-r27d \
CC_aarch64_linux_android=$NDK/bin/aarch64-linux-android29-clang \
CXX_aarch64_linux_android=$NDK/bin/aarch64-linux-android29-clang++ \
AR_aarch64_linux_android=$NDK/bin/llvm-ar \
PATH="$NDK/bin:$PATH" \
CARGO_BUILD_JOBS=2 \
cargo apk build --release
```

Cold build: ~11 minutes (skia-bindings is the slow step, ~7 min by itself).
Incremental builds with no dep changes: <1 minute.

> **Use `CARGO_BUILD_JOBS=2`, not `cargo-apk -j2`.** The `-j` flag is not
> forwarded by cargo-apk. With high parallelism on a low-RAM machine the
> linker can OOM mid-build.

### Why all those CC_ / CXX_ env vars?

`build.rs` compiles a C++ shim (`cpp/wasi_drawable.cpp`) via the `cc` crate.
`cc-rs` reads `CC_<target>` / `CXX_<target>` / `AR_<target>` env vars, NOT
`.cargo/config.toml`. Without them, it tries `aarch64-linux-android-clang`
(unversioned) which doesn't exist in NDK r27 — only versioned names like
`aarch64-linux-android29-clang` ship.

### Why does the linker version need to match `min_sdk_version`?

cargo-apk passes `-Clink-arg=--target=aarch64-linux-android<min_sdk>` and
sets the linker accordingly. If `.cargo/config.toml` has a different version
of `aarch64-linux-android<N>-clang`, cargo treats it as a different
fingerprint and refuses to share cached `.rmeta` / `.rlib` files between
`cargo build` and `cargo apk build`. Result: each command does its own full
rebuild from scratch. Keep them in sync.

### Output

```
target/release/apk/wasm_android_host.apk            # ← signed, aligned APK (~32 MB)
target/release/apk/wasm_android_host-unaligned.apk  # ← intermediate
```

Contents:

```
AndroidManifest.xml
assets/skiko-component.cwasm                # ← embedded WASM component
lib/arm64-v8a/libwasm_android_host.so       # ← cdylib (~15 MB)
lib/arm64-v8a/libc++_shared.so              # ← NDK C++ stdlib (~31 MB)
```

The signing keystore comes from `Cargo.toml`:

```toml
[package.metadata.android.signing.release]
path     = "~/.android/debug.keystore"
keystore_password = "android"
alias    = "androiddebugkey"
key_password = "android"
```

---

## 4. Install on device

```bash
adb install -r ~/wandr/wandr-host/target/release/apk/wasm_android_host.apk
```

`-r` reinstalls keeping app data (the user's `.cwasm` override in
app-private storage survives).

---

## 5. Run

```bash
adb shell am start -n com.example.wasmruntime/android.app.NativeActivity
```

> Activity is `android.app.NativeActivity`, **not** `MainActivity` — cargo-apk
> generates a manifest pointing at NativeActivity automatically.

Tail logs:

```bash
adb logcat -c
adb shell am start -n com.example.wasmruntime/android.app.NativeActivity
adb logcat | grep -iE "wasm_android|wasmtime|FATAL|AndroidRuntime"
```

Healthy boot:

```
wasm_android_host: resumed (warm) — swapping renderer in existing store
wasmtime::runtime::vm..: FreeList::add_capacity(...): capacity growing ...
wasm_android_host::ca..: [wasm] tfstate text="..." sel=TextRange(...)
```

The `tfstate` line means input is flowing into the guest. To test hardware
keyboard:

```bash
adb shell input keyevent KEYCODE_A     # types 'a' into the focused TextField
```

---

## 6. Full end-to-end one-liner (after .cwasm is built)

```bash
cd ~/wandr/wandr-host && \
    cp /tmp/wandr-aot/skiko-component.cwasm assets/skiko-component.cwasm && \
    NDK=~/android-ndk-r27d/toolchains/llvm/prebuilt/linux-x86_64 \
    ANDROID_HOME=~/android-sdk \
    ANDROID_NDK_HOME=~/android-ndk-r27d \
    CC_aarch64_linux_android=$NDK/bin/aarch64-linux-android29-clang \
    CXX_aarch64_linux_android=$NDK/bin/aarch64-linux-android29-clang++ \
    AR_aarch64_linux_android=$NDK/bin/llvm-ar \
    PATH="$NDK/bin:$PATH" \
    CARGO_BUILD_JOBS=2 \
    cargo apk build --release && \
adb install -r target/release/apk/wasm_android_host.apk && \
adb shell am start -n com.example.wasmruntime/android.app.NativeActivity
```

---

## 7. Desktop build (for unit tests / quick iteration)

```bash
cd ~/wandr/wandr-host
cargo build --release --target x86_64-unknown-linux-gnu
```

> The default target in `.cargo/config.toml` is
> `aarch64-linux-android`. Pass `--target x86_64-unknown-linux-gnu` to
> override for desktop. The desktop build uses winit's wayland/x11 backend
> instead of `android-native-activity`.

Desktop can't load `.cwasm` files compiled for aarch64-android — produce a
desktop-targeted one with:

```bash
wasmtime compile --target x86_64-unknown-linux-gnu \
    --wasm component-model --wasm gc --wasm function-references --wasm exceptions \
    -o /tmp/wandr-aot/skiko-component.cwasm \
    /tmp/wandr-aot/skiko-component.wasm
```

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| `failed to find tool "aarch64-linux-android-clang"` | cc-rs needs versioned NDK clang | Export `CC_aarch64_linux_android=...android29-clang` (and `CXX_`, `AR_`) before running |
| `Android SDK is not found` | `ANDROID_HOME` not set | `export ANDROID_HOME=~/android-sdk` |
| `cargo build` and `cargo apk build` each take 11 min on cold cache | `.cargo/config.toml` linker version ≠ `min_sdk_version` | Make them match — keep the API level in sync, e.g. `aarch64-linux-android29-clang` + `min_sdk_version = 29` |
| APK installs but app crashes on launch | `.cwasm` mismatch — desktop AOT vs aarch64-android AOT | Recompile with `wasmtime compile --target aarch64-linux-android` |
| App stuck on splash, no logs | wrong activity name | Use `android.app.NativeActivity` |
| Skia bindings rebuilds every time | Path changes invalidate cache (e.g. `host` → `wandr-host` rename) | One-shot pain; subsequent builds in same dir cache normally |
| `cargo-apk -j2` not recognized | cargo-apk doesn't forward `-j` | Use `CARGO_BUILD_JOBS=2` env var instead |
| OOM during linker | Build parallelism too high | `CARGO_BUILD_JOBS=2` (or 1) |
| App installs but `adb shell input keyevent KEYCODE_A` does nothing | Focus is on a non-TextField | Tap a TextField first; check logcat for `tfstate` after the keystroke |
