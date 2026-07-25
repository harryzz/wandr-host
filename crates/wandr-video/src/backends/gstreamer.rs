//! GStreamer decode backend — one library, every desktop OS's HW+SW decoders.
//!
//! WHY: instead of hand-rolling a per-OS HW backend (d3d11/vaapi/videotoolbox) plus
//! four bundled software codecs, GStreamer's `decodebin`/element zoo already wraps
//! every OS decoder. This backend feeds the guest's already-demuxed access units into
//! `appsrc -> parser -> <decoder> -> videoconvert -> appsink` and hands back I420.
//! Proven feasible incl. zero-copy GL in `repros/gstreamer-spike` (see
//! `[[reference_gstreamer_desktop_backend_spike]]`).
//!
//! CODEC-LEVEL, not player-level: we deliberately do NOT use `playbin3`. Demux,
//! transport (play/stop/seek/rate) and subtitles stay GUEST-side, exactly like every
//! other backend here — the host only decodes chunks. So no `gstreamer-play`, no
//! host-side player state.
//!
//! HW vs SW is chosen by INSTANTIATING the specific element (e.g. `vah264dec` vs
//! `avdec_h264`), never by mutating global registry ranks — so two decoders (one HW,
//! one SW) can coexist in one process. The registry sees two backends, `gstreamer-hw`
//! (Hardware, prio 15) and `gstreamer-sw` (Software, prio 90), sharing this impl.
//!
//! DESKTOP ONLY: never registered on Android (its HW path is JNI MediaCodec, which the
//! `--no-art` stack can't run — Android keeps the NDK `AMediaCodec` backend).
//!
//! OUTPUT LANES: CPU I420 readback (universal), plus per-OS ZERO-COPY `Frame::gpu`
//! into the host's existing importer — Linux dma-buf (`memory:DMABuf`, VA decoders)
//! and Windows D3D11 texture (`memory:D3D11Memory`, decoded on ANGLE's device, copied
//! to a SHADER_RESOURCE texture; see `mod d3d11_gpu`). Both default-on where the driver
//! supports it (`gpu_lane_available`), `WANDR_VIDEO_GST_GPU=0` forces readback. macOS
//! IOSurface (VideoToolbox) is the remaining lane.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ::gstreamer as gst; // leading `::` = the extern crate, not this `backends::gstreamer` module
use ::gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::VideoFrameExt;

use crate::{
    BackendKind, Codec, CodecBackend, CodecError, Decoder, DecoderParams, Encoder, EncoderParams, Frame,
    I420Ref,
};
#[cfg(target_os = "linux")]
use crate::{ColorInfo, DmabufPlane, GpuFrame, GpuFrameOwner};
#[cfg(all(target_os = "windows", feature = "d3d11"))]
use crate::{ColorInfo, GpuFrame, GpuFrameOwner};

// ── backend descriptor (one struct, two registered instances) ─────────────────

pub struct GStreamerBackend {
    pub hardware: bool,
}

impl CodecBackend for GStreamerBackend {
    fn name(&self) -> &'static str {
        if self.hardware { "gstreamer-hw" } else { "gstreamer-sw" }
    }
    fn kind(&self) -> BackendKind {
        if self.hardware { BackendKind::Hardware } else { BackendKind::Software }
    }
    fn priority(&self) -> u32 {
        // HW just below the native d3d11/vaapi backends (~10) so those win when both
        // are compiled; SW below the bundled software codecs (~50-100) so it's the
        // last-resort universal fallback. Tune once both coexist.
        if self.hardware { 15 } else { 95 }
    }
    fn supports_decode(&self, codec: Codec) -> bool {
        ensure_init();
        pick_decoder(codec, self.hardware).is_some()
    }
    fn supports_encode(&self, _codec: Codec) -> bool {
        false
    }
    fn open_decoder(&self, p: &DecoderParams) -> Result<Box<dyn Decoder>, CodecError> {
        Ok(Box::new(GstDecoder::new(p, self.hardware)?))
    }
    fn open_encoder(&self, _p: &EncoderParams) -> Result<Box<dyn Encoder>, CodecError> {
        Err(CodecError::Unsupported)
    }
}

fn ensure_init() {
    // gst::init() is idempotent and cheap after the first call.
    let _ = gst::init();
}

/// Pick the first AVAILABLE decoder element for this codec + HW/SW lane.
fn pick_decoder(codec: Codec, hardware: bool) -> Option<&'static str> {
    let candidates: &[&str] = match (codec, hardware) {
        (Codec::H264, false) => &["avdec_h264"],
        (Codec::H264, true) => &["vah264dec", "vaapih264dec", "d3d11h264dec", "nvh264dec", "vtdec"],
        (Codec::H265, false) => &["avdec_h265", "libde265dec"],
        (Codec::H265, true) => &["vah265dec", "vaapih265dec", "d3d11h265dec", "nvh265dec", "vtdec"],
        (Codec::Vp8, false) => &["avdec_vp8", "vp8dec"],
        (Codec::Vp8, true) => &["vavp8dec", "vaapivp8dec"],
        (Codec::Vp9, false) => &["avdec_vp9", "vp9dec"],
        (Codec::Vp9, true) => &["vavp9dec", "vaapivp9dec", "d3d11vp9dec", "nvvp9dec"],
        (Codec::Av1, false) => &["dav1ddec", "avdec_av1", "av1dec"],
        (Codec::Av1, true) => &["vaav1dec", "d3d11av1dec", "nvav1dec"],
    };
    candidates.iter().copied().find(|n| gst::ElementFactory::find(n).is_some())
}

/// Does this decoder's SRC pad template advertise `video/x-raw(memory:DMABuf)`?
/// The `va` decoders list it exactly when the driver can export dma-buf (proven by
/// `gst-inspect vah264dec`), so this is a data-free capability probe — it decides the
/// zero-copy GPU lane without instantiating the element or pushing a single frame, and
/// without the "capability bits lie" risk that forced vaapi.rs to latch at runtime.
#[cfg(target_os = "linux")]
fn decoder_exports_dmabuf(element: &str) -> bool {
    let Some(factory) = gst::ElementFactory::find(element) else { return false };
    factory.static_pad_templates().iter().any(|t| {
        t.direction() == gst::PadDirection::Src
            && t.caps().iter_with_features().any(|(_, feats)| feats.contains("memory:DMABuf"))
    })
}

/// Is a zero-copy GPU output lane available for this decoder on this OS? Linux =
/// the decoder exports dma-buf (data-free pad-template probe). Windows = a D3D11 HW
/// decoder + the host handed us ANGLE's device (`set_angle_d3d11_device`), so the
/// decoded D3D11 texture can be copied into an ANGLE-importable one. Otherwise the
/// I420 readback lane is used.
fn gpu_lane_available(element: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        decoder_exports_dmabuf(element)
    }
    #[cfg(all(target_os = "windows", feature = "d3d11"))]
    {
        let _ = element;
        d3d11_gpu::win_gpu_available()
    }
    #[cfg(not(any(target_os = "linux", all(target_os = "windows", feature = "d3d11"))))]
    {
        let _ = element;
        false
    }
}

fn encoded_caps(codec: Codec) -> &'static str {
    match codec {
        // Guest feeds Annex-B access units (SPS/PPS in-band at keyframes).
        Codec::H264 => "video/x-h264,stream-format=byte-stream,alignment=au",
        Codec::H265 => "video/x-h265,stream-format=byte-stream,alignment=au",
        Codec::Vp8 => "video/x-vp8",
        Codec::Vp9 => "video/x-vp9",
        Codec::Av1 => "video/x-av1",
    }
}

fn parser_prefix(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264parse ! ",
        Codec::H265 => "h265parse ! ",
        Codec::Av1 => "av1parse ! ",
        Codec::Vp8 | Codec::Vp9 => "",
    }
}

// ── decoder ──────────────────────────────────────────────────────────────────

struct DecodedFrame {
    buf: Vec<u8>, // tightly packed I420
    w: u32,
    h: u32,
    pts_us: i64,
}

pub struct GstDecoder {
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
    /// Decoded samples, pushed by the appsink `new-sample` CALLBACK as GStreamer
    /// produces them (async, on the streaming thread). `next_frame` just pops here.
    /// This is the documented appsink pattern (examples/src/bin/appsink.rs): it
    /// decouples GStreamer's ASYNCHRONOUS decode from the SYNCHRONOUS pull the
    /// caller expects — a plain `try_pull_sample` right after `decode()` misses
    /// the not-yet-produced frame and the playback lane deadlocks.
    samples: Arc<Mutex<VecDeque<gst::Sample>>>,
    /// Set by the appsink EOS callback so `flush` knows the tail is fully queued.
    eos: Arc<AtomicBool>,
    current: Option<DecodedFrame>,
    /// Emit a zero-copy `Frame::gpu` (dma-buf on Linux, D3D11 texture on Windows)
    /// instead of CPU I420. HW lanes only.
    gpu: bool,
    /// Wrapped GstD3D11Device (== ANGLE's device) for the Windows zero-copy lane —
    /// held so `build_gpu_frame_d3d11` can lock it and read its device/context, and
    /// unref'd in `Drop`. Null on the readback lane. (Windows/d3d11 only.)
    #[cfg(all(target_os = "windows", feature = "d3d11"))]
    d3d11_dev: *mut std::ffi::c_void,
}

/// Repack a decoded I420 sample into a tightly-packed, owned frame (GStreamer plane
/// strides may be padded; the I420Ref we hand out must be tight).
fn sample_to_decoded(sample: &gst::Sample) -> Option<DecodedFrame> {
    let buffer = sample.buffer()?;
    let caps = sample.caps()?;
    let info = gst_video::VideoInfo::from_caps(caps).ok()?;
    let vframe = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
    let (w, h) = (info.width() as usize, info.height() as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut buf = Vec::with_capacity(w * h + 2 * cw * ch);
    for (plane, pw, ph) in [(0usize, w, h), (1, cw, ch), (2, cw, ch)] {
        let stride = vframe.plane_stride()[plane] as usize;
        let data = vframe.plane_data(plane as u32).ok()?;
        for row in 0..ph {
            buf.extend_from_slice(&data[row * stride..row * stride + pw]);
        }
    }
    let pts_us = buffer.pts().map(|t| (t.nseconds() / 1000) as i64).unwrap_or(0);
    Some(DecodedFrame { buf, w: w as u32, h: h as u32, pts_us })
}

// ── zero-copy GPU output (Linux dma-buf) ──────────────────────────────────────
// GStreamer's VA decoders export the decoded surface as a DMA-buf, the SAME currency
// GpuFrame already carries (fourcc + DRM modifier + planes) and the host's existing
// dma-buf->EGLImage compositor already imports — so this reuses vaapi.rs's whole
// consumer path, no host changes. Windows/macOS GPU output (D3D11 texture / IOSurface)
// is a separate lane, not built here.
#[cfg(target_os = "linux")]
struct GstDmabufOwner {
    sample: gst::Sample, // keeps the buffer (and its dma-buf fds) alive
    modifier: u64,
}
#[cfg(target_os = "linux")]
unsafe impl Send for GstDmabufOwner {}
#[cfg(target_os = "linux")]
impl GpuFrameOwner for GstDmabufOwner {
    fn read_i420(&self, out: &mut Vec<u8>) -> Result<(), CodecError> {
        // CPU-readback fallback lane. Correct only for LINEAR (modifier 0) dma-buf:
        // a tiled surface (Intel Y-tiled) maps to tiled bytes, which we don't detile
        // here — the host uses the zero-copy EGLImage import (modifier-aware) for those.
        if self.modifier != 0 {
            return Err(CodecError::Unsupported);
        }
        let buffer = self.sample.buffer().ok_or(CodecError::BadFrame)?;
        let caps = self.sample.caps().ok_or(CodecError::BadFrame)?;
        let info = gst_video::VideoInfo::from_caps(caps).map_err(|_| CodecError::BadFrame)?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
            .map_err(|_| CodecError::BadFrame)?;
        let (w, h) = (info.width() as usize, info.height() as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        out.clear();
        out.reserve(w * h + 2 * cw * ch);
        // NV12 -> I420: Y copied tight; interleaved UV split into U then V.
        let y_stride = frame.plane_stride()[0] as usize;
        let y = frame.plane_data(0).map_err(|_| CodecError::BadFrame)?;
        for row in 0..h {
            out.extend_from_slice(&y[row * y_stride..row * y_stride + w]);
        }
        let uv_stride = frame.plane_stride()[1] as usize;
        let uv = frame.plane_data(1).map_err(|_| CodecError::BadFrame)?;
        for plane_sel in [0usize, 1] {
            for row in 0..ch {
                let base = row * uv_stride;
                for col in 0..cw {
                    out.push(uv[base + col * 2 + plane_sel]);
                }
            }
        }
        Ok(())
    }
}

/// Build a zero-copy dma-buf `GpuFrame` from a decoded sample, or `None` if the
/// sample isn't dma-buf backed (e.g. software decoder output).
#[cfg(target_os = "linux")]
fn build_gpu_frame(sample: &gst::Sample) -> Option<GpuFrame> {
    use std::os::fd::BorrowedFd;
    let buffer = sample.buffer()?;
    let caps = sample.caps()?;
    // DMABuf caps carry format=DMA_DRM + drm-format="NV12:0x<modifier>"; width/height
    // are plain fields. Parse the fourcc+modifier the host's importer needs.
    let s = caps.structure(0)?;
    let w = s.get::<i32>("width").ok()? as u32;
    let h = s.get::<i32>("height").ok()? as u32;
    // drm-format = "<4cc>:0x<modifier>", e.g. "NV12:0x0100000000000002". The 4cc packs
    // little-endian into a DRM fourcc (matches EGL_LINUX_DRM_FOURCC_EXT + vaapi.rs).
    let drm_format = s.get::<String>("drm-format").ok()?;
    let (fcc, modh) = drm_format.split_once(':')?;
    let fb = fcc.as_bytes();
    if fb.len() != 4 {
        return None;
    }
    let fourcc =
        fb[0] as u32 | (fb[1] as u32) << 8 | (fb[2] as u32) << 16 | (fb[3] as u32) << 24;
    let modifier = u64::from_str_radix(modh.trim_start_matches("0x"), 16).ok()?;
    let vmeta = buffer.meta::<gst_video::VideoMeta>()?;
    let n_planes = vmeta.n_planes() as usize;
    let offsets = vmeta.offset();
    let strides = vmeta.stride();
    let n_mem = buffer.n_memory() as usize;

    let mut planes = Vec::with_capacity(n_planes);
    for i in 0..n_planes {
        // Single-fd NV12 puts both planes in memory 0 at different offsets; some
        // drivers use one memory per plane. Handle both.
        let mem_idx = if n_mem == n_planes { i } else { 0 };
        let mem = buffer.peek_memory(mem_idx);
        let dmabuf = mem.downcast_memory_ref::<gstreamer_allocators::DmaBufMemory>()?;
        let raw = dmabuf.fd();
        // Dup the fd so DmabufPlane owns/closes an independent one (EGL keeps its own).
        let owned = unsafe { BorrowedFd::borrow_raw(raw) }.try_clone_to_owned().ok()?;
        planes.push(DmabufPlane {
            fd: std::fs::File::from(owned),
            offset: offsets[i] as u32,
            pitch: strides[i] as u32,
        });
    }
    let pts_us = buffer.pts().map(|t| (t.nseconds() / 1000) as i64).unwrap_or(0);
    let color = ColorInfo::for_resolution(w, h);
    Some(GpuFrame::new(
        w,
        h,
        pts_us,
        fourcc,
        modifier,
        planes,
        color,
        Box::new(GstDmabufOwner { sample: sample.clone(), modifier }),
    ))
}

// The pipeline + app elements are only ever touched from the store thread (the
// wasmtime Store is single-threaded); GStreamer's own worker threads are internal.
unsafe impl Send for GstDecoder {}

impl GstDecoder {
    fn new(p: &DecoderParams, hardware: bool) -> Result<Self, CodecError> {
        ensure_init();
        let element = pick_decoder(p.codec, hardware).ok_or(CodecError::Unsupported)?;

        // libav (`avdec_*`) defaults to FRAME threading, which holds ~N-cores frames
        // and — in this appsrc pipeline — does NOT flush that tail on EOS, silently
        // dropping the last ~10 frames of a clip. SLICE threading has no cross-frame
        // delay (so nothing is lost at end-of-stream) while still parallelizing across
        // a frame's slices; correctness over peak throughput, still well above realtime.
        // HW decoders don't use libav threading, so they're left untouched.
        let decoder = if element.starts_with("avdec") {
            format!("{element} thread-type=slice")
        } else {
            element.to_string()
        };
        // GPU output: HW decoders on Linux export a dma-buf we hand straight to the
        // host compositor (zero-copy — ~4% CPU vs ~30% for the I420 readback). ON by
        // default (like vaapi's zero-copy), gated on the decoder ACTUALLY advertising
        // DMABuf in its src pad template — a data-free, driver-accurate probe — so a GPU
        // whose driver can't export dma-buf transparently stays on the I420 readback with
        // no risky mid-stream fallback. WANDR_VIDEO_GST_GPU=0 forces the readback path.
        let gpu = hardware
            && std::env::var("WANDR_VIDEO_GST_GPU").map_or(true, |v| v != "0")
            && gpu_lane_available(element);
        // GPU: keep the decoder's native GPU memory (no videoconvert) — dma-buf on
        // Linux, D3D11 texture on Windows. CPU: convert to I420 (readback).
        let tail = if !gpu {
            "videoconvert ! video/x-raw,format=I420 ! \
             appsink name=sink sync=false max-buffers=16 drop=false"
        } else if cfg!(target_os = "windows") {
            "appsink name=sink caps=\"video/x-raw(memory:D3D11Memory),format=NV12\" sync=false max-buffers=16 drop=false"
        } else {
            "appsink name=sink caps=\"video/x-raw(memory:DMABuf),format=DMA_DRM\" sync=false max-buffers=16 drop=false"
        };
        let launch = format!(
            "appsrc name=src is-live=false format=time do-timestamp=false caps=\"{caps}\" ! \
             {parse}{decoder} ! {tail}",
            caps = encoded_caps(p.codec),
            parse = parser_prefix(p.codec),
        );
        let pipeline = gst::parse::launch(&launch)
            .map_err(|e| {
                log::warn!("wandr-video: gstreamer pipeline parse ({element}): {e}");
                CodecError::InitFailed
            })?
            .downcast::<gst::Pipeline>()
            .map_err(|_| CodecError::InitFailed)?;

        let appsrc = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .ok_or(CodecError::InitFailed)?;
        let appsink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or(CodecError::InitFailed)?;

        // dma-buf zero-copy REQUIRES the sink to advertise GstVideoMeta support in the
        // ALLOCATION query — otherwise the `va` decoder refuses to export and fails with
        // "DMABuf caps negotiated without the mandatory support of VideoMeta"
        // (gstvabasedec.c). appsink's default allocation handling adds nothing, so we
        // intercept the downstream allocation query on its sink pad and add the meta.
        // Only the GPU lane needs it; the I420 readback lane maps the frame on the CPU.
        if gpu {
            let pad = appsink.static_pad("sink").ok_or(CodecError::InitFailed)?;
            pad.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, |_pad, info| {
                if let Some(gst::PadProbeData::Query(q)) = info.data.as_mut() {
                    if let gst::QueryViewMut::Allocation(alloc) = q.view_mut() {
                        alloc.add_allocation_meta::<gst_video::VideoMeta>(None);
                        return gst::PadProbeReturn::Handled;
                    }
                }
                gst::PadProbeReturn::Ok
            });
        }

        // Async delivery: the callback runs when a frame is ready and queues it.
        let samples: Arc<Mutex<VecDeque<gst::Sample>>> = Arc::new(Mutex::new(VecDeque::new()));
        let eos = Arc::new(AtomicBool::new(false));
        {
            let q = samples.clone();
            let e = eos.clone();
            appsink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        q.lock().unwrap().push_back(sample);
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .eos(move |_sink| e.store(true, Ordering::Relaxed))
                    .build(),
            );
        }

        // Windows GPU lane: force the d3d11 decoder onto ANGLE's device BEFORE PLAYING,
        // so its output NV12 texture is a same-device alias the host imports (mirrors
        // d3d11.rs's set_angle_d3d11_device handoff). Held for build_gpu_frame_d3d11 +
        // unref'd in Drop. Injection failing after win_gpu_available() said yes is a hard
        // error (bad device) — let the registry fall back rather than decode into limbo.
        #[cfg(all(target_os = "windows", feature = "d3d11"))]
        let d3d11_dev = {
            let dev = if gpu { unsafe { d3d11_gpu::inject_angle_device(&pipeline) } } else { None };
            if gpu && dev.is_none() {
                log::warn!("wandr-video: gstreamer d3d11 device injection failed");
                return Err(CodecError::InitFailed);
            }
            dev.unwrap_or(std::ptr::null_mut())
        };

        pipeline.set_state(gst::State::Playing).map_err(|e| {
            log::warn!("wandr-video: gstreamer set PLAYING: {e}");
            CodecError::InitFailed
        })?;
        log::info!("wandr-video: gstreamer decode via {element} ({})", if hardware { "HARDWARE" } else { "software" });

        Ok(Self {
            pipeline,
            appsrc,
            samples,
            eos,
            current: None,
            gpu,
            #[cfg(all(target_os = "windows", feature = "d3d11"))]
            d3d11_dev,
        })
    }
}

impl Decoder for GstDecoder {
    fn decode(&mut self, chunk: crate::Chunk<'_>) -> Result<(), CodecError> {
        let mut buffer = gst::Buffer::from_mut_slice(chunk.data.to_vec());
        {
            let b = buffer.get_mut().unwrap();
            // Container PTS (µs) rides through; negatives (rare, B-frame reorder in
            // some containers) clamp to 0 — display order is what next_frame returns.
            b.set_pts(gst::ClockTime::from_useconds(chunk.timestamp_us.max(0) as u64));
        }
        match self.appsrc.push_buffer(buffer) {
            Ok(_) => Ok(()),
            // Flushing/EOS after a reset is back-pressure, not a hard error.
            Err(gst::FlowError::Flushing) | Err(gst::FlowError::Eos) => Ok(()),
            Err(e) => {
                log::warn!("wandr-video: gstreamer push_buffer: {e:?}");
                Err(CodecError::BadFrame)
            }
        }
    }

    fn flush(&mut self) -> Result<(), CodecError> {
        // End-of-stream: signal EOS, then wait for the pipeline to actually reach it
        // so the new-sample callback has queued the whole reorder/thread-delay tail.
        // next_frame then serves it from `samples`.
        let _ = self.appsrc.end_of_stream();
        let start = Instant::now();
        while !self.eos.load(Ordering::Relaxed) && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<(), CodecError> {
        // Seek: flush the pipeline so the decoder drops reference frames; the caller
        // feeds a keyframe next. flush-stop restarts the data flow with a fresh segment.
        let _ = self.appsrc.send_event(gst::event::FlushStart::new());
        let _ = self.appsrc.send_event(gst::event::FlushStop::new(true));
        self.samples.lock().unwrap().clear();
        self.eos.store(false, Ordering::Relaxed);
        self.current = None;
        Ok(())
    }

    fn next_frame(&mut self) -> Option<Frame<'_>> {
        // Pop the next sample the callback queued (non-blocking — it's already there
        // if GStreamer has produced it). No polling of the pipeline.
        let sample = self.samples.lock().unwrap().pop_front()?;

        // GPU lane: hand back the dma-buf as a zero-copy Frame::gpu (owns the sample).
        #[cfg(target_os = "linux")]
        if self.gpu {
            return build_gpu_frame(&sample).map(Frame::gpu);
        }

        // Windows GPU lane: copy the decoded D3D11 texture into an ANGLE-importable one.
        #[cfg(all(target_os = "windows", feature = "d3d11"))]
        if self.gpu {
            return unsafe { d3d11_gpu::build_gpu_frame_d3d11(&sample, self.d3d11_dev) }.map(Frame::gpu);
        }

        // CPU lane: repack to tight I420 held in `current` so the borrow stays valid.
        let df = sample_to_decoded(&sample)?;
        self.current = Some(df);

        let f = self.current.as_ref()?;
        let (w, h) = (f.w, f.h);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y_len = (w * h) as usize;
        let c_len = (cw * ch) as usize;
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

impl Drop for GstDecoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        // Release our ref on the wrapped ANGLE GstD3D11Device (the pipeline/context held
        // its own, dropped when `pipeline` drops right after this).
        #[cfg(all(target_os = "windows", feature = "d3d11"))]
        if !self.d3d11_dev.is_null() {
            unsafe { gst::ffi::gst_object_unref(self.d3d11_dev as *mut gst::ffi::GstObject) };
        }
    }
}

// ── Windows zero-copy GPU output (D3D11 texture) ─────────────────────────────
// The peer of the Linux dma-buf lane. GStreamer's `d3d11h26xdec` decodes into an
// ID3D11Texture2D (a pool array). We force it onto ANGLE's D3D11 device (so the
// texture is a same-device alias, exactly like d3d11.rs), then copy the decoded
// slice into a fresh single SHADER_RESOURCE|RENDER_TARGET NV12 texture — the shape
// the host's `import_d3d11` (video_gl.rs) already imports via EGL_D3D11_TEXTURE_ANGLE.
// So this reuses d3d11.rs's whole host consumer path with NO host changes, and the
// array-slice never reaches the host (we copy to slice 0), mirroring d3d11::export_texture.
//
// GStreamer ships no Rust D3D11 binding, so the GstD3D11* calls are raw FFI against
// gstd3d11-1.0 (public API lib, `gstreamer-d3d11-1.0.pc`). D3D11 itself is the
// `windows` crate (already linked by the d3d11 feature).
#[cfg(all(target_os = "windows", feature = "d3d11"))]
mod d3d11_gpu {
    use std::ffi::c_void;

    use ::gstreamer as gst;
    use ::gstreamer::prelude::*;
    use ::gstreamer_video as gst_video;
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D11::{
        ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
        D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

    use crate::{ColorInfo, GpuFrame, GpuFrameOwner};

    // DRM fourcc 'NV12', for parity with the dma-buf/vaapi frames the host imports.
    const NV12_FOURCC: u32 = 0x3231_564e;

    // gstd3d11-1.0 public C API (no gstreamer-rs binding). Search path comes from the
    // gstreamer-sys build; d3d11/dxgi come from the `windows` crate's own linkage.
    #[link(name = "gstd3d11-1.0")]
    extern "C" {
        fn gst_d3d11_device_new_wrapped(device: *mut c_void) -> *mut c_void;
        fn gst_d3d11_context_new(device: *mut c_void) -> *mut c_void;
        fn gst_d3d11_device_get_device_handle(device: *mut c_void) -> *mut c_void;
        fn gst_d3d11_device_get_device_context_handle(device: *mut c_void) -> *mut c_void;
        fn gst_d3d11_device_lock(device: *mut c_void);
        fn gst_d3d11_device_unlock(device: *mut c_void);
        fn gst_is_d3d11_memory(mem: *mut c_void) -> i32;
        fn gst_d3d11_memory_get_resource_handle(mem: *mut c_void) -> *mut c_void;
        fn gst_d3d11_memory_get_subresource_index(mem: *mut c_void) -> u32;
    }

    /// GPU zero-copy is available iff the host handed us ANGLE's D3D11 device.
    pub(super) fn win_gpu_available() -> bool {
        crate::backends::d3d11::angle_d3d11_device().is_some()
    }

    /// Force the pipeline's d3d11 decoder onto ANGLE's device by publishing a
    /// `gst.d3d11.device.handle` context that wraps it, BEFORE the state change. Returns
    /// our owned GstD3D11Device* (unref in Drop), or None if ANGLE's device isn't set /
    /// wrapping fails.
    pub(super) unsafe fn inject_angle_device(pipeline: &gst::Pipeline) -> Option<*mut c_void> {
        let angle = crate::backends::d3d11::angle_d3d11_device()?; // *mut ID3D11Device
        let gst_dev = gst_d3d11_device_new_wrapped(angle);
        if gst_dev.is_null() {
            return None;
        }
        let ctx_ptr = gst_d3d11_context_new(gst_dev);
        if ctx_ptr.is_null() {
            gst::ffi::gst_object_unref(gst_dev as *mut gst::ffi::GstObject);
            return None;
        }
        let ctx: gst::Context = gst::glib::translate::from_glib_full(ctx_ptr as *mut gst::ffi::GstContext);
        pipeline.set_context(&ctx);
        log::info!("wandr-video: gstreamer d3d11 — decoding on ANGLE's device (zero-copy)");
        Some(gst_dev)
    }

    /// Owns the per-frame NV12 texture; hands the host a D3D11 view for zero-copy import
    /// or reads it back to I420 (fallback) — reusing d3d11.rs's readback.
    struct GstD3d11Owner {
        texture: ID3D11Texture2D,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        width: u32,
        height: u32,
    }
    // SAFETY: like every backend here — driven only from the single store thread.
    unsafe impl Send for GstD3d11Owner {}

    impl GpuFrameOwner for GstD3d11Owner {
        fn read_i420(&self, out: &mut Vec<u8>) -> Result<(), crate::CodecError> {
            let i420 = unsafe {
                crate::backends::d3d11::readback_nv12_texture(&self.device, &self.context, &self.texture, self.width, self.height)
            }
            .map_err(|_| crate::CodecError::BadFrame)?;
            out.clear();
            out.extend_from_slice(&i420);
            Ok(())
        }
        fn d3d11(&self) -> Option<crate::D3d11View> {
            Some(crate::D3d11View {
                texture: self.texture.clone(),
                device: self.device.clone(),
                array_slice: 0,
            })
        }
    }

    /// Extract the decoded D3D11 texture from `sample`, copy the frame's slice into a
    /// fresh ANGLE-importable NV12 texture, and wrap it as a `GpuFrame`.
    pub(super) unsafe fn build_gpu_frame_d3d11(sample: &gst::Sample, gst_dev: *mut c_void) -> Option<GpuFrame> {
        if gst_dev.is_null() {
            return None;
        }
        let buffer = sample.buffer()?;
        let mem = buffer.peek_memory(0);
        let mem_ptr = mem.as_ptr() as *mut c_void;
        if gst_is_d3d11_memory(mem_ptr) == 0 {
            return None;
        }
        let src_raw = gst_d3d11_memory_get_resource_handle(mem_ptr);
        if src_raw.is_null() {
            return None;
        }
        let subresource = gst_d3d11_memory_get_subresource_index(mem_ptr);

        // Device + immediate context GStreamer decoded on — == ANGLE's, since we wrapped it.
        let dev_raw = gst_d3d11_device_get_device_handle(gst_dev);
        let ctx_raw = gst_d3d11_device_get_device_context_handle(gst_dev);
        let device = ID3D11Device::from_raw_borrowed(&dev_raw)?;
        let context = ID3D11DeviceContext::from_raw_borrowed(&ctx_raw)?;
        let src = ID3D11Resource::from_raw_borrowed(&src_raw)?;

        let caps = sample.caps()?;
        let info = gst_video::VideoInfo::from_caps(caps).ok()?;
        let (w, h) = (info.width(), info.height());

        // Fresh single NV12 texture the host can sample (SHADER_RESOURCE) and ANGLE can
        // render-target per plane (mirrors d3d11.rs::export_texture's flags).
        let tdesc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&tdesc, None, Some(&mut tex)).ok()?;
        let texture = tex?;

        // Same-device GPU copy, serialized against GStreamer's own use of the shared
        // device context (decode runs on GStreamer's streaming thread).
        gst_d3d11_device_lock(gst_dev);
        context.CopySubresourceRegion(&texture, 0, 0, 0, 0, src, subresource, None);
        gst_d3d11_device_unlock(gst_dev);

        let pts_us = buffer.pts().map(|t| (t.nseconds() / 1000) as i64).unwrap_or(0);
        let owner = GstD3d11Owner {
            texture: texture.clone(),
            device: device.clone(),
            context: context.clone(),
            width: w,
            height: h,
        };
        Some(GpuFrame::new(
            w,
            h,
            pts_us,
            NV12_FOURCC,
            0, // no DRM modifier on Windows
            Vec::new(), // no dma-buf planes — the host uses d3d11() instead
            ColorInfo::for_resolution(w, h),
            Box::new(owner),
        ))
    }
}
