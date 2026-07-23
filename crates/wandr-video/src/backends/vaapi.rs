//! VA-API **hardware** H.264 decode (task 117 M2 stage 3) — the first HW backend
//! in the registry, and the reason `BackendKind::Hardware` exists.
//!
//! This is a port of `repros/vaapi-decode-probe`, which proved the approach on
//! real hardware (300/300 frames on two independent i965 boxes, verified on
//! dumped pixels rather than a frame counter). Nothing here is new technique;
//! what is new is the trait plumbing — PTS round-tripping, NV12→I420, and the
//! decline-don't-panic behaviour the registry's fallback contract demands.
//!
//! ‼️ WHY VA-ALLOCATED SURFACES AND NOT cros-codecs' OWN FRAME TYPES: its only
//! `VideoFrame` impls are GBM/DMA-backed, and GBM allocation fails on BOTH
//! available machines for unrelated reasons (Ivybridge i915 rejects
//! `GBM_BO_USE_HW_VIDEO_DECODER` contiguous NV12; WSL's DRM node is *vgem*, a
//! dummy device whose real GPU memory lives behind /dev/dxg). VA-API itself works
//! on both. So we implement `VideoFrame` over a VA-allocated `Surface<()>`
//! (`vaCreateSurfaces`, no GBM anywhere) and keep cros-codecs only for its H.264
//! parser + DPB/reference management — the genuinely hard part.
//!
//! ‼️ H.264 ONLY, ON PURPOSE. cros-codecs also implements VP8/VP9/HEVC/AV1
//! stateless decoders, and adding them here is mechanical. They are NOT added
//! because neither available machine can execute them: both are Intel Gen7
//! (Ivybridge), which has no VP8/VP9/HEVC/AV1 decode block at all. Shipping a HW
//! lane nobody can run is precisely the trap the fallback contract was written
//! about — an untested backend that claims a codec and then silently produces
//! nothing. Add each codec when there is hardware to prove it on.
//!
//! OUTPUT TIERS (see the probe for the full write-up): tier 1 = zero-copy
//! `export_prime` → DMA-buf, tier 2 = `vaDeriveImage` map, tier 3 = `vaGetImage`
//! copy. We consume TIER 3, with tier 2 as the fallback — counter-intuitive, and
//! measured: see `readback_i420`. The `I420Ref` contract forces a CPU readback
//! regardless of how the pixels got here, so tier 1's saving only materialises
//! once the host consumes a texture directly — that is the zero-copy present
//! path, not this crate.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;

use cros_codecs::backend::vaapi::decoder::VaapiBackend as CrosVaapiBackend;
use cros_codecs::bitstream_utils::NalIterator;
use cros_codecs::codec::h264::parser::Nalu as H264Nalu;
use cros_codecs::decoder::stateless::h264::H264;
use cros_codecs::decoder::stateless::{DecodeError, StatelessDecoder, StatelessVideoDecoder};
use cros_codecs::decoder::{BlockingMode, DecodedHandle, DecoderEvent};
use cros_codecs::video_frame::{ReadMapping, VideoFrame, WriteMapping};
use cros_codecs::{Fourcc, Resolution};
use libva::{Display, Surface, UsageHint};

use crate::{
    BackendKind, Chunk, Codec, CodecBackend, CodecError, Decoder, DecoderParams, Encoder,
    EncoderParams, I420Ref,
};
use crate::Frame;

/// Upstream cros-codecs 0.0.6 builds a throwaway 16x16 placeholder context at
/// backend construction and `.expect()`s the result. A driver whose decode
/// minimum exceeds this rejects it and the process PANICS — see `caps()`.
const UPSTREAM_PLACEHOLDER_DIM: i32 = 16;

// ── backend descriptor ───────────────────────────────────────────────────────

pub struct VaapiBackend;

impl CodecBackend for VaapiBackend {
    fn name(&self) -> &'static str {
        "vaapi"
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Hardware
    }
    /// Ahead of every software backend (which sit at 100), so HW is tried first
    /// and software remains the fallback.
    fn priority(&self) -> u32 {
        10
    }

    /// PROBED, never declared. The capability bits lie in both directions across
    /// drivers — the probe found one advertising `DRM_PRIME_2` while hanging on
    /// decode, and another reporting no memory types at all while decoding fine —
    /// so this asks the driver and caches the answer.
    fn supports_decode(&self, codec: Codec) -> bool {
        codec == Codec::H264 && caps().h264_vld
    }

    /// Encode is NOT wired. Both available boxes advertise `VAEntrypointEncSlice`
    /// for H.264, so this is a real gap rather than an impossible one — but the
    /// encoder lane feeds live calls (congestion control, forced keyframes,
    /// bitrate retuning), and none of that is exercised by task 117's playback
    /// work. openh264 keeps encoding until there is a test that would catch a
    /// HW encoder regressing.
    fn supports_encode(&self, _codec: Codec) -> bool {
        false
    }

    fn open_decoder(&self, p: &DecoderParams) -> Result<Box<dyn Decoder>, CodecError> {
        if p.codec != Codec::H264 {
            return Err(CodecError::Unsupported);
        }
        Ok(Box::new(VaapiDecoder::open()?))
    }

    fn open_encoder(&self, _p: &EncoderParams) -> Result<Box<dyn Encoder>, CodecError> {
        Err(CodecError::Unsupported)
    }
}

// ── capability probe ─────────────────────────────────────────────────────────

struct Caps {
    h264_vld: bool,
}

/// Open the VA display. `cros-libva`'s `Display::open()` scans
/// renderD128..renderD191; `WANDR_DRM_DEVICE` overrides that for machines where
/// the usable node is elsewhere (WSL's vgem card0) or where a multi-GPU box needs
/// a specific one. An operator knob, not a per-app value.
fn open_display() -> Option<Rc<Display>> {
    match std::env::var("WANDR_DRM_DEVICE") {
        Ok(p) if !p.is_empty() => match Display::open_drm_display(PathBuf::from(&p)) {
            Ok(d) => Some(d),
            Err(e) => {
                log::warn!("wandr-video: vaapi: open {p}: {e:?}");
                None
            }
        },
        _ => Display::open(),
    }
}

/// Probe the driver ONCE per process and cache it. Opening a VA display is far
/// too expensive to repeat on every `supports_decode` call, which the registry
/// makes for every candidate on every open.
fn caps() -> &'static Caps {
    static CAPS: OnceLock<Caps> = OnceLock::new();
    CAPS.get_or_init(|| {
        let Some(display) = open_display() else {
            log::info!("wandr-video: vaapi: no VA display — HW decode unavailable");
            return Caps { h264_vld: false };
        };

        // 1. Does this driver actually decode H.264? Main is the profile
        //    cros-codecs itself configures; High streams decode on a Main config
        //    (VA profiles are a superset ladder here), and every H.264 VLD driver
        //    seen advertises both.
        let vld = display
            .query_config_entrypoints(libva::VAProfile::VAProfileH264Main)
            .map(|eps| eps.contains(&libva::VAEntrypoint::VAEntrypointVLD))
            .unwrap_or(false);
        if !vld {
            log::info!("wandr-video: vaapi: driver has no H264 VLD entrypoint — software it is");
            return Caps { h264_vld: false };
        }

        // 2. ‼️ THE PANIC GUARD, and the reason this probe exists at all.
        //    cros-codecs 0.0.6 hardcodes a 16x16 placeholder context at backend
        //    construction and `.expect()`s it. Mesa's D3D12/VAOn12 driver enforces
        //    a decode-heap minimum (64x64) and rejects it with
        //    VA_STATUS_ERROR_RESOLUTION_NOT_SUPPORTED — which would take the whole
        //    HOST process down, not just this decoder. Refuse the backend instead;
        //    the software fallback then handles the stream normally.
        //    (repros/vaapi-decode-probe carries a vendored cros-codecs that asks
        //    the driver for its minimum instead of assuming one. It is not vendored
        //    here: the only driver that needs it is d3d12, whose decode hangs at the
        //    driver level anyway — ffmpeg -hwaccel vaapi hangs identically — so
        //    lifting this restriction would buy a backend that cannot decode.)
        let min = min_decode_dims(&display);
        if let Some((mw, mh)) = min {
            if mw > UPSTREAM_PLACEHOLDER_DIM || mh > UPSTREAM_PLACEHOLDER_DIM {
                log::warn!(
                    "wandr-video: vaapi: driver minimum {mw}x{mh} exceeds the {UPSTREAM_PLACEHOLDER_DIM}x{UPSTREAM_PLACEHOLDER_DIM} \
                     placeholder cros-codecs hardcodes — declining HW to avoid its panic"
                );
                return Caps { h264_vld: false };
            }
        }

        log::info!("wandr-video: vaapi: H264 VLD available — HW decode enabled");
        Caps { h264_vld: true }
    })
}

/// The driver's minimum decode dimensions, or `None` when it reports none (which
/// both working boxes do — an unreported minimum means "no constraint", not zero).
fn min_decode_dims(display: &Rc<Display>) -> Option<(i32, i32)> {
    let mut config = display
        .create_config(
            vec![libva::VAConfigAttrib {
                type_: libva::VAConfigAttribType::VAConfigAttribRTFormat,
                value: libva::VA_RT_FORMAT_YUV420,
            }],
            libva::VAProfile::VAProfileH264Main,
            libva::VAEntrypoint::VAEntrypointVLD,
        )
        .ok()?;
    let first_int = |cfg: &mut libva::Config, t| -> Option<i32> {
        cfg.query_surface_attributes_by_type(t).ok()?.into_iter().find_map(|g| match g {
            libva::GenericValue::Integer(i) if i > 0 => Some(i),
            _ => None,
        })
    };
    let w = first_int(&mut config, libva::VASurfaceAttribType::VASurfaceAttribMinWidth);
    let h = first_int(&mut config, libva::VASurfaceAttribType::VASurfaceAttribMinHeight);
    match (w, h) {
        (None, None) => None,
        (w, h) => Some((w.unwrap_or(0), h.unwrap_or(0))),
    }
}

// ── a VideoFrame backed by a VA-allocated surface (no GBM) ───────────────────

/// Placeholder frame. The real pixels live in the VA surface that
/// `to_native_handle` allocates; we read them back off the decoded handle's
/// surface, so `map()` is never used.
#[derive(Debug)]
struct VaSurfaceFrame {
    resolution: Resolution,
}

impl VideoFrame for VaSurfaceFrame {
    type MemDescriptor = (); // () = "VA allocates the memory itself"
    type NativeHandle = Surface<()>;

    fn fourcc(&self) -> Fourcc {
        Fourcc::from(b"NV12")
    }
    fn resolution(&self) -> Resolution {
        self.resolution
    }
    fn get_plane_size(&self) -> Vec<usize> {
        let (w, h) = (self.resolution.width as usize, self.resolution.height as usize);
        vec![w * h, w * h / 2]
    }
    fn get_plane_pitch(&self) -> Vec<usize> {
        let w = self.resolution.width as usize;
        vec![w, w]
    }
    fn map<'a>(&'a self) -> Result<Box<dyn ReadMapping<'a> + 'a>, String> {
        Err("VaSurfaceFrame: read back via the decoded handle's surface".into())
    }
    fn map_mut<'a>(&'a mut self) -> Result<Box<dyn WriteMapping<'a> + 'a>, String> {
        Err("VaSurfaceFrame is decode-output only".into())
    }
    /// Let VA allocate the decode target — this is the whole point: no GBM.
    fn to_native_handle(&self, display: &Rc<Display>) -> Result<Self::NativeHandle, String> {
        display
            .create_surfaces(
                libva::VA_RT_FORMAT_YUV420,
                Some(u32::from(self.fourcc())),
                self.resolution.width,
                self.resolution.height,
                Some(UsageHint::USAGE_HINT_DECODER),
                vec![()],
            )
            .map_err(|e| format!("vaCreateSurfaces failed: {e:?}"))?
            .pop()
            .ok_or_else(|| "vaCreateSurfaces returned no surface".to_string())
    }
}

// ── readback: VA surface -> owned, tightly-packed I420 ───────────────────────

/// A decoded picture still living in its VA surface.
struct Decoded {
    /// `cros_codecs`' own `DecodedHandle` alias is pub(crate), so name what it
    /// actually is — `VaapiDecodedHandle` itself is public. Avoids a third patch
    /// to the vendored fork just to widen a type alias.
    handle: Rc<RefCell<cros_codecs::backend::vaapi::decoder::VaapiDecodedHandle<VaSurfaceFrame>>>,
    w: u32,
    h: u32,
    pts_us: i64,
}

/// Whether this stream exports DMA-bufs. Latched after the first attempt: an
/// export that fails on frame 1 will fail on frame 300, and retrying per frame
/// would cost a syscall each time to learn the same thing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExportState {
    Untried,
    Exports,
    ReadsBack,
}

/// One decoded frame, owned as tightly-packed I420 (the `I420Ref` shape).
struct DecodedI420 {
    buf: Vec<u8>,
    w: u32,
    h: u32,
    pts_us: i64,
}

/// Pull an NV12 VA surface into an owned I420 frame.
///
/// ‼️ TIER 3 (`create_from` / vaGetImage) FIRST, tier 2 (`derive_from`) only as a
/// fallback — the opposite of what "a map beats a copy" suggests, and MEASURED,
/// not assumed. `vaDeriveImage` on Intel hands back the surface's own memory,
/// which is TILED: every CPU read walks a swizzled address pattern with no useful
/// cache behaviour. `vaGetImage` costs a copy but the driver DETILES it, so the
/// bytes we then touch are linear. MEASURED on Ivybridge/i965, 300 frames of
/// 720p: 16.6 s via derive_from vs 1.15 s via create_from — 14x, and the
/// difference between dropping 251 of 300 frames and running at ~260 fps.
///
/// This file originally had it the other way round, on the plausible-sounding
/// reasoning that a map must beat a copy. It cost more than it saved and nothing
/// in the frame counters showed it — only wall-clock did. The probe had used
/// vaGetImage all along; the "optimization" was the regression.
///
/// The fallback direction still matters: NVDEC decodes into CUDA memory and
/// rejects `vaGetImage`-style access patterns differently, so keeping both paths
/// is what lets one backend serve drivers that disagree about which works at all.
fn readback_i420(surface: &Surface<()>, w: u32, h: u32, pts_us: i64) -> Result<DecodedI420, CodecError> {
    let res = (w, h);
    // vaGetImage needs an explicit target format; NV12 is what every VLD decoder
    // here produces.
    let mut fmt: libva::VAImageFormat = unsafe { std::mem::zeroed() };
    fmt.fourcc = u32::from(Fourcc::from(b"NV12"));
    fmt.byte_order = 1; // VA_LSB_FIRST
    fmt.bits_per_pixel = 12;
    let image = match libva::Image::create_from(surface, fmt, res, res) {
        Ok(img) => img,
        Err(_) => libva::Image::derive_from(surface, res).map_err(|e| {
            log::warn!("wandr-video: vaapi: create_from and derive_from both failed: {e:?}");
            CodecError::BadFrame
        })?,
    };
    let va = *image.image();
    let data: &[u8] = image.as_ref();
    let (y_off, y_pitch) = (va.offsets[0] as usize, va.pitches[0] as usize);
    let (uv_off, uv_pitch) = (va.offsets[1] as usize, va.pitches[1] as usize);
    let (wu, hu) = (w as usize, h as usize);
    let (cw, ch) = (w.div_ceil(2) as usize, h.div_ceil(2) as usize);

    // NV12 -> I420: copy luma row by row (dropping the surface's padding), then
    // DE-INTERLEAVE the single UV plane into separate U and V planes. The
    // de-interleave is the part the probe never had to do — it dumped NV12
    // straight to PPM — so it is the one genuinely new piece of pixel handling
    // here, and the one worth checking on a real picture rather than a counter.
    let mut buf = vec![0u8; wu * hu + 2 * cw * ch];
    for row in 0..hu {
        let src = y_off + row * y_pitch;
        let src_end = src.checked_add(wu).ok_or(CodecError::BadFrame)?;
        if src_end > data.len() {
            return Err(CodecError::BadFrame);
        }
        buf[row * wu..row * wu + wu].copy_from_slice(&data[src..src_end]);
    }
    let u_base = wu * hu;
    let v_base = u_base + cw * ch;
    for row in 0..ch {
        let src = uv_off + row * uv_pitch;
        if src + cw * 2 > data.len() {
            return Err(CodecError::BadFrame);
        }
        for col in 0..cw {
            buf[u_base + row * cw + col] = data[src + col * 2];
            buf[v_base + row * cw + col] = data[src + col * 2 + 1];
        }
    }
    Ok(DecodedI420 { buf, w, h, pts_us })
}

/// Operator escape hatch, matching WANDR_VIDEO_BACKEND / WANDR_VIDEO_NO_HW: forces
/// the readback path so the two lanes can be compared on one machine, and gives a
/// one-env-var rollback if an import turns out to be wrong on some driver.
fn zerocopy_disabled() -> bool {
    std::env::var("WANDR_VIDEO_ZEROCOPY").is_ok_and(|v| v == "0")
}

/// Keeps the decoded VA surface alive for as long as the exported DMA-buf is in
/// use, and knows the ONE correct way to read it back.
struct VaSurfaceOwner {
    decoded: Decoded,
}

// SAFETY: same invariant as `VaapiDecoder` — the handle and its `Rc` graph are a
// self-contained object used from one thread at a time (the store thread). `Sync`
// is deliberately NOT claimed, so sharing is still a compile error.
unsafe impl Send for VaSurfaceOwner {}

impl crate::GpuFrameOwner for VaSurfaceOwner {
    fn read_i420(&self, out: &mut Vec<u8>) -> Result<(), CodecError> {
        let inner = self.decoded.handle.borrow();
        let f = readback_i420(inner.surface(), self.decoded.w, self.decoded.h, self.decoded.pts_us)?;
        out.clear();
        out.extend_from_slice(&f.buf);
        Ok(())
    }
}

/// Turn a `VADRMPRIMESurfaceDescriptor` into the crate's `GpuFrame`.
///
/// Only the single-object / single-layer shape is accepted, which is what
/// `export_prime` produces for NV12 (it asks for COMPOSED_LAYERS). Anything else
/// returns `None` and the caller reads back — describing a shape we have not
/// seen and cannot test would be guessing at the exact point where a wrong guess
/// renders silent garbage.
fn gpu_frame_from_prime(
    mut desc: libva::DrmPrimeSurfaceDescriptor,
    decoded: Decoded,
    color: crate::ColorInfo,
) -> Option<crate::GpuFrame> {
    // Copy the scalars out before touching `objects` — `layers` borrows `desc`.
    let (drm_format, num_planes, offsets, pitches) = {
        let l = desc.layers.first()?;
        (l.drm_format, l.num_planes as usize, l.offset, l.pitch)
    };
    // Only the single-object shape is accepted. `export_prime` asks for
    // COMPOSED_LAYERS, so NV12 comes back as one object with one layer; anything
    // else is a shape we have never seen and cannot test, and guessing at it is
    // exactly where a wrong guess renders silent garbage.
    if desc.objects.len() != 1 || num_planes == 0 || num_planes > 4 {
        return None;
    }
    let obj = desc.objects.pop()?;
    let modifier = obj.drm_format_modifier;
    let mut planes = Vec::with_capacity(num_planes);
    for i in 0..num_planes {
        // All planes live in the same buffer object, so each gets its own dup of
        // the one fd — independently closed, no shared-ownership bookkeeping.
        planes.push(crate::DmabufPlane {
            // OwnedFd -> File: File owns/closes the fd the same way; the crate's
            // DmabufPlane uses File so it compiles on Windows (see its doc).
            fd: std::fs::File::from(obj.fd.try_clone().ok()?),
            offset: offsets[i],
            pitch: pitches[i],
        });
    }
    let (w, h, pts) = (decoded.w, decoded.h, decoded.pts_us);
    Some(crate::GpuFrame::new(
        w,
        h,
        pts,
        drm_format,
        modifier,
        planes,
        color,
        Box::new(VaSurfaceOwner { decoded }),
    ))
}

// ── decoder ──────────────────────────────────────────────────────────────────

type CrosDecoder = StatelessDecoder<H264, CrosVaapiBackend<VaSurfaceFrame>>;

pub struct VaapiDecoder {
    /// Held for the decoder's lifetime — `dec` borrows VA state from it.
    display: Rc<Display>,
    dec: CrosDecoder,
    /// Stream resolution, learned from `FormatChanged`. The frame-allocation
    /// callback reads it, which is why it is shared rather than owned.
    res: Rc<RefCell<Resolution>>,
    /// Frames decoded and synced, oldest first — still in GPU memory.
    out: VecDeque<Decoded>,
    /// The CPU frame the most recent `next_frame` materialised, if it had to.
    /// Kept alive so its borrow stays valid until the following `next_frame`.
    current: Option<DecodedI420>,
    /// The stream's signalled colour, from the SPS VUI. `None` until an SPS is
    /// seen (or when it signals nothing usable), in which case the resolution
    /// heuristic applies — see `ColorInfo::for_resolution`.
    color: Option<crate::ColorInfo>,
    /// cros-codecs' own H.264 parser, used ONLY to read the VUI. The decoder
    /// parses the SPS internally too but does not expose it, and hand-rolling an
    /// SPS reader (exp-golomb, scaling lists, HRD) to recover three fields would
    /// be a needless second implementation to get wrong.
    parser: cros_codecs::codec::h264::parser::Parser,
    /// Whether to export DMA-bufs (zero-copy) or read back. Latched per stream
    /// from the FIRST export attempt: capability bits lie in both directions on
    /// these drivers, so the only trustworthy probe is doing it once for real.
    export: ExportState,
}

// SAFETY: the same reasoning as every other backend in this crate (openh264,
// libde265, dav1d all do this). `StatelessDecoder` and `Rc<Display>` are not
// `Sync` and carry non-atomic refcounts, but the whole `VaapiDecoder` — display,
// decoder, and every `Rc` clone inside it — is one self-contained object that is
// only ever used from one thread at a time; the host owns it in a wasmtime
// `ResourceTable` and drives it from the store's thread. Moving the object
// between threads is sound; sharing it is not, and `Sync` is deliberately NOT
// claimed, so the compiler still forbids that.
unsafe impl Send for VaapiDecoder {}

impl VaapiDecoder {
    pub fn open() -> Result<Self, CodecError> {
        if !caps().h264_vld {
            return Err(CodecError::Unsupported);
        }
        let display = open_display().ok_or(CodecError::InitFailed)?;
        let dec = Self::new_decoder(&display)?;
        Ok(Self {
            display,
            dec,
            res: Rc::new(RefCell::new(Resolution::from((0, 0)))),
            out: VecDeque::new(),
            current: None,
            export: ExportState::Untried,
            color: None,
            parser: Default::default(),
        })
    }

    /// One place to build the cros-codecs decoder so `open` and `reset` agree.
    ///
    /// ‼️ `catch_unwind` is not defensive clutter. cros-codecs 0.0.6 builds its
    /// placeholder VA config/context inside `VaapiBackend::new` with `.expect()`
    /// on both — a driver that refuses either takes the entire host process down.
    /// `caps()` already declines the one failure mode we have actually observed
    /// (a decode minimum above the hardcoded placeholder), but a panic here is so
    /// much worse than a software fallback that catching the ones we have not
    /// predicted is worth the six lines. A caught panic is reported as
    /// `InitFailed`, which is exactly what the registry needs to fall back.
    fn new_decoder(display: &Rc<Display>) -> Result<CrosDecoder, CodecError> {
        let d = Rc::clone(display);
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            StatelessDecoder::<H264, CrosVaapiBackend<VaSurfaceFrame>>::new_vaapi(
                d,
                BlockingMode::Blocking,
            )
        }));
        match built {
            Ok(Ok(dec)) => Ok(dec),
            Ok(Err(e)) => {
                log::warn!("wandr-video: vaapi: decoder init: {e:?}");
                Err(CodecError::InitFailed)
            }
            Err(_) => {
                log::warn!(
                    "wandr-video: vaapi: cros-codecs PANICKED building its VA context — \
                     declining HW so the registry falls back to software"
                );
                Err(CodecError::InitFailed)
            }
        }
    }

    /// Consume every pending decoder event, reading each ready frame back into an
    /// owned I420 buffer.
    ///
    /// ‼️ PTS COMES BACK FROM THE HANDLE, not from a FIFO. cros-codecs carries the
    /// timestamp we hand `decode()` through to the decoded handle, so presentation
    /// order and timestamps stay married even when the stream reorders — which is
    /// strictly better than the openh264 backend's input FIFO, and the reason this
    /// backend needs no "no B-frames" caveat.
    fn drain(&mut self) {
        while let Some(ev) = self.dec.next_event() {
            match ev {
                DecoderEvent::FrameReady(handle) => {
                    if let Err(e) = handle.sync() {
                        log::warn!("wandr-video: vaapi: sync decoded frame: {e:?}");
                        continue;
                    }
                    let r = handle.display_resolution();
                    let pts_us = handle.timestamp() as i64;
                    // Park the HANDLE, decoded and synced, without touching the
                    // pixels. Whether they ever reach the CPU is `next_frame`'s
                    // decision — and in the zero-copy path they never do. This
                    // used to call `readback_i420` right here, which meant even
                    // decode-to-BUFFER (frame counting, which throws the pixels
                    // away) paid a full vaGetImage + de-interleave per frame.
                    self.out.push_back(Decoded { handle, w: r.width, h: r.height, pts_us });
                }
                DecoderEvent::FormatChanged => {
                    if let Some(info) = self.dec.stream_info() {
                        *self.res.borrow_mut() = info.coded_resolution;
                        log::info!(
                            "wandr-video: vaapi: format {:?} coded {}x{} display {}x{}",
                            info.format,
                            info.coded_resolution.width,
                            info.coded_resolution.height,
                            info.display_resolution.width,
                            info.display_resolution.height
                        );
                    }
                }
            }
        }
    }
}

impl Decoder for VaapiDecoder {
    fn decode(&mut self, chunk: Chunk<'_>) -> Result<(), CodecError> {
        // cros-codecs consumes ONE NAL per `decode` call and reports how many
        // bytes it took, so a caller handing us a whole access unit (SPS+PPS+
        // slices) needs splitting and looping. `NalIterator` does the Annex-B
        // split; a caller passing a single NAL just gets a one-element iterator.
        // Read the stream's own colour signalling once, from the SPS. Assuming a
        // matrix instead is the difference between a correct picture and one that
        // is subtly, silently wrong — no counter downstream can detect it. Costs a
        // scan only until an SPS is seen, then never again.
        if self.color.is_none() {
            let mut cursor = std::io::Cursor::new(chunk.data);
            while let Ok(nalu) = H264Nalu::next(&mut cursor) {
                if !matches!(nalu.header.type_, cros_codecs::codec::h264::parser::NaluType::Sps) {
                    continue;
                }
                if let Ok(sps) = self.parser.parse_sps(&nalu) {
                    let vui = &sps.vui_parameters;
                    let signalled =
                        sps.vui_parameters_present_flag && vui.colour_description_present_flag;
                    self.color = signalled
                        .then(|| {
                            crate::ColorInfo::from_h264_vui(
                                vui.matrix_coefficients,
                                vui.video_full_range_flag,
                            )
                        })
                        .flatten();
                    match self.color {
                        Some(c) => log::info!("wandr-video: vaapi: stream signals {c:?}"),
                        None => log::info!(
                            "wandr-video: vaapi: no usable colour signalled (VUI present={}) \
                             — using the resolution heuristic",
                            sps.vui_parameters_present_flag
                        ),
                    }
                }
                break;
            }
        }

        let ts = chunk.timestamp_us as u64;
        for nal in NalIterator::<H264Nalu>::new(chunk.data) {
            let bitstream = nal.as_ref();
            let mut off = 0usize;
            let mut stalls = 0u32;
            while off < bitstream.len() {
                let res_cb = Rc::clone(&self.res);
                let mut alloc_cb = move || {
                    let r = *res_cb.borrow();
                    (r.width > 0).then_some(VaSurfaceFrame { resolution: r })
                };
                match self.dec.decode(ts, &bitstream[off..], &mut alloc_cb) {
                    // No progress on this input: drain and abandon this NAL rather
                    // than spin. Headers before the first SPS land here.
                    Ok(0) => {
                        self.drain();
                        break;
                    }
                    Ok(n) => {
                        off += n;
                        stalls = 0;
                        self.drain();
                    }
                    // Back-pressure, NOT an error: the decoder needs pending events
                    // consumed (or output frames returned) before it can accept
                    // more. Drain and RETRY the same bytes.
                    Err(DecodeError::CheckEvents) | Err(DecodeError::NotEnoughOutputBuffers(_)) => {
                        self.drain();
                        stalls += 1;
                        if stalls > 64 {
                            log::warn!("wandr-video: vaapi: stalled on back-pressure, skipping NAL");
                            break;
                        }
                    }
                    Err(e) => {
                        log::warn!("wandr-video: vaapi: decode: {e:?}");
                        return Err(CodecError::BadFrame);
                    }
                }
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), CodecError> {
        // Release the frames the DPB is holding back for reordering, then collect
        // them. Without this the tail of a clip silently disappears.
        if let Err(e) = self.dec.flush() {
            log::warn!("wandr-video: vaapi: flush: {e:?}");
        }
        self.drain();
        Ok(())
    }

    fn reset(&mut self) -> Result<(), CodecError> {
        // A fresh decoder is the reliable seek: it drops all reference state, and
        // the caller feeds a keyframe next. The VA display is reused — it is the
        // expensive part and holds no per-stream state.
        self.dec = Self::new_decoder(&self.display)?;
        *self.res.borrow_mut() = Resolution::from((0, 0));
        self.out.clear();
        self.current = None;
        Ok(())
    }

    fn next_frame(&mut self) -> Option<Frame<'_>> {
        let d = self.out.pop_front()?;

        // ZERO-COPY FIRST. `export_prime` hands back a DMA-buf fd for the very
        // memory the hardware decoded into: no readback, no de-interleave, no
        // colour conversion, and the host can import it straight as a texture.
        // Falling back to a readback when it fails is not defensive clutter — a
        // driver that decodes perfectly but cannot export is still a perfectly
        // good decoder, and declining the whole backend would drop us to
        // openh264 over a PRESENTATION limitation. That is why this lives here
        // and not in `caps()`.
        if self.export != ExportState::ReadsBack && !zerocopy_disabled() {
            let inner = d.handle.borrow();
            match inner.surface().export_prime() {
                Ok(desc) => {
                    if self.export == ExportState::Untried {
                        log::info!(
                            "wandr-video: vaapi: zero-copy ON — export_prime gave {} object(s), \
                             {} layer(s), modifier {:#x}",
                            desc.objects.len(),
                            desc.layers.len(),
                            desc.objects.first().map(|o| o.drm_format_modifier).unwrap_or(0),
                        );
                        self.export = ExportState::Exports;
                    }
                    drop(inner);
                    let color = self
                        .color
                        .unwrap_or_else(|| crate::ColorInfo::for_resolution(d.w, d.h));
                    if let Some(g) = gpu_frame_from_prime(desc, d, color) {
                        return Some(Frame::gpu(g));
                    }
                    // Export succeeded but its shape is not one we can describe
                    // (multi-object, or no layer). Nothing is wrong with the
                    // decode, so read back rather than lose the frame — but say
                    // so once, because it means zero-copy is silently off.
                    log::warn!("wandr-video: vaapi: export shape unusable — reading back instead");
                    self.export = ExportState::ReadsBack;
                    return None;
                }
                Err(e) => {
                    log::info!(
                        "wandr-video: vaapi: export_prime unavailable ({e:?}) — readback path"
                    );
                    self.export = ExportState::ReadsBack;
                }
            }
        }

        // Readback path: identical to what this backend always did.
        let inner = d.handle.borrow();
        match readback_i420(inner.surface(), d.w, d.h, d.pts_us) {
            Ok(f) => {
                drop(inner);
                self.current = Some(f);
            }
            Err(e) => {
                log::warn!("wandr-video: vaapi: readback: {e:?}");
                return None;
            }
        }
        let f = self.current.as_ref()?;
        let (w, h) = (f.w, f.h);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y_len = (w * h) as usize;
        let c_len = (cw * ch) as usize;
        let color = self.color.unwrap_or_else(|| crate::ColorInfo::for_resolution(w, h));
        Some(Frame::cpu(I420Ref {
            y: &f.buf[..y_len],
            y_stride: w,
            u: &f.buf[y_len..y_len + c_len],
            u_stride: cw,
            v: &f.buf[y_len + c_len..y_len + 2 * c_len],
            v_stride: cw,
            width: w,
            height: h,
            timestamp_us: f.pts_us,
        }))
    }

}
