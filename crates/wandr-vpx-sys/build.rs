//! Build libvpx from the vendored source and generate bindings against it.
//!
//! Two modes:
//!   1. `VPX_LIB_DIR` set  → use that prebuilt libvpx (the Windows/vcpkg path, and
//!      an escape hatch for anyone who wants a system libvpx).
//!   2. otherwise          → configure+make `vendor/libvpx` into OUT_DIR.
//!
//! Mode 2 is what makes a plain `cargo build` self-contained — the whole point of
//! task 117 step 6. It costs ~1-2 min on a cold build and is then cached in
//! OUT_DIR like any other build artifact.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=ffi.h");
    println!("cargo:rerun-if-env-changed=VPX_LIB_DIR");
    println!("cargo:rerun-if-env-changed=VPX_INCLUDE_DIR");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let vendor = manifest.join("../../vendor/libvpx");

    let include_dir = match std::env::var_os("VPX_LIB_DIR") {
        // ── mode 1: prebuilt ────────────────────────────────────────────────
        Some(lib_dir) => {
            let lib_dir = PathBuf::from(lib_dir);
            println!("cargo:rustc-link-search=native={}", lib_dir.display());
            // vcpkg installs `vpx.lib` on Windows and `libvpx.a` elsewhere; cargo
            // wants the name without the lib prefix/extension either way.
            println!("cargo:rustc-link-lib=static=vpx");
            std::env::var_os("VPX_INCLUDE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| lib_dir.join("../include"))
        }
        // ── mode 2: build the vendored source ───────────────────────────────
        None => {
            let configure = vendor.join("configure");
            assert!(
                configure.is_file(),
                "vendored libvpx missing at {} — run:\n  \
                 git submodule update --init --depth 1 vendor/libvpx\n\
                 (or set VPX_LIB_DIR to a prebuilt libvpx)",
                vendor.display()
            );
            println!("cargo:rerun-if-changed={}", configure.display());
            let prefix = build_libvpx(&configure, &out_dir);
            println!("cargo:rustc-link-search=native={}", prefix.join("lib").display());
            println!("cargo:rustc-link-lib=static=vpx");
            prefix.join("include")
        }
    };

    let bindings = bindgen::builder()
        .header("ffi.h")
        .clang_arg(format!("-I{}", include_dir.display()))
        // MUST match what wandr-video's code expects: `vpx_codec_err_t::VPX_CODEC_OK`
        // rather than bare integer constants.
        .rustified_enum("^v.*")
        .allowlist_function("^vpx_.*")
        .allowlist_type("^(vpx|vp8|vp9|VPX).*")
        .allowlist_var("^(VPX|VP8|VP9).*")
        .layout_tests(false)
        .generate()
        .expect("bindgen: failed to generate libvpx bindings");
    bindings
        .write_to_file(out_dir.join("vpx_ffi.rs"))
        .expect("bindgen: failed to write vpx_ffi.rs");
}

/// configure + make the vendored libvpx into `<out_dir>/libvpx`, returning the
/// install prefix. Mirrors scripts/build-libvpx.sh — keep the flags in sync.
fn build_libvpx(configure: &Path, out_dir: &Path) -> PathBuf {
    let prefix = out_dir.join("libvpx");
    let build = out_dir.join("libvpx-build");

    // Already built (OUT_DIR survives across incremental builds).
    if prefix.join("lib/libvpx.a").is_file() {
        return prefix;
    }

    let target = std::env::var("TARGET").unwrap_or_default();
    // x86 needs an external assembler or configure silently produces a pure-C
    // build with badly degraded realtime encode performance. arm64/NEON goes
    // through the C compiler and needs none.
    let mut as_flag = None;
    if target.starts_with("x86_64") || target.starts_with("i686") {
        for asm in ["nasm", "yasm"] {
            if Command::new(asm).arg("--version").output().is_ok_and(|o| o.status.success()) {
                as_flag = Some(format!("--as={asm}"));
                break;
            }
        }
        assert!(
            as_flag.is_some(),
            "neither nasm nor yasm found. libvpx would fall back to a pure-C build \
             with badly degraded realtime encode performance.\n  install one: \
             sudo apt install nasm   (macOS: brew install nasm)"
        );
    }

    std::fs::create_dir_all(&build).expect("create libvpx build dir");

    let mut args = vec![
        format!("--prefix={}", prefix.display()),
        "--disable-shared".into(),
        "--enable-static".into(),
        "--enable-pic".into(), // rustc links a PIE; non-PIC .a fails to link
        "--disable-examples".into(),
        "--disable-tools".into(),
        "--disable-docs".into(),
        "--disable-unit-tests".into(),
        "--enable-vp8".into(),
        "--enable-vp8-encoder".into(),
        "--enable-vp8-decoder".into(),
        "--enable-vp9".into(),
        "--enable-vp9-encoder".into(),
        "--enable-vp9-decoder".into(),
        "--disable-webm-io".into(),
        "--disable-libyuv".into(),
        "--disable-postproc".into(),
        "--enable-runtime-cpu-detect".into(),
        // Call-only: drops the good/best deadline paths, so VPX_DL_GOOD_QUALITY
        // behaves as realtime. Revisit if offline encoding is ever wanted.
        "--enable-realtime-only".into(),
        // Drops VP9 profile 2; WebRTC VP9 is profile 0 in practice. Pairs with
        // the I420-only decode path in wandr-video.
        "--disable-vp9-highbitdepth".into(),
    ];
    args.extend(as_flag);

    run(Command::new(configure).args(&args).current_dir(&build), "libvpx configure");
    let jobs = std::env::var("NUM_JOBS").unwrap_or_else(|_| "4".into());
    run(Command::new("make").arg(format!("-j{jobs}")).current_dir(&build), "libvpx make");
    run(Command::new("make").arg("install").current_dir(&build), "libvpx make install");

    prefix
}

fn run(cmd: &mut Command, what: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("{what}: failed to spawn ({e}). Is `make` installed?"));
    assert!(status.success(), "{what} failed with {status}");
}
