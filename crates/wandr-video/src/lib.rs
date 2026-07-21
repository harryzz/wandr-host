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
    H264,
    H265,
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

/// One compressed frame going IN to the decoder.
///
/// `timestamp_us` is a PRESENTATION timestamp in microseconds — the WebCodecs
/// unit, matching `wasi:audio-codec`'s `encoded-chunk`. Deliberately NOT the
/// 90 kHz `u32` RTP clock the call path uses: that is a transport timestamp and
/// it wraps every ~13.25 h, which is fine for a call and useless for a file.
pub struct Chunk<'a> {
    pub data: &'a [u8],
    pub timestamp_us: i64,
}

impl<'a> Chunk<'a> {
    pub fn new(data: &'a [u8], timestamp_us: i64) -> Self {
        Self { data, timestamp_us }
    }
}

/// A video decoder: compressed packets in, borrowed I420 frames out.
///
/// PLAYBACK SHAPE (task 117 M2). The decoder carries presentation timestamps and
/// supports the two stream discontinuities a player needs — end-of-stream
/// (`flush`) and seek (`reset`). It does NOT schedule presentation: the caller
/// decides when a frame is shown, because sync policy differs per player (live
/// vs VOD, frame-drop vs audio-stretch). On Android the WIT-level
/// `present(at-ns)` maps to `AMediaCodec_releaseOutputBufferAtTime`; on desktop
/// the host adapter times it. Neither belongs in the codec.
pub trait Decoder: Send {
    fn decode(&mut self, chunk: Chunk<'_>) -> Result<(), CodecError>;

    /// End of stream: decode anything still queued so `next_frame` can drain it.
    /// (= WebCodecs `flush()`.)
    fn flush(&mut self) -> Result<(), CodecError>;

    /// Seek: discard queued work and reset reference frames. The caller MUST
    /// feed a keyframe next. (= WebCodecs `reset()`.)
    fn reset(&mut self) -> Result<(), CodecError>;

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
    /// The presentation timestamp of the `Chunk` this frame came from, carried
    /// through the codec unchanged. This is what a player schedules against.
    pub timestamp_us: i64,
}

// ── backend registry ─────────────────────────────────────────────────────────
// Runtime codec selection, modelled on oxideav's design (which we spiked — see
// repros/oxideav-spike): every backend declares which codecs it handles, whether
// it is HW or SW, and a priority; `open_*` walks the candidates for the requested
// codec in priority order and returns the first that opens. Adding a backend
// (a HW lane, or oxideav once its silent-fallback bug is fixed) is one new file
// implementing `CodecBackend` plus one line in `default_registry()` — no call
// site changes. That is the decoupling seam.

/// Whether a backend talks to a hardware codec block or decodes in software.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Hardware,
    Software,
}

/// A pluggable codec backend. One instance describes a whole implementation
/// (e.g. "libvpx", "openh264", a future "vaapi"); it is a *factory*, so it is
/// cheap, `Sync`, and constructed once into the registry.
///
/// ‼️ THE FALLBACK CONTRACT (learned from the oxideav spike, where a HW H.264
/// decoder returned success while producing ZERO frames and the registry never
/// fell back): a backend's `open_*` MUST return `Err` if the implementation is
/// not actually going to work — a HW backend that cannot reach its device, or
/// whose probe decode yields nothing, must fail at `open`, NOT succeed and then
/// silently decode nothing. The registry can only fall back on an `Err`.
pub trait CodecBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn kind(&self) -> BackendKind;
    /// Lower wins. HW backends sit ~10, software ~100, so HW is tried first and
    /// software is the fallback.
    fn priority(&self) -> u32;
    fn supports_decode(&self, codec: Codec) -> bool;
    fn supports_encode(&self, codec: Codec) -> bool;
    fn open_decoder(&self, params: &DecoderParams) -> Result<Box<dyn Decoder>, CodecError>;
    fn open_encoder(&self, params: &EncoderParams) -> Result<Box<dyn Encoder>, CodecError>;
}

/// Caller policy over backend selection (mirrors oxideav's `CodecPreferences`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Preferences {
    /// Skip hardware backends entirely — byte-deterministic output, or bisecting
    /// a HW regression against the software path.
    pub no_hardware: bool,
    /// Refuse to fall back to software — surface the HW error instead of silently
    /// degrading. The opt-out for a caller that genuinely needs HW.
    pub require_hardware: bool,
}

/// The set of compiled-in backends, sorted by priority.
pub struct Registry {
    backends: Vec<Box<dyn CodecBackend>>,
}

impl Registry {
    pub fn new() -> Self {
        Registry { backends: Vec::new() }
    }

    /// Add a backend; keeps the list sorted by ascending priority.
    pub fn register(&mut self, backend: Box<dyn CodecBackend>) {
        log::debug!(
            "wandr-video: register backend {} ({:?}, priority {})",
            backend.name(),
            backend.kind(),
            backend.priority()
        );
        self.backends.push(backend);
        self.backends.sort_by_key(|b| b.priority());
    }

    fn candidates(&self, prefs: Preferences) -> impl Iterator<Item = &dyn CodecBackend> {
        // Two independent exclusions — kept as two clauses on purpose; clippy's
        // De-Morgan merge into one `!(… || …)` reads worse than the intent.
        #[allow(clippy::nonminimal_bool)]
        self.backends.iter().map(|b| b.as_ref()).filter(move |b| {
            !(prefs.no_hardware && b.kind() == BackendKind::Hardware)
                && !(prefs.require_hardware && b.kind() == BackendKind::Software)
        })
    }

    pub fn open_decoder(
        &self,
        params: &DecoderParams,
        prefs: Preferences,
    ) -> Result<Box<dyn Decoder>, CodecError> {
        let mut last = CodecError::Unsupported;
        for b in self.candidates(prefs).filter(|b| b.supports_decode(params.codec)) {
            match b.open_decoder(params) {
                Ok(d) => {
                    log::info!("wandr-video: {:?} decode via {}", params.codec, b.name());
                    return Ok(d);
                }
                Err(e) => {
                    log::warn!("wandr-video: {} declined {:?} decode: {e:?}", b.name(), params.codec);
                    last = e;
                }
            }
        }
        Err(last)
    }

    pub fn open_encoder(
        &self,
        params: &EncoderParams,
        prefs: Preferences,
    ) -> Result<Box<dyn Encoder>, CodecError> {
        let mut last = CodecError::Unsupported;
        for b in self.candidates(prefs).filter(|b| b.supports_encode(params.codec)) {
            match b.open_encoder(params) {
                Ok(e) => {
                    log::info!("wandr-video: {:?} encode via {}", params.codec, b.name());
                    return Ok(e);
                }
                Err(e) => {
                    log::warn!("wandr-video: {} declined {:?} encode: {e:?}", b.name(), params.codec);
                    last = e;
                }
            }
        }
        Err(last)
    }
}

impl Default for Registry {
    fn default() -> Self {
        default_registry()
    }
}

/// Build the registry from every backend compiled into this build. A backend
/// whose feature is off simply isn't added; a HW backend whose native library is
/// absent declines at construction (returns `None` from its own `try_new`), so
/// it never enters the registry — the "load failure" fallback path.
pub fn default_registry() -> Registry {
    let mut r = Registry::new();
    #[cfg(feature = "libvpx")]
    r.register(Box::new(backends::libvpx::LibvpxBackend));
    #[cfg(feature = "openh264")]
    r.register(Box::new(backends::openh264::OpenH264Backend));
    #[cfg(feature = "oxideav-h265")]
    r.register(Box::new(backends::oxideav_h265::OxideH265Backend));
    #[cfg(feature = "libde265")]
    r.register(Box::new(backends::libde265::Libde265Backend));
    // Future: HW backends register here at priority < 100, and oxideav slots in
    // as just another software (or HW-bridge) backend once it is ready.
    r
}

fn global_registry() -> &'static Registry {
    use std::sync::OnceLock;
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(default_registry)
}

/// Open a decoder for `params.codec` using the process default registry and
/// default preferences (HW first, software fallback).
pub fn open_decoder(params: &DecoderParams) -> Result<Box<dyn Decoder>, CodecError> {
    global_registry().open_decoder(params, Preferences::default())
}

/// Open an encoder for `params.codec` using the process default registry.
pub fn open_encoder(params: &EncoderParams) -> Result<Box<dyn Encoder>, CodecError> {
    global_registry().open_encoder(params, Preferences::default())
}

/// 90 kHz RTP timestamp from a frame index at `fps` (wraps like RTP).
pub fn rtp_ts(idx: i64, fps: u32) -> u32 {
    let fps = fps.max(1) as i64;
    ((idx * 90_000) / fps) as u32
}
