//! Codec backends.
//!
//! GStreamer (backends/gstreamer.rs) is the one DECODE backend — it wraps every
//! desktop OS's HW+SW decoders (VA / DXVA / VideoToolbox + libav) behind one library.
//! libvpx stays for VP8/VP9 ENCODE (Signal video calls), which the decode-only
//! GStreamer backend does not replace.
//!
//! The hand-written per-OS decoders (vaapi / d3d11 + DXVA HEVC / videotoolbox) and the
//! bundled software decoders (openh264 / libde265 / oxideav / dav1d) were RETIRED on
//! 2026-07-26; the small per-OS GPU zero-copy glue they carried moved to `gpu_interop`.

#[cfg(feature = "libvpx")]
pub mod libvpx;

// GStreamer decode backend — one library covering every DESKTOP OS's HW+SW decoders.
// Never on Android (GStreamer HW = JNI MediaCodec, unusable under `--no-art`).
#[cfg(all(feature = "gstreamer", not(target_os = "android")))]
pub mod gstreamer;

// Per-OS GPU zero-copy interop for the GStreamer backend (ANGLE device handoff +
// D3D11/CVPixelBuffer readbacks) — lifted out of the retired d3d11/videotoolbox decoders.
#[cfg(all(feature = "gstreamer", not(target_os = "android")))]
pub mod gpu_interop;
