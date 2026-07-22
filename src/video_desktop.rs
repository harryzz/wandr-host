//! Desktop (non-Android) `wandr:video` backend — nokhwa camera capture + software
//! VP8/VP9 via `wandr-video` (statically-linked libvpx). The cross-platform peer
//! of the Android NDK-camera + AMediaCodec backend in `video.rs::android` (Linux
//! v4l2 / Windows MediaFoundation / macOS AVFoundation via nokhwa).
//!
//! This file owns everything that is NOT a codec — camera capture, the PiP
//! self-view, and the Skia compositing registry — and delegates encode/decode to
//! `wandr_video`. That split is why the codec crate can be unit-tested with a
//! synthetic gradient and no hardware.
//!
//! Task 117 replaced ffmpeg here. Two behavior changes fell out of it:
//!   * `set_bitrate` is REAL now (libvpx retunes rate control mid-stream), so the
//!     desktop encoder finally honors the guest's REMB/TWCC congestion control.
//!   * A camera frame whose size differs from the encode size is now RESIZED
//!     rather than dropped — the old code hard-skipped those frames.
//!
//! WSLg note: the RDP-forwarded virtual camera truncates large buffers, so
//! >640x480 tears; the call path uses 640x480, which is intact. Real cameras
//! (device/native) handle 720p+.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use nokhwa::Camera;

use wandr_video::{CodecError, DecoderParams, EncoderParams, Rgb24Frame};

use crate::video::{Codec, DecoderConfig, EncodedFrame, EncoderConfig, VideoError, VideoRect};

/// The codec crate's errors are narrower than the WIT surface (a codec cannot
/// fail with `surface-unavailable`), so widen at the boundary.
fn map_err(e: CodecError) -> VideoError {
    match e {
        CodecError::Unsupported => VideoError::NoHwCodec,
        CodecError::InitFailed => VideoError::CodecInitFailed,
        CodecError::BadFrame => VideoError::BadFrame,
    }
}

fn codec_of(c: Codec) -> wandr_video::Codec {
    match c {
        Codec::Vp8 => wandr_video::Codec::Vp8,
        Codec::Vp9 => wandr_video::Codec::Vp9,
        Codec::H264 => wandr_video::Codec::H264,
        Codec::H265 => wandr_video::Codec::H265,
        Codec::Av1 => wandr_video::Codec::Av1,
    }
}

// ── PiP self-view registry ───────────────────────────────────────────────────
// The encoder captures the LOCAL camera; the host composites that frame at the
// preview rect (the self-view). Encoder + render loop run on the same store
// thread, so a thread_local slot carries the latest RGBA frame + rect + visible
// across — no locking (mirrors audio_desktop's thread_local stream registry).
// Android instead composites via a SurfaceView child surface; this is the
// desktop analog, drawn onto the same Skia surface as the guest UI.
/// A composited video surface: the encoder's PiP self-view (mirrored) OR the
/// decoder's remote stream (upright, possibly rotated). Both draw onto the same
/// Skia surface as the guest UI (above-ui) — the desktop analog of Android's
/// SurfaceView child surfaces. z-layer (behind/above-ui) isn't distinguished yet:
/// everything composites above the UI.
struct VideoSurface {
    /// `None` until the first frame. GPU textures or CPU RGBA — see SurfaceContent.
    content: Option<SurfaceContent>,
    w: u32,
    h: u32,
    rect: VideoRect,
    visible: bool,
    /// Mirror horizontally — the front-camera self-view convention; false for
    /// remote video.
    mirror: bool,
    /// Degrees CW to rotate for upright display (the decoder's peer-CVO rotation;
    /// 0 for the self-view preview).
    rotation: u32,
}

thread_local! {
    static SURFACES: RefCell<HashMap<u32, VideoSurface>> = RefCell::new(HashMap::new());
    static SURFACE_NEXT: Cell<u32> = const { Cell::new(1) };
}

/// Composite every visible video surface onto `canvas` — called by the
/// wasi:canvas host `present` AFTER the guest UI (above-ui) and before swap.
/// Rects are absolute surface pixels.

/// Turn a surface's content into something Skia can draw.
///
/// GPU frames need a `DirectContext`, which `Canvas::direct_context()` supplies
/// — and returns `None` on the headless RASTER surface that every video
/// diagnostic and `--run-once` use. That `None` is exactly the runtime gate: no
/// GPU context, no GPU path, and the CPU lane serves as it always did.
fn image_for(
    canvas: &skia_safe::Canvas,
    content: &SurfaceContent,
    w: u32,
    h: u32,
) -> Option<skia_safe::Image> {
    match content {
        SurfaceContent::Rgba(rgba) => {
            let info = skia_safe::ImageInfo::new(
                (w as i32, h as i32),
                skia_safe::ColorType::RGBA8888,
                skia_safe::AlphaType::Unpremul,
                None,
            );
            let data = skia_safe::Data::new_copy(rgba);
            skia_safe::images::raster_from_data(&info, data, (w * 4) as usize)
        }
        SurfaceContent::Texture(tref) => {
            let mut ctx = canvas.direct_context()?;
            let (y_tex, uv_tex) =
                TEXTURES.with(|m| m.borrow().get(&tref.0).map(|t| (t.y_tex, t.uv_tex)))?;
            // NV12 as two planes: R8 luma, RG8 interleaved chroma. Skia does the
            // YUV->RGB itself from the YUVAInfo we hand it — which is the whole
            // reason for the two-texture shape. The alternative
            // (GL_TEXTURE_EXTERNAL_OES) can only pass colour as an EGL *hint*
            // that drivers ignore, defaulting to BT.601 limited whatever the
            // content is.
            const GL_TEXTURE_2D: u32 = 0x0DE1;
            const GL_R8: u32 = 0x8229;
            const GL_RG8: u32 = 0x822B;
            let planes = [(y_tex, GL_R8, w, h), (uv_tex, GL_RG8, w.div_ceil(2), h.div_ceil(2))];
            let textures: Vec<skia_safe::gpu::BackendTexture> = planes
                .iter()
                .map(|&(id, fmt, pw, ph)| {
                    let info = skia_safe::gpu::gl::TextureInfo {
                        target: GL_TEXTURE_2D,
                        id,
                        format: fmt,
                        protected: skia_safe::gpu::Protected::No,
                    };
                    // SAFETY: `t` owns these texture names and outlives the image
                    // we build from them (it is held by the surface).
                    unsafe {
                        skia_safe::gpu::ganesh::gl::backend_textures::make_gl(
                            (pw as i32, ph as i32),
                            skia_safe::gpu::Mipmapped::No,
                            info,
                            "wandr-video-plane",
                        )
                    }
                })
                .collect();

            // ‼️ BT.601 LIMITED is the H.264 SD/HD default and what these VLD
            // decoders emit; it also matches `convert.rs`, so the two lanes agree.
            // The RIGHT source is the stream's VUI (colour_primaries /
            // matrix_coefficients / video_full_range_flag) — carrying that through
            // is a follow-up, and until then this is a documented assumption
            // rather than a silent one.
            let yuva = skia_safe::YUVAInfo::new(
                (w as i32, h as i32),
                skia_safe::yuva_info::PlaneConfig::Y_UV,
                skia_safe::yuva_info::Subsampling::S420,
                skia_safe::YUVColorSpace::Rec601_Limited,
                None,
                None,
            )?;
            let backend = skia_safe::gpu::YUVABackendTextures::new(
                &yuva,
                &textures,
                skia_safe::gpu::SurfaceOrigin::TopLeft,
            )?;
            let img = skia_safe::gpu::ganesh::images::texture_from_yuva_textures(
                &mut ctx, &backend, None,
            );
            // Skia caches GL binding state; our raw GL in video_gl.rs bound and
            // unbound textures behind its back, so tell it to re-read. Skipping
            // this makes the NEXT Skia draw sample whatever we left bound —
            // intermittent, and invisible to every frame counter. The targeted
            // call rather than a full `reset`, because texture bindings are the
            // only state video_gl touches.
            ctx.reset_gl_texture_bindings();
            img
        }
    }
}

pub fn composite_video_surfaces(canvas: &skia_safe::Canvas) {
    // Paint any guest-scheduled frames that have come due (task 117 M2). This
    // runs before compositing so a frame scheduled for "now" appears in THIS
    // host frame rather than the next one.
    drain_scheduled();
    SURFACES.with(|m| {
        for s in m.borrow().values() {
            let Some(content) = s.content.as_ref() else { continue };
            if !s.visible || content.is_empty() || s.rect.w <= 0 || s.rect.h <= 0 {
                continue;
            }
            let Some(img) = image_for(canvas, content, s.w, s.h) else { continue };
            let dst = skia_safe::Rect::from_xywh(
                s.rect.x as f32, s.rect.y as f32, s.rect.w as f32, s.rect.h as f32,
            );
            let mut paint = skia_safe::Paint::default();
            paint.set_anti_alias(true);
            canvas.save();
            canvas.reset_matrix();
            // Peer CVO rotation, about the rect centre (no-op for the preview).
            if s.rotation % 360 != 0 {
                canvas.rotate(
                    s.rotation as f32,
                    Some(skia_safe::Point::new(dst.center_x(), dst.center_y())),
                );
            }
            if s.mirror {
                // Mirror horizontally in place: x → (left+right) − x.
                canvas.translate((dst.left + dst.right, 0.0));
                canvas.scale((-1.0, 1.0));
            }
            canvas.draw_image_rect(&img, None, dst, &paint);
            canvas.restore();
        }
    });
}

fn alloc_surface(rect: VideoRect, mirror: bool, rotation: u32) -> u32 {
    let id = SURFACE_NEXT.with(|n| {
        let v = n.get();
        n.set(v.wrapping_add(1).max(1));
        v
    });
    SURFACES.with(|m| {
        m.borrow_mut().insert(id, VideoSurface {
            content: None, w: 0, h: 0, rect, visible: true, mirror, rotation,
        });
    });
    id
}

/// Update a surface's pixels (from the encoder capture / decoder output).
fn surface_set_frame(id: u32, rgba: Vec<u8>, w: u32, h: u32) {
    surface_set_content(id, SurfaceContent::Rgba(rgba), w, h);
}

/// ‼️ Silently no-ops on an unknown id, and `SURFACES` is THREAD-LOCAL — so a
/// decoder driven from another thread would make video vanish with no error.
/// `VideoDecoder` is `Send` (the wasmtime ResourceTable requires it), so nothing
/// stops that at compile time. The host runs single-threaded on the winit loop;
/// this comment is the guard rail.
fn surface_set_content(id: u32, content: SurfaceContent, w: u32, h: u32) {
    SURFACES.with(|m| {
        if let Some(s) = m.borrow_mut().get_mut(&id) {
            s.content = Some(content);
            s.w = w;
            s.h = h;
        }
    });
}

fn surface_with<F: FnOnce(&mut VideoSurface)>(id: u32, f: F) {
    SURFACES.with(|m| {
        if let Some(s) = m.borrow_mut().get_mut(&id) {
            f(s);
        }
    });
}

fn surface_remove(id: u32) {
    SURFACES.with(|m| {
        m.borrow_mut().remove(&id);
    });
}

/// No binder off-Android (the Android path spins up an rsbinder threadpool for
/// the camera/codec HAL; desktop nokhwa/libvpx need none).
pub fn ensure_binder_threadpool() -> bool {
    false
}

// ── encoder ──────────────────────────────────────────────────────────────────

/// The latest camera frame the background capture thread produced (tightly-packed
/// RGB24). Read + consumed by `capture_encode` on the store thread.
struct LatestFrame {
    rgb: Vec<u8>,
    w: u32,
    h: u32,
    seq: u64,
}

/// Move-into-thread wrapper for the !Send nokhwa `Camera`. The camera is owned
/// and touched ONLY by the capture thread, so crossing the spawn boundary once
/// (and never sharing it) is sound.
struct SendCamera(Camera);
unsafe impl Send for SendCamera {}

pub struct VideoEncoder {
    /// Background capture thread — owns the `Camera` and blocks on its `frame()`
    /// OFF the store thread, publishing the newest decoded RGB frame into
    /// `latest`. This is the Signal self-view freeze fix: the render/pump loop
    /// reads `latest` non-blocking and never stalls on the camera.
    cam_thread: Option<std::thread::JoinHandle<()>>,
    cam_stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<LatestFrame>>>,
    /// Sequence of the last frame we encoded — skip re-encoding a stale frame.
    last_seq: u64,
    enc: Box<dyn wandr_video::Encoder>,
    force_keyframe: bool,
    /// PiP self-view surface (Some iff opened with a preview rect).
    preview_id: Option<u32>,
    /// Reused RGB→RGBA staging for the self-view, so steady state doesn't allocate.
    pip: Vec<u8>,
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        // Stop the capture thread and wait for it (it exits within ~1 camera
        // frame once the flag is set; dropping the Camera closes the stream).
        self.cam_stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.cam_thread.take() {
            let _ = h.join();
        }
        if let Some(id) = self.preview_id {
            surface_remove(id);
        }
    }
}

// NOTE: no `unsafe impl Send` here any more. The old one existed only because
// ffmpeg's Context is !Send; `Box<dyn wandr_video::Encoder>` is Send because the
// trait requires it, and the codec backend justifies that itself.

impl VideoEncoder {
    pub fn open(config: &EncoderConfig) -> Result<Self, VideoError> {
        let (w, h, fps) = (config.width, config.height, config.framerate.max(1));

        // Camera 0 (facing isn't selectable on most desktop webcams). Try source
        // formats in preference order: MJPEG is smallest over a virtual/RDP pipe
        // (WSLg/Windows), but macOS built-in cameras DON'T offer MJPEG — only YUV
        // variants — so fall back through NV12/YUYV/RAWRGB until one negotiates.
        // nokhwa decodes any of these to RGB via RgbFormat.
        let mut camera = None;
        for &fmt in &[FrameFormat::MJPEG, FrameFormat::NV12, FrameFormat::YUYV, FrameFormat::RAWRGB] {
            let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
                CameraFormat::new(Resolution::new(w, h), fmt, fps),
            ));
            match Camera::new(CameraIndex::Index(0), requested) {
                Ok(c) => { log::info!("video_desktop: camera opened as {fmt:?}"); camera = Some(c); break; }
                Err(e) => log::debug!("video_desktop: camera format {fmt:?} unavailable: {e:?}"),
            }
        }
        let mut camera = camera.ok_or_else(|| {
            log::warn!("video_desktop: Camera::new failed for all formats (no camera / permission?)");
            VideoError::CodecInitFailed
        })?;
        let cam = camera.camera_format();
        let (cam_w, cam_h) = (cam.resolution().width_x, cam.resolution().height_y);
        camera.open_stream().map_err(|e| {
            log::warn!("video_desktop: open_stream failed: {e:?}");
            VideoError::CodecInitFailed
        })?;
        log::info!(
            "video_desktop: camera {cam_w}x{cam_h} @ {} fps {:?} → encode {w}x{h} {:?}",
            cam.frame_rate(), cam.format(), config.codec
        );

        // Software VP8/VP9. Resize (camera size → encode size) and RGB→I420 both
        // happen inside the codec crate, straight into libvpx's own planes.
        let enc = wandr_video::open_encoder(&EncoderParams {
            codec: codec_of(config.codec),
            width: w,
            height: h,
            bitrate_bps: config.bitrate_bps,
            framerate: fps,
        })
        .map_err(map_err)?;

        // PiP self-view: register a slot the render loop composites (above-ui).
        // Self-view surface: mirrored, upright (rotation 0).
        let preview_id = config.preview.map(|rect| alloc_surface(rect, true, 0));

        // Capture thread: owns the camera, blocks on frame() here (not on the
        // store thread), and publishes the newest RGB frame into `latest`.
        let cam_stop = Arc::new(AtomicBool::new(false));
        let latest: Arc<Mutex<Option<LatestFrame>>> = Arc::new(Mutex::new(None));
        let (stop_t, latest_t) = (cam_stop.clone(), latest.clone());
        let send_cam = SendCamera(camera);
        let cam_thread = std::thread::Builder::new()
            .name("wandr-cam-capture".into())
            .spawn(move || {
                // Capture the whole SendCamera (Send), not just `.0` — Rust-2021
                // disjoint capture would otherwise grab the !Send Camera field.
                let send_cam = send_cam;
                let mut cam = send_cam.0;
                let mut seq = 0u64;
                while !stop_t.load(Ordering::Relaxed) {
                    match cam.frame().and_then(|b| b.decode_image::<RgbFormat>()) {
                        Ok(img) => {
                            seq += 1;
                            let (fw, fh) = (img.width(), img.height());
                            *latest_t.lock().unwrap() =
                                Some(LatestFrame { rgb: img.into_raw(), w: fw, h: fh, seq });
                        }
                        Err(e) => {
                            log::debug!("video_desktop: capture: {e:?}");
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                    }
                }
                // dropping `cam` here stops + closes the stream
            })
            .ok();

        Ok(Self {
            cam_thread, cam_stop, latest, last_seq: 0,
            enc, force_keyframe: false, preview_id, pip: Vec::new(),
        })
    }

    /// Capture one camera frame, hand it to the encoder, and update the self-view.
    fn capture_encode(&mut self) {
        // Non-blocking: take the freshest frame the capture thread published. No
        // new frame since we last encoded → return immediately. The render/pump
        // loop NEVER blocks on the camera here (the self-view freeze fix).
        let frame = {
            let mut g = self.latest.lock().unwrap();
            match g.as_ref() {
                Some(f) if f.seq != self.last_seq => g.take(),
                _ => None,
            }
        };
        let Some(frame) = frame else { return };
        self.last_seq = frame.seq;

        // PiP self-view: hand the render loop the latest camera frame as RGBA.
        if let Some(id) = self.preview_id {
            let (w, h) = (frame.w, frame.h);
            let px = (w * h) as usize;
            self.pip.resize(px * 4, 0);
            for i in 0..px {
                self.pip[i * 4] = frame.rgb[i * 3];
                self.pip[i * 4 + 1] = frame.rgb[i * 3 + 1];
                self.pip[i * 4 + 2] = frame.rgb[i * 3 + 2];
                self.pip[i * 4 + 3] = 255;
            }
            surface_set_frame(id, self.pip.clone(), w, h);
        }

        // Frames whose size differs from the encode size are resized by the codec
        // crate — the old ffmpeg path hard-skipped them instead.
        let force = std::mem::take(&mut self.force_keyframe);
        if let Err(e) = self
            .enc
            .encode(Rgb24Frame::new(&frame.rgb, frame.w, frame.h), force)
        {
            log::warn!("video_desktop: encode failed: {e:?}");
            // Don't silently swallow the keyframe request — retry it next frame.
            self.force_keyframe |= force;
        }
    }

    pub fn next_frame(&mut self) -> Option<EncodedFrame> {
        if let Some(p) = self.enc.next_packet() {
            return Some(EncodedFrame { data: p.data, timestamp: p.timestamp, keyframe: p.keyframe });
        }
        self.capture_encode();
        self.enc
            .next_packet()
            .map(|p| EncodedFrame { data: p.data, timestamp: p.timestamp, keyframe: p.keyframe })
    }

    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    pub fn set_bitrate(&mut self, bps: u32) {
        // Was a no-op on the ffmpeg path (libvpx rate control couldn't be retuned
        // post-open through ffmpeg-next without a full reconfigure). libvpx does
        // it directly and cheaply, so desktop now adapts to congestion for real.
        if let Err(e) = self.enc.set_bitrate(bps) {
            log::warn!("video_desktop: set_bitrate({bps}) failed: {e:?}");
        }
    }

    pub fn set_preview_rect(&mut self, rect: VideoRect) {
        if let Some(id) = self.preview_id {
            surface_with(id, |s| s.rect = rect);
        }
    }

    pub fn set_preview_visible(&mut self, visible: bool) {
        if let Some(id) = self.preview_id {
            surface_with(id, |s| s.visible = visible);
        }
    }

    pub fn display_rotation(&self) -> u32 {
        0 // desktop webcams are upright
    }
}

// ── decoder (decode-to-surface, or decode-to-buffer when rect is empty) ───────

/// What a video surface currently holds.
///
/// Two variants because BOTH are legitimate end states, chosen at RUNTIME: a
/// hardware frame imported straight into GL textures (zero CPU copies), or CPU
/// RGBA — which is what software decode produces, what the readback fallback
/// produces, and what the camera self-view genuinely is. It cannot be a compile
/// -time choice: `canvas.direct_context()` is `None` on the headless raster
/// surface every diagnostic uses, so the CPU lane must stay reachable.
pub(crate) enum SurfaceContent {
    /// An imported GPU frame, by ID into the thread-local texture registry.
    ///
    /// ‼️ AN ID, NOT THE TEXTURE ITSELF, and the compiler is what forced it: a
    /// `TakenFrame` goes into the wasmtime `ResourceTable`, which requires
    /// `Send`, and GL objects are only valid on the thread holding the context.
    /// `unsafe impl Send` would have compiled and then run `glDeleteTextures` on
    /// whatever thread dropped the frame — undefined behaviour, intermittently.
    /// Carrying a `u64` keeps every GL object strictly thread-local; the worst a
    /// wrong-thread drop can now do is leak, loudly.
    Texture(TextureRef),
    Rgba(Vec<u8>),
}

thread_local! {
    /// Imported GPU frames, owned here so no GL object ever crosses threads.
    static TEXTURES: RefCell<std::collections::HashMap<u64, crate::video_gl::TextureFrame>> =
        RefCell::new(std::collections::HashMap::new());
    static TEXTURE_NEXT: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

/// Owns one entry in the thread-local texture registry; frees it on drop.
/// Move-only, because exactly one place owns a frame at a time as it travels
/// pending -> taken -> surface.
pub(crate) struct TextureRef(u64);

impl TextureRef {
    fn insert(t: crate::video_gl::TextureFrame) -> Self {
        let id = TEXTURE_NEXT.with(|n| {
            let v = n.get();
            n.set(v.wrapping_add(1));
            v
        });
        TEXTURES.with(|m| m.borrow_mut().insert(id, t));
        TextureRef(id)
    }
}

impl Drop for TextureRef {
    fn drop(&mut self) {
        // A miss means this dropped on a thread that never held the textures —
        // impossible today (single-threaded host) but worth not being silent.
        let found = TEXTURES.with(|m| m.borrow_mut().remove(&self.0)).is_some();
        if !found {
            log::warn!("video_desktop: texture {} dropped off-thread — GL objects LEAKED", self.0);
        }
    }
}

impl SurfaceContent {
    fn is_empty(&self) -> bool {
        match self {
            SurfaceContent::Texture(_) => false,
            SurfaceContent::Rgba(v) => v.is_empty(),
        }
    }
}

/// Total decoded-frame memory one decoder may have outstanding.
///
/// Bounded in BYTES, not frames, so the answer is resolution-independent: a 4K
/// stream gets fewer frames in flight than a 720p one for the same footprint,
/// which is the behaviour you want and a frame count cannot express. 64 MiB is
/// ~17 frames at 1280x720 RGBA and ~4 at 4K.
const MAX_IN_FLIGHT_BYTES: usize = 64 * 1024 * 1024;

/// …but never fewer frames than a codec needs to REORDER, or decode deadlocks:
/// the codec waits for output slots the caller cannot free because the frames it
/// needs have not been emitted yet. H.264 allows 16 frames in the DPB (Annex A
/// MaxDpbMbs), so 17 is the smallest floor that is safe for any conformant
/// stream. This is the host-side twin of the guest's own cushion — see
/// wandr.video.player's MAX_H264_DPB.
const MIN_IN_FLIGHT_FRAMES: usize = 17;

/// Decrements a decoder's in-flight count when the frame it guards is finally
/// released — presented, discarded, or dropped.
///
/// A COUNTER rather than a decoder back-reference on purpose: `TakenFrame` is
/// deliberately self-contained so `present`/`discard` never need the decoder
/// back, and the guest may outlive it. An `Arc<AtomicUsize>` keeps that property
/// while still letting the decoder see how much memory it is responsible for.
struct InFlight(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A decoded frame waiting for its presentation time (playback mode only).
struct PendingFrame {
    pts_us: i64,
    content: SurfaceContent,
    w: u32,
    h: u32,
    /// Released when this frame's memory is, wherever it ends up.
    _in_flight: InFlight,
}

/// A decoded frame handed OUT to the guest as a `wandr:video` `decoded-frame`
/// resource (task 117 M2 stage 1).
///
/// Deliberately self-contained — it carries its own target surface — so
/// `present`/`discard` never need the decoder back. That matters because the
/// guest may hold a frame across decoder calls, and it keeps the resource's
/// lifetime independent of the decoder's borrow.
pub struct TakenFrame {
    pts_us: i64,
    content: SurfaceContent,
    w: u32,
    h: u32,
    surface_id: Option<u32>,
    /// Moved over from `PendingFrame`: a frame the guest holds, or one parked in
    /// SCHEDULED awaiting its deadline, is still memory this decoder owns. That
    /// is the whole point — presenting with a FUTURE deadline is exactly how a
    /// player accumulates frames in the host, and it must count.
    _in_flight: InFlight,
}

impl TakenFrame {
    pub fn timestamp_us(&self) -> i64 {
        self.pts_us
    }
    /// Paint it now. `surface_id == None` = decode-to-buffer, so this is the
    /// counted-and-dropped diagnostic path and presenting is a no-op.
    pub fn present_now(self) {
        if let Some(id) = self.surface_id {
            surface_set_content(id, self.content, self.w, self.h);
        }
    }
}

/// Frames the guest scheduled for a FUTURE `at-ns`, ordered by deadline.
///
/// The guest calls `present(at-ns)` on the wasi:clocks monotonic timeline; the
/// host owns the actual paint. Draining happens in `composite_video_surfaces`,
/// which already runs once per host frame — so scheduling costs no new thread,
/// timer or tick. On Android this queue does not exist at all:
/// `AMediaCodec_releaseOutputBufferAtTime` hands the timestamp straight to the
/// HW compositor.
thread_local! {
    static SCHEDULED: RefCell<Vec<(u64, TakenFrame)>> = const { RefCell::new(Vec::new()) };
}

/// Monotonic host clock in nanoseconds — the timeline `present(at-ns)` speaks.
pub fn monotonic_now_ns() -> u64 {
    // THE host timeline (host_clock) — shared with wasi:clocks and on-frame, so
    // a deadline a guest computes is one this scheduler understands. This used
    // to be its own `Instant` origin, which is why `present(at-ns)` never worked.
    crate::host_clock::now_ns()
}

/// Schedule `frame` for `at_ns`; presents immediately if already due.
pub fn schedule_present(at_ns: u64, frame: TakenFrame) {
    if at_ns <= monotonic_now_ns() {
        frame.present_now();
        return;
    }
    SCHEDULED.with(|s| {
        let mut s = s.borrow_mut();
        let at = s.partition_point(|(t, _)| *t <= at_ns);
        s.insert(at, (at_ns, frame));
    });
}

/// Paint every scheduled frame whose deadline has passed. Called once per host
/// frame from `composite_video_surfaces`.
///
/// Frame-drop policy matches `present_due`: when several frames on the SAME
/// surface are due, only the newest is painted — showing the older ones would
/// be showing the past. That is player policy, which is why it lives here on
/// the host adapter and not in the codec.
fn drain_scheduled() {
    let now = monotonic_now_ns();
    let due: Vec<TakenFrame> = SCHEDULED.with(|s| {
        let mut s = s.borrow_mut();
        let n = s.partition_point(|(t, _)| *t <= now);
        s.drain(..n).map(|(_, f)| f).collect()
    });
    let mut newest: std::collections::HashMap<Option<u32>, TakenFrame> = Default::default();
    for f in due {
        newest.insert(f.surface_id, f);
    }
    for (_, f) in newest {
        f.present_now();
    }
}

pub struct VideoDecoder {
    dec: Box<dyn wandr_video::Decoder>,
    decoded: u64,
    /// Compositing surface (Some iff opened with a real rect = decode-to-surface).
    /// None = decode-to-buffer: frames are counted + dropped (the Phase-1 loopback
    /// diagnostic; `wandr.video.test` Part 1).
    surface_id: Option<u32>,
    /// Reused RGBA staging, so steady-state decode-to-surface doesn't allocate.
    rgba: Vec<u8>,
    /// Reused I420 staging for materialising a GPU frame on the readback path.
    gpu_scratch: Vec<u8>,
    /// PLAYBACK mode (task 117 M2): frames decoded ahead, each held until the
    /// caller's media clock reaches its PTS. Empty on the call path, which
    /// presents immediately because the network is the pacer.
    ///
    /// On Android this queue does not exist — `AMediaCodec_releaseOutputBufferAtTime`
    /// hands the PTS to the HW compositor and the frame never reaches the CPU.
    /// This is the desktop stand-in for that, which is why it holds RGBA.
    pending: VecDeque<PendingFrame>,
    /// Frames dropped because a newer one was already due (playback only).
    dropped: u64,
    /// Decoded frames whose memory this decoder still owns: queued in `pending`,
    /// held by the guest as a `decoded-frame`, or parked in SCHEDULED. Shared
    /// with each frame's `InFlight` guard, which decrements on release.
    in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Cap on the above, derived from the frame size once it is known (0 = not
    /// yet). See MAX_IN_FLIGHT_BYTES / MIN_IN_FLIGHT_FRAMES.
    in_flight_cap: usize,
    /// Which backend actually served this decoder. Reported to the guest via
    /// `implementation()`: `acceleration` is a preference, so a guest that asked
    /// for hardware and got software must be able to find out.
    backend: wandr_video::BackendInfo,
}

/// A guest's hardware/software preference for a decoder (the WIT `acceleration`
/// enum, kept out of the WIT bindings so `video.rs` stays binding-agnostic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Accel {
    #[default]
    NoPreference,
    PreferHardware,
    PreferSoftware,
    RequireHardware,
}

impl VideoDecoder {
    pub fn open(config: &DecoderConfig) -> Result<Self, VideoError> {
        Self::open_with_accel(config, Accel::NoPreference)
    }

    /// Open with an explicit acceleration preference.
    ///
    /// `prefer-*` are HINTS and must still open if the preference cannot be met —
    /// a player that refuses to play is worse than one that plays in software.
    /// Only `require-hardware` may turn a working decode into an error, which is
    /// the point of it. The operator's env policy still wins over the guest's
    /// preference: a machine-level "no hardware here" is a statement of fact, and
    /// letting an app override it would make measurements meaningless.
    pub fn open_with_accel(config: &DecoderConfig, accel: Accel) -> Result<Self, VideoError> {
        Self::open_impl(config, accel)
    }

    fn open_impl(config: &DecoderConfig, accel: Accel) -> Result<Self, VideoError> {
        // Backend policy comes from the environment (WANDR_VIDEO_BACKEND /
        // WANDR_VIDEO_NO_HW / WANDR_VIDEO_REQUIRE_HW), so the SAME app and the same
        // clip can be run against hardware and software on one machine. Default is
        // unchanged: hardware first, software fallback.
        let params = DecoderParams {
            codec: codec_of(config.codec),
            width: config.width,
            height: config.height,
        };
        let mut prefs = crate::video_prefs_from_env();
        // Guest preference, applied UNDER the operator's env policy (which is why
        // these are `||=` rather than assignments — env-forced software stays
        // forced even if the guest asks for hardware).
        match accel {
            Accel::NoPreference => {}
            Accel::PreferSoftware => prefs.no_hardware = true,
            Accel::RequireHardware => prefs.require_hardware = true,
            Accel::PreferHardware => {} // already the default order: HW first
        }
        let (dec, backend) = match wandr_video::open_decoder_named(&params, prefs) {
            Ok(d) => d,
            // A HINT that could not be met must not fail the open — retry with the
            // host's default policy. `require-hardware` deliberately does not get
            // this second chance.
            Err(e) if matches!(accel, Accel::PreferSoftware | Accel::PreferHardware) => {
                log::warn!(
                    "video_desktop: {accel:?} could not be satisfied ({e:?}) — \
                     falling back to default backend policy"
                );
                wandr_video::open_decoder_named(&params, crate::video_prefs_from_env())
                    .map_err(map_err)?
            }
            Err(e) => return Err(map_err(e)),
        };

        // A real rect = decode-to-SURFACE (composite on screen, upright per the
        // peer's CVO rotation); empty/None = decode-to-buffer (count only).
        let surface_id = config.rect.filter(|r| r.w > 0 && r.h > 0).map(|rect| {
            log::info!("video_desktop: decode-to-surface {}x{} @ ({},{}) rot={}°",
                rect.w, rect.h, rect.x, rect.y, config.rotation);
            alloc_surface(rect, false, config.rotation)
        });
        Ok(Self { dec, decoded: 0, surface_id, rgba: Vec::new(), gpu_scratch: Vec::new(),
                  pending: VecDeque::new(), dropped: 0,
                  in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                  in_flight_cap: 0, backend })
    }

    pub fn submit(&mut self, data: &[u8], timestamp: u32) -> Result<(), VideoError> {
        // The call path speaks a 90 kHz RTP clock; the codec speaks microseconds
        // (task 117 M2). Convert at the boundary rather than widening the codec
        // back to a transport timestamp. Playback callers pass real µs directly.
        let timestamp_us = (timestamp as i64) * 1_000_000 / 90_000;
        self.dec
            .decode(wandr_video::Chunk::new(data, timestamp_us))
            .map_err(map_err)?;
        // VP8/VP9 keyframes carry their own dimensions, and the codec reports them
        // per frame — which is why the old lazy scaler-rebuild machinery is gone.
        while let Some(frame) = self.dec.next_frame() {
            self.decoded += 1;
            let Some(id) = self.surface_id else { continue };
            let (w, h) = frame.dimensions();
            // Only convert in decode-to-surface mode — decode-to-buffer must not
            // pay for a colorspace conversion it throws away.
            //
            // The CALL path presents immediately, so a GPU frame is materialised
            // here rather than kept: the network is the pacer and nothing holds
            // frames. Zero-copy pays off in the PLAYBACK path, where frames are
            // queued (see `queue_decoded`).
            let mut scratch = std::mem::take(&mut self.gpu_scratch);
            let ok = match frame.as_i420() {
                Some(i420) => wandr_video::i420_to_rgba(i420, &mut self.rgba).is_ok(),
                None => frame
                    .read_i420(&mut scratch)
                    .map(|i420| wandr_video::i420_to_rgba(&i420, &mut self.rgba).is_ok())
                    .unwrap_or(false),
            };
            self.gpu_scratch = scratch;
            if ok {
                surface_set_frame(id, self.rgba.clone(), w, h);
            }
        }
        Ok(())
    }

    pub fn decoded_frames(&self) -> u64 {
        self.decoded
    }

    /// Which backend served this decoder — name and whether it is hardware.
    pub fn backend(&self) -> (&'static str, bool) {
        (self.backend.name, self.backend.is_hardware())
    }

    // ── playback mode (task 117 M2) ──────────────────────────────────────────
    // The call path presents immediately: the network is the pacer, and a late
    // frame is worthless. A player is the opposite — frames are decoded AHEAD and
    // each is held until the media clock reaches its PTS. These three methods are
    // that difference, and nothing else on this type changes.

    /// Decode a chunk carrying a real presentation timestamp, queueing the frames
    /// it produces rather than presenting them.
    pub fn submit_for_playback(&mut self, data: &[u8], pts_us: i64) -> Result<(), VideoError> {
        // ‼️ BACK-PRESSURE, BEFORE DECODING. `queue-full` has been in the
        // contract since stage 1 ("NOT loss: retry the SAME frame") and the
        // desktop host never once returned it, so a guest had no signal to feed
        // against and could only GUESS a decode-ahead cushion — guess low and it
        // deadlocks silently, guess high and it decodes a whole file into host
        // memory. Refusing here is what turns feeding into a conversation.
        //
        // Checked before `decode` on purpose: decoding first would materialise
        // the very frame we have no room for.
        if self.in_flight_cap != 0
            && self.in_flight.load(std::sync::atomic::Ordering::Relaxed) >= self.in_flight_cap
        {
            return Err(VideoError::QueueFull);
        }
        self.dec
            .decode(wandr_video::Chunk::new(data, pts_us))
            .map_err(map_err)?;
        self.queue_decoded();
        Ok(())
    }

    /// End of stream — drain whatever the codec held back.
    pub fn finish_playback(&mut self) -> Result<(), VideoError> {
        self.dec.flush().map_err(map_err)?;
        self.queue_decoded();
        Ok(())
    }

    /// Seek — discard queued work; the caller must feed a keyframe next.
    pub fn seek_reset(&mut self) -> Result<(), VideoError> {
        self.dec.reset().map_err(map_err)?;
        self.pending.clear();
        Ok(())
    }

    fn queue_decoded(&mut self) {
        while let Some(f) = self.dec.next_frame() {
            self.decoded += 1;
            let (w, h) = f.dimensions();
            let pts_us = f.timestamp_us();
            let mut scratch = std::mem::take(&mut self.gpu_scratch);
            let content = match f.into_gpu() {
                // ZERO-COPY: the decoded frame is still in GPU memory. Import it
                // as two GL textures and never touch a pixel. Import is attempted
                // here rather than at composite time so a failure falls back to
                // the readback while we still hold the frame.
                Ok(gpu) => match crate::video_gl::import_nv12(gpu) {
                    Ok(t) => Some(SurfaceContent::Texture(TextureRef::insert(t))),
                    // Import unavailable or refused — no GL context (every
                    // headless diagnostic), no extension, or a modifier we cannot
                    // describe. The frame comes BACK, so read it out the same way
                    // a CPU frame is materialised. Without this the diagnostics
                    // decode 300 frames and present none.
                    Err(gpu) => {
                        let f = wandr_video::Frame::gpu(gpu);
                        let mut rgba = Vec::new();
                        let ok = f
                            .read_i420(&mut scratch)
                            .map(|i420| wandr_video::i420_to_rgba(&i420, &mut rgba).is_ok())
                            .unwrap_or(false);
                        ok.then_some(SurfaceContent::Rgba(rgba))
                    }
                },
                Err(cpu) => {
                    let mut rgba = Vec::new();
                    let ok = match cpu.as_i420() {
                        Some(i420) => wandr_video::i420_to_rgba(i420, &mut rgba).is_ok(),
                        None => cpu
                            .read_i420(&mut scratch)
                            .map(|i420| wandr_video::i420_to_rgba(&i420, &mut rgba).is_ok())
                            .unwrap_or(false),
                    };
                    ok.then_some(SurfaceContent::Rgba(rgba))
                }
            };
            self.gpu_scratch = scratch;
            if let Some(content) = content {
                // Insert in PTS order — this makes `pending` a reorder buffer.
                // Codecs with B-frames (H.264/H.265) emit in DECODE order, so
                // frames arrive out of presentation order; `present_due` needs the
                // front to be the smallest PTS. VP8/VP9 (no B-frames) always
                // insert at the back, so this costs them nothing. The decode-ahead
                // cushion the player keeps must exceed the stream's reorder depth,
                // else a late earlier-PTS frame would arrive after its slot passed.
                // Derive the cap the first time we know the real frame size; a
                // decoder's `frames_in_flight_limit` wins when it has a hard
                // pool (MediaCodec indices, VA surfaces) rather than just memory.
                if self.in_flight_cap == 0 {
                    let bytes = (w as usize).saturating_mul(h as usize).saturating_mul(4).max(1);
                    let by_memory = (MAX_IN_FLIGHT_BYTES / bytes).max(MIN_IN_FLIGHT_FRAMES);
                    self.in_flight_cap = self.dec.frames_in_flight_limit().unwrap_or(by_memory);
                    log::info!(
                        "video_desktop: in-flight cap {} frames ({}x{} RGBA, {} MiB budget)",
                        self.in_flight_cap, w, h, MAX_IN_FLIGHT_BYTES / (1024 * 1024)
                    );
                }
                self.in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let guard = InFlight(std::sync::Arc::clone(&self.in_flight));
                let at = self.pending.partition_point(|p| p.pts_us <= pts_us);
                self.pending.insert(at, PendingFrame { pts_us, content, w, h, _in_flight: guard });
            }
        }
    }

    /// Take the next decoded frame in DISPLAY order and hand ownership to the
    /// caller (the `decoded-frame` resource). `pending` is already PTS-ordered,
    /// so the front is the correct next frame even for B-frame streams.
    ///
    /// This is the guest-driven counterpart to `present_due`: there the HOST
    /// decides what to show against a clock it is given; here the GUEST decides,
    /// which is what a real player needs in order to slave video to the audio
    /// master clock.
    pub fn take_next_decoded(&mut self) -> Option<TakenFrame> {
        let f = self.pending.pop_front()?;
        Some(TakenFrame {
            pts_us: f.pts_us,
            content: f.content,
            w: f.w,
            h: f.h,
            surface_id: self.surface_id,
            // The guard MOVES, it is not recreated: handing a frame to the guest
            // changes who holds the memory, not whether it is held.
            _in_flight: f._in_flight,
        })
    }

    /// Present whatever is due at `clock_us` and return its PTS.
    ///
    /// Frame-drop policy: if several frames are already due, only the NEWEST is
    /// shown and the older ones are counted as dropped — showing them would be
    /// showing the past. That is a player POLICY choice, which is exactly why it
    /// lives here on the host adapter and not inside the codec.
    pub fn present_due(&mut self, clock_us: i64) -> Option<i64> {
        let mut chosen: Option<PendingFrame> = None;
        while self.pending.front().is_some_and(|f| f.pts_us <= clock_us) {
            if chosen.is_some() {
                self.dropped += 1;
            }
            chosen = self.pending.pop_front();
        }
        let f = chosen?;
        if let Some(id) = self.surface_id {
            surface_set_content(id, f.content, f.w, f.h);
        }
        Some(f.pts_us)
    }

    /// Frames decoded but not yet due — the player's cushion against a decode
    /// stall. A real player refills toward a target depth.
    pub fn queued_frames(&self) -> usize {
        self.pending.len()
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped
    }

    pub fn set_rect(&mut self, rect: VideoRect) {
        if let Some(id) = self.surface_id {
            surface_with(id, |s| s.rect = rect);
        }
    }
    pub fn set_visible(&mut self, visible: bool) {
        if let Some(id) = self.surface_id {
            surface_with(id, |s| s.visible = visible);
        }
    }
    pub fn set_rotation(&mut self, degrees: u32) {
        if let Some(id) = self.surface_id {
            surface_with(id, |s| s.rotation = degrees);
        }
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        if let Some(id) = self.surface_id {
            surface_remove(id);
        }
    }
}
