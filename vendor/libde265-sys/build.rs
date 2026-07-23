use const_format::formatcp;
use curl::easy::Easy;
use flate2::read::GzDecoder;
use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};
use tar::Archive;
use walkdir::WalkDir;

const LIBDE265_VERSION: &str = "1.0.15";
const LIBDE265_NAME: &str = formatcp!("libde265-{LIBDE265_VERSION}");
const LIBDE265_FILE_NAME: &str = formatcp!("{LIBDE265_NAME}.tar.gz");
const LIBDE265_URL: &str = formatcp!(
    "https://github.com/strukturag/libde265/releases/download/v{LIBDE265_VERSION}/{LIBDE265_FILE_NAME}",
);

fn download<P: AsRef<Path>>(source_url: &str, target_file: P) -> anyhow::Result<()> {
    let f = fs::File::create(&target_file)?;
    let mut writer = io::BufWriter::new(f);
    let mut easy = Easy::new();
    easy.useragent("Curl Download")?;
    easy.url(source_url)?;
    easy.follow_location(true)?;
    easy.write_function(move |data| Ok(writer.write(data).unwrap()))?;
    easy.perform()?;

    let response_code = easy.response_code()?;
    if response_code == 200 {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Unexpected response code {} for {}",
            response_code,
            source_url
        ))
    }
}

fn extract<P1: AsRef<Path>, P2: AsRef<Path>>(filename: P1, outpath: P2) -> anyhow::Result<()> {
    let file = fs::File::open(&filename)?;
    let tar = GzDecoder::new(file);
    let mut archive = Archive::new(tar);
    archive.unpack(outpath.as_ref())?;

    Ok(())
}

/// Finds all files with an extension, ignoring some.
fn glob_import<P: AsRef<Path>>(root: P, extenstion: &str, exclude: &[&str]) -> Vec<String> {
    WalkDir::new(root)
        .into_iter()
        .map(|x| x.unwrap())
        .filter(|x| x.path().to_str().unwrap().ends_with(extenstion))
        .map(|x| x.path().to_str().unwrap().to_string())
        .filter(|x| !exclude.iter().any(|e| x.contains(e)))
        .collect()
}

fn feature_enabled(feature: &str) -> bool {
    let env_var_name = format!("CARGO_FEATURE_{}", feature.replace('-', "_").to_uppercase());
    println!("cargo:rerun-if-env-changed={env_var_name}");
    env::var(env_var_name).is_ok()
}

fn compile_and_add_libde265_static_lib(root: &Path, libname: &str, encoder: bool) {
    // ── wandr patches (2026-07-23) ────────────────────────────────────────
    // Three fixes so the STATIC source build works on Windows and macOS-arm64,
    // not just Linux-x86. Each is root-caused from libde265 1.0.15's own source;
    // upstream's CMake/autotools already do the equivalent, but this crate's
    // hand-rolled cc build did not. See vendor/libde265-sys/WANDR-PATCHES.md.
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let is_x86 = target_arch == "x86" || target_arch == "x86_64";

    let libde265_src = root.join("libde265");
    let mut cc_build = cc::Build::new();
    let mut files = glob_import(
        &libde265_src,
        ".cc",
        if encoder {
            &[]
        } else {
            &["encoder", "en265.cc"]
        },
    );

    // FIX 1 — x86 SSE sources on non-x86. `libde265/x86/sse-*.cc` include
    // <emmintrin.h> and fail on aarch64 ("only meant for x86"). Their init
    // (`init_acceleration_functions_sse`) is called only `#ifdef HAVE_SSE4_1`,
    // which this build never defines — so on x86 they compile but are DEAD, and
    // dropping them everywhere-but-x86 is safe (the C fallback runs). Match the
    // `x86` PATH COMPONENT, not the substring, or the target triple
    // (x86_64-...) would wrongly exclude every file.
    if !is_x86 {
        let sep = std::path::MAIN_SEPARATOR;
        let marker = format!("{sep}x86{sep}");
        files.retain(|f| !f.contains(&marker));
    }

    cc_build
        .include(&root)
        .include(&libde265_src)
        .cpp(true)
        .warnings(false)
        .files(files)
        .pic(true);

    // FIX 2 — malloc.h. `image.cc` does `#ifdef HAVE_MALLOC_H #include <malloc.h>`;
    // macOS has no <malloc.h> (it's <malloc/malloc.h>). Every OTHER malloc.h in
    // libde265 is `#if defined(_MSC_VER)`-guarded, so this is the only offender.
    // Define it where the header exists (Linux, MSVC), not on macOS.
    if target_os != "macos" {
        cc_build.define("HAVE_MALLOC_H", "true");
    }

    // FIX 3 — MSVC dllimport. `de265.h` makes LIBDE265_API `__declspec(dllimport)`
    // under `_MSC_VER && !LIBDE265_STATIC_BUILD`, so compiling the DEFINITIONS in
    // de265.cc is C2491 ("definition of dllimport function not allowed"). Define
    // LIBDE265_STATIC_BUILD so the macro is empty — we are, in fact, static.
    if target_env == "msvc" {
        cc_build.define("LIBDE265_STATIC_BUILD", None);
    }

    // SSE/AVX compiler flags only make sense on x86 (and flag_if_supported would
    // skip them elsewhere anyway); keep them x86-only for clarity.
    if is_x86 {
        cc_build.flag_if_supported("-msse4.1").flag_if_supported("-mavx2");
    }

    cc_build.compile(libname);

    println!("cargo:rustc-link-lib=static={libname}");
}

fn generate_bindings(root: &Path) -> anyhow::Result<()> {
    let builder = bindgen::Builder::default()
        .header(format!("{}", root.join("libde265/en265.h").display()))
        .allowlist_type("(de|en)265_.*")
        .allowlist_item("(de|en)265_.*")
        .clang_arg("-std=c++14")
        .clang_arg("-x")
        .clang_arg("c++")
        .constified_enum_module(".*")
        .layout_tests(false);

    builder.generate()?.write_to_file("./src/ffi.rs")?;

    Ok(())
}

fn build_libde265_from_sources() -> anyhow::Result<()> {
    let encoder = feature_enabled("encoder");
    let out_path = PathBuf::from(env::var("OUT_DIR")?);
    let libname = format!("libde265{}", if encoder { "_en" } else { "" });

    let lib_path = out_path.join(format!("{libname}.a"));
    if !lib_path.exists() {
        let archive_file = out_path.join(LIBDE265_FILE_NAME);
        let archive_root_dir = out_path.join(LIBDE265_NAME);

        if !archive_root_dir.exists() {
            download(LIBDE265_URL, &archive_file)?;
            extract(archive_file, &out_path)?;
        }

        compile_and_add_libde265_static_lib(&archive_root_dir, &libname, encoder);

        if feature_enabled("generate-bindings") {
            generate_bindings(&archive_root_dir)?;
        }
    }

    Ok(())
}

fn link_system_libde265() -> anyhow::Result<()> {
    println!("cargo:rustc-link-lib=dylib=de265");
    Ok(())
}

fn main() -> anyhow::Result<()> {
    if feature_enabled("static") {
        build_libde265_from_sources()?;
    } else if feature_enabled("system") {
        link_system_libde265()?;
    } else {
        panic!("Either `system` or `static` feature should be enabled!");
    }

    Ok(())
}
