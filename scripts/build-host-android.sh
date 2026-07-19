#!/usr/bin/env bash
# Build wasm-android-host for aarch64-linux-android (release).
# Output: target/aarch64-linux-android/release/wasm-android-host
#
# Needs the NDK (see scripts/env-android.sh) and the AOSP vendor submodules, which
# build.rs reads to generate the AIDL bindings:
#   git submodule update --init --recursive
#
# Alternative that needs no env setup (what CI uses):
#   cargo install cargo-ndk
#   cargo ndk -t arm64-v8a --platform 30 build --release --features p3-async
#
# NOTE: wandr-arbiter is NOT built here — it lives in the wandr umbrella repo
# (codeberg.org/harryzz/wandr), not in this one.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=./env-android.sh
source "$REPO_ROOT/scripts/env-android.sh"

# p3-async (WASI 0.3) — ON by default, matching the desktop backends: current
# guests (Signal, audio.player) import wasi:sockets/tls@0.3 and fail to
# instantiate against a p2-only host ("resource implementation is missing").
# NOTE: enabling it flips `wasm_component_model_async(true)`, which changes the
# AOT precompile config hash → every device .cwasm must be rebuilt (task 115 M4).
# Set P3=0 to build the p2-only flavor (older cwasm-compatible).
FEATURES=()
[[ "${P3:-1}" == "1" ]] && FEATURES=(--features p3-async)

if [[ ! -d "$REPO_ROOT/vendor/aosp-frameworks-hardware-interfaces/sensorservice" ]]; then
    echo "error: the AOSP vendor submodules are not initialized (build.rs needs their AIDL)." >&2
    echo "  git -C \"$REPO_ROOT\" submodule update --init --recursive" >&2
    exit 1
fi

cd "$REPO_ROOT"
echo "Building wasm-android-host for aarch64-linux-android (release${P3:+, p3-async=$P3}) …"
cargo build --target aarch64-linux-android --release "${FEATURES[@]}"

OUT="$REPO_ROOT/target/aarch64-linux-android/release/wasm-android-host"
echo "Built: $(du -sh "$OUT" | cut -f1)  $OUT"
