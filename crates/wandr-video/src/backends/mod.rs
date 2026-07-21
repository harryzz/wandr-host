//! Codec backends. Today: software VP8/VP9 via statically-linked libvpx.
//!
//! HW backends land here per platform (vaapi.rs / videotoolbox.rs /
//! mediafoundation.rs), with `open_encoder`/`open_decoder` in lib.rs trying HW for
//! the requested codec first and falling back to libvpx — which is what FFmpeg did
//! for us, minus the licence and the runtime `.so`.

#[cfg(feature = "libvpx")]
pub mod libvpx;

#[cfg(feature = "openh264")]
pub mod openh264;

#[cfg(feature = "oxideav-h265")]
pub mod oxideav_h265;

#[cfg(feature = "libde265")]
pub mod libde265;

#[cfg(feature = "dav1d")]
pub mod dav1d;
