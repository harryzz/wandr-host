//! Desktop (non-Android) `wandr:video` backend — nokhwa camera capture + ffmpeg
//! software VP8/VP9 (libvpx) encode/decode. The cross-platform peer of the
//! Android NDK-camera + AMediaCodec backend in `video.rs::android` (Linux v4l2 /
//! Windows MediaFoundation / macOS AVFoundation via nokhwa; libvpx via ffmpeg).
//!
//! Scope: the OUTGOING encoder (camera → VP8) and decode-to-BUFFER (frame
//! counting) — enough for `wandr.video.test` Part 1/2 to pass. On-screen
//! compositing (PiP preview + decode-to-surface) is a follow-up; the Android
//! SurfaceView child-surface model has no desktop analog yet, so `set_preview_*`
//! / `set_rect` / `set_visible` are recorded no-ops. Proven end-to-end in
//! `repros/nokhwa-camera-probe` (camera → VP8 → decode).
//!
//! WSLg note: the RDP-forwarded virtual camera truncates large buffers, so
//! >640x480 tears; the call path uses 640x480, which is intact. Real cameras
//! (device/native) handle 720p+.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};

use ffmpeg_next as ffmpeg;
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg::util::frame::video::Video as FfVideoFrame;
use ffmpeg::util::picture;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use nokhwa::Camera;

use crate::video::{Codec, DecoderConfig, EncodedFrame, EncoderConfig, VideoError, VideoRect};

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
    rgba: Vec<u8>, // tightly-packed RGBA8888, w*h*4 (empty until the first frame)
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
pub fn composite_video_surfaces(canvas: &skia_safe::Canvas) {
    SURFACES.with(|m| {
        for s in m.borrow().values() {
            if !s.visible || s.rgba.is_empty() || s.rect.w <= 0 || s.rect.h <= 0 {
                continue;
            }
            let info = skia_safe::ImageInfo::new(
                (s.w as i32, s.h as i32),
                skia_safe::ColorType::RGBA8888,
                skia_safe::AlphaType::Unpremul,
                None,
            );
            let data = skia_safe::Data::new_copy(&s.rgba);
            let Some(img) = skia_safe::images::raster_from_data(&info, data, (s.w * 4) as usize)
            else {
                continue;
            };
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
            rgba: Vec::new(), w: 0, h: 0, rect, visible: true, mirror, rotation,
        });
    });
    id
}

/// Update a surface's pixels (from the encoder capture / decoder output).
fn surface_set_frame(id: u32, rgba: Vec<u8>, w: u32, h: u32) {
    SURFACES.with(|m| {
        if let Some(s) = m.borrow_mut().get_mut(&id) {
            s.rgba = rgba;
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
/// the camera/codec HAL; desktop nokhwa/ffmpeg need none).
pub fn ensure_binder_threadpool() -> bool {
    false
}

fn ff_init() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = ffmpeg::init();
    });
}

/// libvpx encoder name for the codec (VP8/VP9; H264/H265 are rejected upstream).
fn enc_name(codec: Codec) -> &'static str {
    match codec {
        Codec::Vp8 => "libvpx",
        Codec::Vp9 => "libvpx-vp9",
    }
}

fn dec_id(codec: Codec) -> ffmpeg::codec::Id {
    match codec {
        Codec::Vp8 => ffmpeg::codec::Id::VP8,
        Codec::Vp9 => ffmpeg::codec::Id::VP9,
    }
}

/// 90 kHz RTP timestamp from a frame index at `fps` (matches the Android encoder's
/// µs-PTS → 90 kHz conversion; wraps like RTP).
fn rtp_ts(idx: i64, fps: u32) -> u32 {
    let fps = fps.max(1) as i64;
    ((idx * 90_000) / fps) as u32
}

// ── encoder ──────────────────────────────────────────────────────────────────

pub struct VideoEncoder {
    camera: Camera,
    scaler: Scaler,
    encoder: ffmpeg::encoder::video::Encoder,
    rgb: FfVideoFrame,
    /// Camera's actual decoded resolution (scaler input); may differ from the
    /// encoder's target (config w×h), so the scaler also resizes.
    cam_w: u32,
    cam_h: u32,
    fps: u32,
    pts: i64,
    force_keyframe: bool,
    pending: VecDeque<EncodedFrame>,
    /// PiP self-view surface (Some iff opened with a preview rect).
    preview_id: Option<u32>,
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        if let Some(id) = self.preview_id {
            surface_remove(id);
        }
    }
}

// The store is single-threaded on desktop (winit loop / run-once command); the
// !Send nokhwa/ffmpeg contexts never cross threads. Mirrors video.rs::android's
// `unsafe impl Send for VideoEncoder`.
unsafe impl Send for VideoEncoder {}

impl VideoEncoder {
    pub fn open(config: &EncoderConfig) -> Result<Self, VideoError> {
        ff_init();
        let (w, h, fps) = (config.width, config.height, config.framerate.max(1));

        // Camera 0 (facing isn't selectable on most desktop webcams). MJPEG is
        // the smallest over a virtual/RDP pipe; nokhwa decodes it to RGB.
        let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
            CameraFormat::new(Resolution::new(w, h), FrameFormat::MJPEG, fps),
        ));
        let mut camera = Camera::new(CameraIndex::Index(0), requested).map_err(|e| {
            log::warn!("video_desktop: Camera::new failed: {e:?}");
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

        // libvpx encoder.
        let enc_codec = ffmpeg::encoder::find_by_name(enc_name(config.codec)).ok_or_else(|| {
            log::warn!("video_desktop: no {} encoder", enc_name(config.codec));
            VideoError::NoHwCodec
        })?;
        let mut enc_ctx = ffmpeg::codec::context::Context::new_with_codec(enc_codec)
            .encoder()
            .video()
            .map_err(|_| VideoError::CodecInitFailed)?;
        enc_ctx.set_width(w);
        enc_ctx.set_height(h);
        enc_ctx.set_format(Pixel::YUV420P);
        enc_ctx.set_time_base((1, fps as i32));
        enc_ctx.set_bit_rate(config.bitrate_bps.max(100_000) as usize);
        // Recover-friendly keyframe cadence; on-demand keyframes via set_kind(I).
        enc_ctx.set_gop(fps * 4);
        let mut opts = ffmpeg::Dictionary::new();
        opts.set("deadline", "realtime"); // low-latency VP8 (a call, not a file)
        opts.set("lag-in-frames", "0");
        let encoder = enc_ctx.open_with(opts).map_err(|e| {
            log::warn!("video_desktop: encoder open failed: {e:?}");
            VideoError::CodecInitFailed
        })?;

        let scaler = Scaler::get(Pixel::RGB24, cam_w, cam_h, Pixel::YUV420P, w, h, Flags::BILINEAR)
            .map_err(|_| VideoError::CodecInitFailed)?;
        let rgb = FfVideoFrame::new(Pixel::RGB24, cam_w, cam_h);

        // PiP self-view: register a slot the render loop composites (above-ui).
        // Self-view surface: mirrored, upright (rotation 0).
        let preview_id = config.preview.map(|rect| alloc_surface(rect, true, 0));

        Ok(Self {
            camera, scaler, encoder, rgb, cam_w, cam_h, fps,
            pts: 0, force_keyframe: false, pending: VecDeque::new(), preview_id,
        })
    }

    /// Capture one camera frame, scale RGB→YUV420P, encode; push any produced
    /// packets onto `pending`.
    fn capture_encode(&mut self) {
        let buf = match self.camera.frame() {
            Ok(b) => b,
            Err(e) => { log::debug!("video_desktop: capture: {e:?}"); return; }
        };
        let img = match buf.decode_image::<RgbFormat>() {
            Ok(im) => im,
            Err(e) => { log::debug!("video_desktop: decode: {e:?}"); return; }
        };
        if img.width() != self.cam_w || img.height() != self.cam_h {
            log::warn!("video_desktop: frame {}x{} != {}x{}, skipping",
                img.width(), img.height(), self.cam_w, self.cam_h);
            return;
        }
        // Copy tightly-packed RGB into the ffmpeg frame (aligned stride).
        {
            let w3 = self.cam_w as usize * 3;
            let stride = self.rgb.stride(0);
            let data = self.rgb.data_mut(0);
            let src = img.as_raw();
            for y in 0..self.cam_h as usize {
                data[y * stride..y * stride + w3].copy_from_slice(&src[y * w3..y * w3 + w3]);
            }
        }
        // PiP self-view: hand the render loop the latest camera frame as RGBA.
        if let Some(id) = self.preview_id {
            let (w, h) = (self.cam_w, self.cam_h);
            let rgb = img.as_raw();
            let mut rgba = vec![0u8; (w * h * 4) as usize];
            for i in 0..(w * h) as usize {
                rgba[i * 4] = rgb[i * 3];
                rgba[i * 4 + 1] = rgb[i * 3 + 1];
                rgba[i * 4 + 2] = rgb[i * 3 + 2];
                rgba[i * 4 + 3] = 255;
            }
            surface_set_frame(id, rgba, w, h);
        }
        let mut yuv = FfVideoFrame::empty();
        if self.scaler.run(&self.rgb, &mut yuv).is_err() {
            return;
        }
        yuv.set_pts(Some(self.pts));
        if self.force_keyframe {
            yuv.set_kind(picture::Type::I);
            self.force_keyframe = false;
        }
        if self.encoder.send_frame(&yuv).is_err() {
            return;
        }
        self.drain_packets();
        self.pts += 1;
    }

    fn drain_packets(&mut self) {
        let mut pkt = ffmpeg::Packet::empty();
        while self.encoder.receive_packet(&mut pkt).is_ok() {
            if let Some(data) = pkt.data() {
                self.pending.push_back(EncodedFrame {
                    data: data.to_vec(),
                    timestamp: rtp_ts(pkt.pts().unwrap_or(self.pts), self.fps),
                    keyframe: pkt.is_key(),
                });
            }
        }
    }

    pub fn next_frame(&mut self) -> Option<EncodedFrame> {
        if self.pending.is_empty() {
            self.capture_encode();
        }
        self.pending.pop_front()
    }

    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    pub fn set_bitrate(&mut self, _bps: u32) {
        // libvpx rate control can't be retuned post-open via ffmpeg without a
        // reconfigure; the desktop path is best-effort (the device honors REMB).
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

pub struct VideoDecoder {
    decoder: ffmpeg::decoder::Video,
    decoded: u64,
    /// Compositing surface (Some iff opened with a real rect = decode-to-surface).
    /// None = decode-to-buffer: frames are counted + dropped (the Phase-1 loopback
    /// diagnostic; `wandr.video.test` Part 1).
    surface_id: Option<u32>,
    /// YUV→RGBA scaler, (re)built lazily to match the decoded frame's size (VP8
    /// keyframes carry their own dimensions, which can change mid-stream).
    scaler: Option<Scaler>,
    scaler_dims: (u32, u32),
}

unsafe impl Send for VideoDecoder {}

impl VideoDecoder {
    pub fn open(config: &DecoderConfig) -> Result<Self, VideoError> {
        ff_init();
        let dec_codec = ffmpeg::decoder::find(dec_id(config.codec)).ok_or(VideoError::NoHwCodec)?;
        let decoder = ffmpeg::codec::context::Context::new_with_codec(dec_codec)
            .decoder()
            .video()
            .map_err(|e| {
                log::warn!("video_desktop: decoder open failed: {e:?}");
                VideoError::CodecInitFailed
            })?;
        // A real rect = decode-to-SURFACE (composite on screen, upright per the
        // peer's CVO rotation); empty/None = decode-to-buffer (count only).
        let surface_id = config.rect.filter(|r| r.w > 0 && r.h > 0).map(|rect| {
            log::info!("video_desktop: decode-to-surface {}x{} @ ({},{}) rot={}°",
                rect.w, rect.h, rect.x, rect.y, config.rotation);
            alloc_surface(rect, false, config.rotation)
        });
        Ok(Self { decoder, decoded: 0, surface_id, scaler: None, scaler_dims: (0, 0) })
    }

    /// Convert one decoded YUV frame to RGBA and hand it to the compositor.
    fn composite_frame(&mut self, frame: &FfVideoFrame) {
        let Some(id) = self.surface_id else { return };
        let (w, h) = (frame.width(), frame.height());
        if w == 0 || h == 0 {
            return;
        }
        if self.scaler.is_none() || self.scaler_dims != (w, h) {
            match Scaler::get(frame.format(), w, h, Pixel::RGBA, w, h, Flags::BILINEAR) {
                Ok(s) => { self.scaler = Some(s); self.scaler_dims = (w, h); }
                Err(e) => { log::warn!("video_desktop: decode scaler: {e:?}"); return; }
            }
        }
        let mut rgba_frame = FfVideoFrame::empty();
        if self.scaler.as_mut().unwrap().run(frame, &mut rgba_frame).is_err() {
            return;
        }
        // Tightly pack RGBA (drop the aligned row stride) for skia raster upload.
        let stride = rgba_frame.stride(0);
        let src = rgba_frame.data(0);
        let w4 = w as usize * 4;
        let mut rgba = vec![0u8; w4 * h as usize];
        for y in 0..h as usize {
            rgba[y * w4..y * w4 + w4].copy_from_slice(&src[y * stride..y * stride + w4]);
        }
        surface_set_frame(id, rgba, w, h);
    }

    pub fn submit(&mut self, data: &[u8], _timestamp: u32) -> Result<(), VideoError> {
        let pkt = ffmpeg::Packet::copy(data);
        self.decoder.send_packet(&pkt).map_err(|_| VideoError::BadFrame)?;
        let mut frame = FfVideoFrame::empty();
        while self.decoder.receive_frame(&mut frame).is_ok() {
            self.decoded += 1;
            self.composite_frame(&frame);
        }
        Ok(())
    }

    pub fn decoded_frames(&self) -> u64 {
        self.decoded
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
