// rsbinder-aidl generated Rust bindings for vendored AOSP HALs.
// Source AIDL: wandr-host/vendor/aosp-hardware-interfaces (android-11.0.0_r48).
// Generated at build time by build.rs into $OUT_DIR/aosp_hal_bindings.rs.
//
// The codegen emits modules matching the AIDL package paths, e.g.
// android::hardware::vibrator::IVibrator::IVibrator (trait) +
// android::hardware::vibrator::Effect (enum). Consumer code reaches them
// via `crate::binder_aidl::android::hardware::vibrator::...`.

#[cfg(target_os = "android")]
#[allow(non_snake_case, non_camel_case_types, non_upper_case_globals, dead_code, unused_imports, clippy::all)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/aosp_hal_bindings.rs"));
}

#[cfg(target_os = "android")]
pub use generated::*;
