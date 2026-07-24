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

// The first HARDWARE backend. Linux-only by construction (VA-API is a Linux API)
// and additionally excluded on Android, which reaches its HW codecs through
// MediaCodec and never links a codec library.
#[cfg(all(feature = "vaapi", target_os = "linux", not(target_os = "android")))]
pub mod vaapi;

// Windows HARDWARE H.264 decode via DXVA2 / ID3D11VideoDecoder — the Windows peer
// of vaapi. Runs on the same D3D11 device our ANGLE renderer uses (Phase-2
// zero-copy needs no cross-API bridge). Reuses cros-codecs' H.264 parser.
#[cfg(all(feature = "d3d11", target_os = "windows"))]
pub mod d3d11;

// DXVA HEVC/H.265 decode structures (task 117 M2) — used by d3d11.rs to extend the
// Windows HW path from H.264 to H.265.
#[cfg(all(feature = "d3d11", target_os = "windows"))]
pub mod hevc_dxva;

// DXVA HEVC/H.265 hardware decoder (task 117 M2) — the H.265 peer of d3d11.rs.
#[cfg(all(feature = "d3d11", target_os = "windows"))]
pub mod hevc;

// macOS HW decode via VideoToolbox (system framework, no vendored lib). Registers
// HW-first alongside vaapi/d3d11; VideoToolbox owns the DPB so no cros-codecs.
#[cfg(all(feature = "videotoolbox", target_os = "macos"))]
pub mod videotoolbox;
