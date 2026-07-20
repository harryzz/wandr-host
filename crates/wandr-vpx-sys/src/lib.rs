//! Raw FFI bindings to libvpx (BSD-3), generated at build time from the vendored
//! `vendor/libvpx` headers and linked against a static `libvpx.a` this crate's
//! build script compiles.
//!
//! Replaces the `env-libvpx-sys` crate (task 117 step 6). Two reasons:
//!   * that crate is MPL-2.0, and vendoring + modifying it attaches obligations —
//!     unhelpful in a task whose entire purpose is licence cleanliness;
//!   * its build script can only CONSUME a prebuilt libvpx (pkg-config or
//!     `VPX_LIB_DIR`), so a plain `cargo build` always needed a separate step
//!     first. This one builds libvpx itself.
//!
//! Safe wrappers live in `wandr-video`; nothing here is safe to call directly.
#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]
// Generated FFI: bindgen emits `u128` in a few unused typedefs and its own doc
// formatting; neither is worth fighting.
#![allow(dead_code, improper_ctypes, clippy::all)]

include!(concat!(env!("OUT_DIR"), "/vpx_ffi.rs"));
