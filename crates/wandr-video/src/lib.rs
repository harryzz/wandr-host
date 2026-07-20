//! `wandr-video` — portable video codec backends behind the `wandr:video` WIT
//! contract (task 117).
//!
//! This crate is a backend **dispatch** layer, not a codec: it owns the codec
//! traits and the pixel-format plumbing, and each backend implements them. Today
//! there is exactly one backend — software VP8/VP9 via statically-linked libvpx
//! (BSD-3) — which replaces the FFmpeg dependency the desktop host used to carry.
//! HW backends (VAAPI / VideoToolbox / MediaFoundation) slot in behind the same
//! traits, with libvpx as the fallback.
//!
//! DESKTOP ONLY. Android encodes AND decodes in hardware via MediaCodec and never
//! links a codec library, so the host depends on this crate from its
//! `cfg(not(target_os = "android"))` table — exactly where `ffmpeg-next` sat.
//!
//! DELIBERATELY NOT HERE:
//!   * camera capture — the trait input is pixels. Desktop captures via nokhwa in
//!     the host; Android captures via NDK camera2. Neither belongs in a codec crate.
//!   * compositing / skia — `skia_safe` is the heaviest dependency in the tree and
//!     would make `cargo test -p wandr-video` a multi-minute Skia build.
//!   * preview rects, facing, rotation, visibility, z-order — camera and
//!     compositor policy. The host's `video.rs` keeps owning the WIT-shaped
//!     `EncoderConfig`/`DecoderConfig`/`VideoRect`; this crate takes only the
//!     codec-relevant subset below and the desktop adapter maps at the boundary.
//!     That keeps `video.rs` — and therefore the whole Android backend —
//!     untouched by task 117.

mod backends;
pub mod convert;

pub use convert::{i420_to_rgba, Rgb24Frame};

// ── codec vocabulary ─────────────────────────────────────────────────────────
// Only what a codec actually needs. The host's WIT-shaped types (VideoRect,
// ZLayer, EncoderConfig, DecoderConfig, EncodedFrame) stay in `video.rs`; the
// desktop adapter converts. Two small structs at the boundary beat dragging
// compositor policy into a codec crate.

/// What can go wrong inside a codec. Narrower than the host's `VideoError` —
/// surface/queue/compositor failures cannot originate here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// The build has no backend for this codec (feature off, or HW-only codec).
    Unsupported,
    /// The codec exists but would not initialize with these parameters.
    InitFailed,
    /// A frame was malformed, truncated, or in an unexpected pixel format.
    BadFrame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Vp8,
    Vp9,
}

/// Everything a codec needs to start encoding — and nothing else. Notably absent:
/// camera facing and the PiP preview rect, which are host concerns.
#[derive(Debug, Clone, Copy)]
pub struct EncoderParams {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub bitrate_bps: u32,
    pub framerate: u32,
}

/// Decoder init parameters. `width`/`height` are only a hint from signaling — the
/// stream wins, since VP8/VP9 keyframes carry their own dimensions.
#[derive(Debug, Clone, Copy)]
pub struct DecoderParams {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
}

/// One compressed frame out of the encoder.
pub struct Packet {
    pub data: Vec<u8>,
    /// 90 kHz RTP timestamp (wrapping).
    pub timestamp: u32,
    pub keyframe: bool,
}

// ── traits ───────────────────────────────────────────────────────────────────

/// A video encoder: RGB24 frames in, compressed packets out.
///
/// `Send` is required — the host stores these in a wasmtime `ResourceTable`.
pub trait Encoder: Send {
    /// Encode one tightly-packed RGB24 frame. Resize to the configured encode
    /// size and RGB→I420 conversion happen internally, writing straight into the
    /// codec's own image planes (no intermediate buffer).
    ///
    /// Source dimensions may differ from the encode size — the camera negotiates
    /// its own resolution — and the resize is skipped entirely when they match.
    fn encode(&mut self, frame: Rgb24Frame<'_>, force_keyframe: bool) -> Result<(), CodecError>;

    /// Pop one compressed packet produced by a previous `encode`, if any.
    fn next_packet(&mut self) -> Option<Packet>;

    /// Retune the target bitrate mid-stream (the guest drives this from REMB/TWCC).
    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError>;
}

/// A video decoder: compressed packets in, borrowed I420 frames out.
pub trait Decoder: Send {
    fn decode(&mut self, data: &[u8]) -> Result<(), CodecError>;

    /// Next decoded frame from the last `decode`, as a borrowed I420 view.
    ///
    /// The borrow is what enforces libvpx's rule that the returned image is only
    /// valid until the next `decode` call — a raw pointer would leave that to a
    /// comment. Returns I420 rather than RGBA so decode-to-buffer mode (frame
    /// counting) never pays for a colorspace conversion it throws away.
    fn next_frame(&mut self) -> Option<I420Ref<'_>>;
}

/// A borrowed, non-owning view of a decoded I420 frame.
pub struct I420Ref<'a> {
    pub y: &'a [u8],
    pub y_stride: u32,
    pub u: &'a [u8],
    pub u_stride: u32,
    pub v: &'a [u8],
    pub v_stride: u32,
    pub width: u32,
    pub height: u32,
}

// ── factories ────────────────────────────────────────────────────────────────
// `Box<dyn>` (rather than a concrete type behind a cfg) because the point of the
// crate is runtime "try HW for this codec, fall back to software". One vtable
// call per encoded frame is unmeasurable against a VP8 encode.
//
// With no backend feature enabled the crate is types-only (what Android wants —
// it encodes from a Surface via MediaCodec), and these report `NoHwCodec` rather
// than failing to compile, so callers need no cfg of their own.

pub fn open_encoder(params: &EncoderParams) -> Result<Box<dyn Encoder>, CodecError> {
    #[cfg(feature = "libvpx")]
    {
        return Ok(Box::new(backends::libvpx::LibvpxEncoder::open(params)?));
    }
    #[cfg(not(feature = "libvpx"))]
    {
        let _ = params;
        log::warn!("wandr-video: no codec backend compiled in (enable the `libvpx` feature)");
        Err(CodecError::Unsupported)
    }
}

pub fn open_decoder(params: &DecoderParams) -> Result<Box<dyn Decoder>, CodecError> {
    #[cfg(feature = "libvpx")]
    {
        return Ok(Box::new(backends::libvpx::LibvpxDecoder::open(params)?));
    }
    #[cfg(not(feature = "libvpx"))]
    {
        let _ = params;
        log::warn!("wandr-video: no codec backend compiled in (enable the `libvpx` feature)");
        Err(CodecError::Unsupported)
    }
}

/// 90 kHz RTP timestamp from a frame index at `fps` (wraps like RTP).
pub fn rtp_ts(idx: i64, fps: u32) -> u32 {
    let fps = fps.max(1) as i64;
    ((idx * 90_000) / fps) as u32
}
