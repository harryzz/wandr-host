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

use std::collections::VecDeque;

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

        Ok(Self {
            camera, scaler, encoder, rgb, cam_w, cam_h, fps,
            pts: 0, force_keyframe: false, pending: VecDeque::new(),
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

    pub fn set_preview_rect(&mut self, _rect: VideoRect) {} // no desktop compositing yet
    pub fn set_preview_visible(&mut self, _visible: bool) {}

    pub fn display_rotation(&self) -> u32 {
        0 // desktop webcams are upright
    }
}

// ── decoder (decode-to-buffer) ────────────────────────────────────────────────

pub struct VideoDecoder {
    decoder: ffmpeg::decoder::Video,
    decoded: u64,
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
        if config.rect.map(|r| r.w > 0 && r.h > 0).unwrap_or(false) {
            log::info!("video_desktop: decode-to-surface not yet supported — decode-to-buffer");
        }
        Ok(Self { decoder, decoded: 0 })
    }

    pub fn submit(&mut self, data: &[u8], _timestamp: u32) -> Result<(), VideoError> {
        let pkt = ffmpeg::Packet::copy(data);
        self.decoder.send_packet(&pkt).map_err(|_| VideoError::BadFrame)?;
        let mut frame = FfVideoFrame::empty();
        while self.decoder.receive_frame(&mut frame).is_ok() {
            self.decoded += 1;
        }
        Ok(())
    }

    pub fn decoded_frames(&self) -> u64 {
        self.decoded
    }

    pub fn set_rect(&mut self, _rect: VideoRect) {}
    pub fn set_visible(&mut self, _visible: bool) {}
    pub fn set_rotation(&mut self, _degrees: u32) {}
}
