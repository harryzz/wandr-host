# Sourced helper — exports everything the Android cross-build needs.
# Use: source "$(dirname "$0")/env-android.sh"
#
# Deliberately has NO machine-specific defaults: this repo is cloned by other people, and
# baking one developer's NDK path into it is exactly what made the crate un-buildable
# elsewhere before the split. Set ANDROID_NDK_HOME (or ANDROID_NDK_ROOT), or let the
# probe below find a standard install.
#
# Rationale for the pinned API level: NDK r27 ships only API-versioned
# `aarch64-linux-androidNN-clang` binaries (no unversioned `aarch64-linux-android-clang`).
# cc-rs (skia-bindings, zstd-sys, …) defaults to the unversioned name and fails to find a
# tool, so we point CC/CXX/AR and rustc's linker at the versioned driver explicitly.

if [[ -z "${ANDROID_NDK_HOME:-}" && -n "${ANDROID_NDK_ROOT:-}" ]]; then
    export ANDROID_NDK_HOME="$ANDROID_NDK_ROOT"
fi

if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
    for _c in "$HOME"/android-ndk-* "$HOME"/Android/Sdk/ndk/* \
              "$HOME"/Library/Android/sdk/ndk/* /usr/local/lib/android/sdk/ndk/*; do
        if [[ -d "$_c/toolchains/llvm/prebuilt" ]]; then
            export ANDROID_NDK_HOME="$_c"
            break
        fi
    done
    unset _c
fi

if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
    echo "env-android.sh: ANDROID_NDK_HOME is not set and no NDK was found." >&2
    echo "  install the NDK (r27+) and: export ANDROID_NDK_HOME=/path/to/android-ndk" >&2
    return 1 2>/dev/null || exit 1
fi
export ANDROID_NDK_ROOT="${ANDROID_NDK_ROOT:-$ANDROID_NDK_HOME}"

case "$(uname -s)" in
    Darwin) _WANDR_HOST_TAG=darwin-x86_64 ;;
    *)      _WANDR_HOST_TAG=linux-x86_64 ;;
esac
_WANDR_TC="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$_WANDR_HOST_TAG/bin"
_WANDR_API="${ANDROID_API_LEVEL:-30}"

export CC_aarch64_linux_android="${CC_aarch64_linux_android:-$_WANDR_TC/aarch64-linux-android${_WANDR_API}-clang}"
export CXX_aarch64_linux_android="${CXX_aarch64_linux_android:-$_WANDR_TC/aarch64-linux-android${_WANDR_API}-clang++}"
export AR_aarch64_linux_android="${AR_aarch64_linux_android:-$_WANDR_TC/llvm-ar}"

# rustc's linker for the target. NOT in .cargo/config.toml on purpose — that would need an
# absolute path and break every other machine. The versioned clang driver supplies its own
# sysroot and defaults to lld. Do NOT set *_RUSTFLAGS here: it would clobber the
# aes_armv8 / polyval_armv8 cfgs .cargo/config.toml sets for the ARMv8 crypto backends.
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="${CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER:-$_WANDR_TC/aarch64-linux-android${_WANDR_API}-clang}"

case ":$PATH:" in
    *":$_WANDR_TC:"*) ;;
    *) export PATH="$_WANDR_TC:$PATH" ;;
esac

unset _WANDR_TC _WANDR_API _WANDR_HOST_TAG

if [[ ! -x "$CC_aarch64_linux_android" ]]; then
    echo "env-android.sh: CC not executable: $CC_aarch64_linux_android" >&2
    echo "  hint: set ANDROID_API_LEVEL=NN to pick another versioned clang" >&2
    return 1 2>/dev/null || exit 1
fi
