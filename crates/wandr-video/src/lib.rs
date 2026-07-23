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

pub use convert::{i420_to_rgba, i420_to_rgba_with, Rgb24Frame};

/// Windows/DXVA2 only: tell the d3d11 decoder which `ID3D11Device` to decode on
/// (ANGLE's, for zero-copy import). Call on the GL thread before opening a decoder.
#[cfg(all(feature = "d3d11", target_os = "windows"))]
pub use backends::d3d11::set_angle_d3d11_device;

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
    Av1,
}

impl Codec {
    /// Every codec in the vocabulary — what capability listing iterates.
    pub const ALL: [Codec; 5] = [Codec::Vp8, Codec::Vp9, Codec::H264, Codec::H265, Codec::Av1];

    /// Lower-case name, as used on the command line and in listings.
    pub fn name(self) -> &'static str {
        match self {
            Codec::Vp8 => "vp8",
            Codec::Vp9 => "vp9",
            Codec::H264 => "h264",
            Codec::H265 => "h265",
            Codec::Av1 => "av1",
        }
    }

    pub fn from_name(s: &str) -> Option<Codec> {
        Codec::ALL.into_iter().find(|c| c.name().eq_ignore_ascii_case(s))
    }
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

    /// How many decoded frames a caller may hold before this decoder can no
    /// longer make progress, or `None` for "bounded only by host memory".
    ///
    /// `None` is the honest answer for every SOFTWARE backend here: they hand
    /// back a borrow of their own or a copy the caller owns, so nothing the
    /// caller keeps can starve the codec. A backend whose output is a FIXED POOL
    /// must report its real budget — MediaCodec output-buffer indices, a V4L2
    /// CAPTURE queue, a VA surface pool — because there every frame the caller
    /// still holds is a buffer the codec cannot decode into, and exceeding it
    /// stops decoding dead rather than erroring.
    ///
    /// The host turns this into `queue-full` back-pressure, which is what lets a
    /// guest feed until told to stop instead of guessing a decode-ahead cushion.
    fn frames_in_flight_limit(&self) -> Option<usize> {
        None
    }

    /// Next decoded frame from the last `decode`, as a borrowed I420 view.
    ///
    /// The borrow is what enforces libvpx's rule that the returned image is only
    /// valid until the next `decode` call — a raw pointer would leave that to a
    /// comment. Returns I420 rather than RGBA so decode-to-buffer mode (frame
    /// counting) never pays for a colorspace conversion it throws away.
    fn next_frame(&mut self) -> Option<Frame<'_>>;
}

/// How a frame's YUV samples map to RGB.
///
/// ‼️ NOT A DETAIL. Get this wrong and the picture is still sharp, still moving,
/// still passes every frame counter and pixel-variance check — just subtly wrong
/// in colour. It is exactly the class of bug this path keeps producing, so it is
/// carried explicitly rather than assumed anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorInfo {
    pub matrix: ColorMatrix,
    /// `false` = studio/limited (16-235), `true` = full (0-255).
    pub full_range: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

impl ColorInfo {
    /// What to assume when the stream does not signal — and most do not
    /// (H.264 `colour_primaries` defaults to 2 = *unspecified*).
    ///
    /// The resolution rule is what ffmpeg, mpv and every player use: SD is
    /// BT.601, HD is BT.709. It is a HEURISTIC and can be wrong, so callers
    /// should say when they fall back to it rather than let a guess look like a
    /// fact.
    pub fn for_resolution(width: u32, height: u32) -> Self {
        let _ = width;
        ColorInfo {
            matrix: if height >= 720 { ColorMatrix::Bt709 } else { ColorMatrix::Bt601 },
            full_range: false,
        }
    }

    /// From H.264 VUI `matrix_coefficients` (ITU-T H.273 / Table E-5) plus the
    /// range flag. `None` for values that do not name a matrix we can apply —
    /// 0 (identity/RGB), 2 (unspecified) and anything unrecognised — so the
    /// caller falls back explicitly instead of silently picking one.
    pub fn from_h264_vui(matrix_coefficients: u8, full_range: bool) -> Option<Self> {
        let matrix = match matrix_coefficients {
            1 => ColorMatrix::Bt709,
            // 4 = FCC, 5 = BT.470BG, 6 = SMPTE 170M. All are the BT.601 matrix.
            4 | 5 | 6 => ColorMatrix::Bt601,
            9 | 10 => ColorMatrix::Bt2020,
            _ => return None,
        };
        Some(ColorInfo { matrix, full_range })
    }
}

/// One decoded picture, OPAQUE over where its pixels live.
///
/// That opacity is the whole point. A backend answers "here is a picture", not
/// "here is a CPU buffer" — so a hardware decoder can hand back a frame that is
/// still in GPU memory and a software decoder can hand back one that never left
/// the CPU, through the same exit. Before this existed the only exit was
/// `I420Ref`, which forced every VA-API frame through a readback: pixels went
/// GPU -> CPU -> GPU, five copies and ~415 MB/s at 720p30, and the hardware
/// decode saved less CPU than the plumbing burned.
///
/// It is an enum INSIDE an opaque type, not a public enum, for the same reason
/// `decoded-frame` is opaque in the WIT: adding a variant later (VideoToolbox
/// `CVPixelBuffer`, MediaCodec buffer indices, V4L2 MMAP) touches the accessors
/// and nothing else, instead of every `match` in the tree.
pub struct Frame<'a> {
    inner: FrameInner<'a>,
    color: ColorInfo,
}

enum FrameInner<'a> {
    /// Pixels on the CPU — borrowed from the codec's own memory (libvpx) or from
    /// the backend's staging slot (openh264, dav1d, libde265, oxideav).
    Cpu(I420Ref<'a>),
    /// Pixels still in GPU memory, exported as a DMA-buf. Self-owning, which is
    /// why `into_gpu` can move it out of the decoder's borrow.
    Gpu(GpuFrame),
}

/// Where a `Frame`'s pixels are. Diagnostics and policy only — a caller decides
/// what to DO via `as_i420` / `into_gpu`, not by branching on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLocation {
    Cpu,
    Gpu,
}

impl<'a> Frame<'a> {
    /// Colour defaults to the resolution heuristic. A backend that KNOWS (because
    /// it parsed the stream's VUI) calls `with_color`.
    pub fn cpu(r: I420Ref<'a>) -> Self {
        let color = ColorInfo::for_resolution(r.width, r.height);
        Frame { inner: FrameInner::Cpu(r), color }
    }
    pub fn gpu(g: GpuFrame) -> Self {
        let color = g.color;
        Frame { inner: FrameInner::Gpu(g), color }
    }
    pub fn with_color(mut self, color: ColorInfo) -> Self {
        self.color = color;
        self
    }
    /// How to convert this frame's YUV to RGB — used by both the CPU converter
    /// and the GPU sampler, so the two lanes cannot disagree.
    pub fn color(&self) -> ColorInfo {
        self.color
    }

    pub fn timestamp_us(&self) -> i64 {
        match &self.inner {
            FrameInner::Cpu(r) => r.timestamp_us,
            FrameInner::Gpu(g) => g.timestamp_us,
        }
    }

    pub fn dimensions(&self) -> (u32, u32) {
        match &self.inner {
            FrameInner::Cpu(r) => (r.width, r.height),
            FrameInner::Gpu(g) => (g.width, g.height),
        }
    }

    pub fn location(&self) -> FrameLocation {
        match &self.inner {
            FrameInner::Cpu(_) => FrameLocation::Cpu,
            FrameInner::Gpu(_) => FrameLocation::Gpu,
        }
    }

    /// CPU view WITHOUT materialising anything — `None` for a GPU frame. A
    /// caller that gets `None` and genuinely needs bytes calls `read_i420`.
    pub fn as_i420(&self) -> Option<&I420Ref<'a>> {
        match &self.inner {
            FrameInner::Cpu(r) => Some(r),
            FrameInner::Gpu(_) => None,
        }
    }

    /// Take the GPU handle. Returns the frame back UNTOUCHED as `Err` when it
    /// was a CPU frame — no panic, no unwrap, and the caller can still read it.
    pub fn into_gpu(self) -> Result<GpuFrame, Frame<'a>> {
        match self.inner {
            FrameInner::Gpu(g) => Ok(g),
            cpu @ FrameInner::Cpu(_) => Err(Frame { inner: cpu, color: self.color }),
        }
    }

    /// Materialise tightly-packed I420 into a caller-owned buffer: a copy for a
    /// CPU frame, the backend's own readback for a GPU one. The caller owns
    /// `scratch` so steady state does not allocate.
    pub fn read_i420<'s>(&self, scratch: &'s mut Vec<u8>) -> Result<I420Ref<'s>, CodecError> {
        let (w, h) = self.dimensions();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (y_len, c_len) = ((w * h) as usize, (cw * ch) as usize);
        match &self.inner {
            FrameInner::Cpu(r) => {
                scratch.clear();
                scratch.reserve(y_len + 2 * c_len);
                for row in 0..h as usize {
                    let o = row * r.y_stride as usize;
                    scratch.extend_from_slice(r.y.get(o..o + w as usize).ok_or(CodecError::BadFrame)?);
                }
                for (plane, stride) in [(r.u, r.u_stride), (r.v, r.v_stride)] {
                    for row in 0..ch as usize {
                        let o = row * stride as usize;
                        scratch
                            .extend_from_slice(plane.get(o..o + cw as usize).ok_or(CodecError::BadFrame)?);
                    }
                }
            }
            FrameInner::Gpu(g) => g.owner.read_i420(scratch)?,
        }
        if scratch.len() < y_len + 2 * c_len {
            return Err(CodecError::BadFrame);
        }
        Ok(I420Ref {
            y: &scratch[..y_len],
            y_stride: w,
            u: &scratch[y_len..y_len + c_len],
            u_stride: cw,
            v: &scratch[y_len + c_len..y_len + 2 * c_len],
            v_stride: cw,
            width: w,
            height: h,
            timestamp_us: self.timestamp_us(),
        })
    }
}

/// A decoded picture living in GPU memory, exported as a DMA-buf.
///
/// The currency is a DMA-BUF and deliberately not a GL texture or an `SkImage`:
/// this crate's header excludes skia and compositing on purpose, and a dma-buf
/// is also what a future Wayland/DRM SCANOUT path wants — a texture would
/// already have committed us to sampling.
pub struct GpuFrame {
    pub width: u32,
    pub height: u32,
    pub timestamp_us: i64,
    /// DRM fourcc — `NV12` for every VLD decoder here.
    pub fourcc: u32,
    /// DRM format modifier. ‼️ Load-bearing: Intel decode output is TILED, and a
    /// tiled buffer imported as linear renders silent garbage. An importer that
    /// cannot pass this on MUST refuse rather than guess.
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
    /// How the sampler must convert these samples. Travels WITH the frame so the
    /// compositor never has to guess, and so the zero-copy and readback lanes
    /// convert identically — otherwise WANDR_VIDEO_ZEROCOPY=0 stops being a
    /// valid A/B.
    pub color: ColorInfo,
    /// Keeps the underlying surface alive AND knows how to read it back.
    owner: Box<dyn GpuFrameOwner>,
}

impl GpuFrame {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        width: u32,
        height: u32,
        timestamp_us: i64,
        fourcc: u32,
        modifier: u64,
        planes: Vec<DmabufPlane>,
        color: ColorInfo,
        owner: Box<dyn GpuFrameOwner>,
    ) -> Self {
        Self { width, height, timestamp_us, fourcc, modifier, planes, color, owner }
    }

    /// The Windows D3D11 texture handle for this frame, if it has one. The host's
    /// ANGLE import calls this; a dma-buf frame (and every non-Windows build)
    /// returns `None` and the caller uses `planes` / `read_i420` instead.
    #[cfg(all(feature = "d3d11", target_os = "windows"))]
    pub fn d3d11(&self) -> Option<D3d11View> {
        self.owner.d3d11()
    }
}

pub struct DmabufPlane {
    /// The dma-buf, owned so it is closed on drop (EGL keeps its own reference
    /// after import). A `File` rather than `OwnedFd` ONLY so the type compiles on
    /// Windows, which has no `std::os::fd`; dma-buf is a Linux concept and this is
    /// never constructed off-Linux (the vaapi backend is the only producer).
    pub fd: std::fs::File,
    pub offset: u32,
    pub pitch: u32,
}

/// Backend-supplied CPU materialisation for a GPU frame.
///
/// ‼️ Kept in the BACKEND, not in a generic "mmap the dma-buf" helper, because
/// the right way to read one is API-specific and getting it wrong is expensive
/// rather than broken. For VA-API this must stay `vaGetImage`: `vaDeriveImage`
/// hands back tiled memory and measured 16.6 s vs 1.15 s for 300 frames of 720p
/// on Ivybridge. A generic dma-buf mmap would reintroduce exactly that.
pub trait GpuFrameOwner: Send {
    fn read_i420(&self, out: &mut Vec<u8>) -> Result<(), CodecError>;

    /// The Windows zero-copy handle, when this GPU frame is a D3D11 NV12 texture
    /// (DXVA2). `None` for a dma-buf frame. Gated to Windows so the DMA-buf lane
    /// and lib.rs stay free of the `windows` crate everywhere else — the Linux
    /// vaapi owner never implements it. The host imports the texture into ANGLE
    /// (same D3D11 device) rather than reading it back.
    #[cfg(all(feature = "d3d11", target_os = "windows"))]
    fn d3d11(&self) -> Option<D3d11View> {
        None
    }
}

/// A borrowed view of a decoded NV12 picture as a D3D11 texture (Windows/DXVA2).
/// The `owner` that hands this out keeps the texture (and its device) alive for
/// as long as the `GpuFrame` lives, so these are safe to use un-refcounted.
#[cfg(all(feature = "d3d11", target_os = "windows"))]
pub struct D3d11View {
    pub texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    /// The device the texture belongs to — the host shares it with ANGLE (or
    /// opens a shared handle from it) so no cross-device copy is needed.
    pub device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
    /// Which array slice of `texture` holds this frame (0 for a per-frame texture).
    pub array_slice: u32,
}

#[cfg(all(feature = "d3d11", target_os = "windows"))]
impl D3d11View {
    /// The raw `ID3D11Texture2D*` to pass as ANGLE's `EGL_D3D11_TEXTURE_ANGLE`
    /// client buffer. Keeps the `windows` crate dependency inside this crate — the
    /// host's `video_gl` import takes an opaque pointer.
    pub fn texture_ptr(&self) -> *mut std::ffi::c_void {
        use windows::core::Interface;
        self.texture.as_raw()
    }
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
    /// Use ONLY this backend, by `name()`. For A/B-ing one implementation against
    /// another on the same machine — the question "is this the codec or the rest
    /// of the pipeline?" is otherwise very hard to answer. An unknown name matches
    /// nothing and the open fails, deliberately: silently ignoring it would give a
    /// measurement of the wrong thing, which is worse than an error.
    pub backend: Option<&'static str>,
}

/// What one backend can do. The shape `--list-codecs` prints and the guest-facing
/// listing is built from.
#[derive(Debug, Clone)]
pub struct BackendInfo {
    pub name: &'static str,
    pub kind: BackendKind,
    pub priority: u32,
    pub decode: Vec<Codec>,
    pub encode: Vec<Codec>,
}

impl BackendInfo {
    pub fn is_hardware(&self) -> bool {
        self.kind == BackendKind::Hardware
    }
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

    /// Every registered backend and what it supports, in priority order.
    ///
    /// ‼️ This ASKS each backend, it does not read a table: a HW backend's
    /// `supports_decode` probes the driver, so the answer is what this machine can
    /// actually do rather than what the build could theoretically do. That is the
    /// whole point of listing it — on a box with no VA driver, vaapi appears with
    /// an EMPTY decode list rather than not appearing at all, which is the
    /// difference between "not built in" and "built in but unusable here".
    pub fn describe(&self) -> Vec<BackendInfo> {
        self.backends
            .iter()
            .map(|b| BackendInfo {
                name: b.name(),
                kind: b.kind(),
                priority: b.priority(),
                decode: Codec::ALL.into_iter().filter(|c| b.supports_decode(*c)).collect(),
                encode: Codec::ALL.into_iter().filter(|c| b.supports_encode(*c)).collect(),
            })
            .collect()
    }

    fn candidates(&self, prefs: Preferences) -> impl Iterator<Item = &dyn CodecBackend> {
        // Three independent exclusions — kept as separate clauses on purpose;
        // clippy's De-Morgan merge into one `!(… || …)` reads worse than the intent.
        #[allow(clippy::nonminimal_bool)]
        self.backends.iter().map(|b| b.as_ref()).filter(move |b| {
            !(prefs.no_hardware && b.kind() == BackendKind::Hardware)
                && !(prefs.require_hardware && b.kind() == BackendKind::Software)
                && prefs.backend.is_none_or(|want| b.name() == want)
        })
    }

    pub fn open_decoder(
        &self,
        params: &DecoderParams,
        prefs: Preferences,
    ) -> Result<Box<dyn Decoder>, CodecError> {
        self.open_decoder_named(params, prefs).map(|(d, _)| d)
    }

    /// `open_decoder`, also reporting WHICH backend served it.
    ///
    /// A caller that asked for hardware and silently got software has no way to
    /// know otherwise — and "why is this dropping frames" is unanswerable without
    /// it. The guest-facing `implementation()` verb is built on this.
    pub fn open_decoder_named(
        &self,
        params: &DecoderParams,
        prefs: Preferences,
    ) -> Result<(Box<dyn Decoder>, BackendInfo), CodecError> {
        let mut last = CodecError::Unsupported;
        for b in self.candidates(prefs).filter(|b| b.supports_decode(params.codec)) {
            match b.open_decoder(params) {
                Ok(d) => {
                    log::info!("wandr-video: {:?} decode via {}", params.codec, b.name());
                    let info = BackendInfo {
                        name: b.name(),
                        kind: b.kind(),
                        priority: b.priority(),
                        decode: Codec::ALL.into_iter().filter(|c| b.supports_decode(*c)).collect(),
                        encode: Codec::ALL.into_iter().filter(|c| b.supports_encode(*c)).collect(),
                    };
                    return Ok((d, info));
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
    // HARDWARE FIRST (priority 10) — but only registered, never assumed: the
    // backend probes the driver in `supports_decode` and declines at `open` if it
    // cannot actually decode, so the software backends below stay the fallback.
    #[cfg(all(feature = "vaapi", target_os = "linux", not(target_os = "android")))]
    r.register(Box::new(backends::vaapi::VaapiBackend));
    #[cfg(all(feature = "d3d11", target_os = "windows"))]
    r.register(Box::new(backends::d3d11::D3d11Backend));
    #[cfg(feature = "libvpx")]
    r.register(Box::new(backends::libvpx::LibvpxBackend));
    #[cfg(feature = "openh264")]
    r.register(Box::new(backends::openh264::OpenH264Backend));
    #[cfg(feature = "oxideav-h265")]
    r.register(Box::new(backends::oxideav_h265::OxideH265Backend));
    #[cfg(feature = "libde265")]
    r.register(Box::new(backends::libde265::Libde265Backend));
    #[cfg(feature = "dav1d")]
    r.register(Box::new(backends::dav1d::Dav1dBackend));
    // Future HW backends (VideoToolbox / MediaFoundation) register alongside
    // vaapi at priority < 100, and oxideav slots in as just another software
    // backend once it is ready.
    r
}

fn global_registry() -> &'static Registry {
    use std::sync::OnceLock;
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(default_registry)
}

/// Describe every backend in the process default registry — `--list-codecs` and
/// the guest-facing capability list both come from here.
pub fn describe_backends() -> Vec<BackendInfo> {
    global_registry().describe()
}

/// Open a decoder for `params.codec` using the process default registry and
/// default preferences (HW first, software fallback).
pub fn open_decoder(params: &DecoderParams) -> Result<Box<dyn Decoder>, CodecError> {
    open_decoder_with(params, Preferences::default())
}

/// Open a decoder against the process default registry with explicit policy —
/// the entry point for forcing HW, forcing SW, or pinning one named backend.
pub fn open_decoder_with(
    params: &DecoderParams,
    prefs: Preferences,
) -> Result<Box<dyn Decoder>, CodecError> {
    global_registry().open_decoder(params, prefs)
}

/// `open_decoder_with`, also reporting which backend served it.
pub fn open_decoder_named(
    params: &DecoderParams,
    prefs: Preferences,
) -> Result<(Box<dyn Decoder>, BackendInfo), CodecError> {
    global_registry().open_decoder_named(params, prefs)
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
