//! Software H.264 via Cisco's OpenH264 (BSD-2, built from source by
//! `openh264-sys2`'s `source` feature — no system library, cross-compiles with
//! `cc`). The desktop software fallback for H.264; Android decodes H.264 in
//! hardware via MediaCodec and never reaches this.
//!
//! WHY openh264 and not a HW lane here: this is the SOFTWARE floor. The HW lanes
//! (VAAPI / VideoToolbox / MediaFoundation) are separate backends that register
//! at a lower priority and win when present; this is what runs when they don't.
//!
//! ‼️ PTS AND B-FRAMES. OpenH264's decoder does NOT hand a per-frame timestamp
//! back (unlike libvpx's `user_priv`), so we pair decoded frames to input PTS
//! with a FIFO. That is correct ONLY while decode order == presentation order,
//! i.e. NO B-frames. Our own encoder is configured `CameraVideoRealTime`
//! (low-latency, no B-frames), so the round-trip is sound. A real H.264 *file*
//! can contain
//! B-frames, where decode order != presentation order and the FIFO would
//! mispair — that reorder buffer is the next sub-step (task 117 M2 step 2b),
//! gated behind a real file + demuxer. Until then this backend asserts its
//! streams are B-frame-free by construction.

use std::collections::VecDeque;

use openh264::decoder::Decoder as OhDecoder;
use openh264::encoder::{
    BitRate, Encoder as OhEncoder, EncoderConfig, FrameRate, IntraFramePeriod, UsageType,
};
use openh264::formats::{RgbSliceU8, YUVBuffer, YUVSource};

use crate::convert::Rgb24Frame;
use crate::{
    BackendKind, Codec, CodecBackend, CodecError, Decoder, DecoderParams, Encoder, EncoderParams,
    I420Ref, Packet,
};

// ── backend descriptor ───────────────────────────────────────────────────────

pub struct OpenH264Backend;

impl CodecBackend for OpenH264Backend {
    fn name(&self) -> &'static str {
        "openh264"
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Software
    }
    fn priority(&self) -> u32 {
        100
    }
    fn supports_decode(&self, codec: Codec) -> bool {
        codec == Codec::H264
    }
    fn supports_encode(&self, codec: Codec) -> bool {
        codec == Codec::H264
    }
    fn open_decoder(&self, p: &DecoderParams) -> Result<Box<dyn Decoder>, CodecError> {
        Ok(Box::new(H264Decoder::open(p)?))
    }
    fn open_encoder(&self, p: &EncoderParams) -> Result<Box<dyn Encoder>, CodecError> {
        Ok(Box::new(H264Encoder::open(p)?))
    }
}

// ── encoder ──────────────────────────────────────────────────────────────────

pub struct H264Encoder {
    enc: OhEncoder,
    width: u32,
    height: u32,
    fps: u32,
    pts: i64,
    force_keyframe: bool,
    pending: VecDeque<Packet>,
    /// Reused resized-RGB scratch, so a steady-state encode doesn't allocate.
    scratch: Vec<u8>,
    resizer: fast_image_resize::Resizer,
}

// OpenH264's encoder is a plain heap object with no thread affinity.
unsafe impl Send for H264Encoder {}

impl H264Encoder {
    pub fn open(p: &EncoderParams) -> Result<Self, CodecError> {
        let (w, h, fps) = (p.width, p.height, p.framerate.max(1));
        let cfg = EncoderConfig::new()
            .bitrate(BitRate::from_bps(p.bitrate_bps.max(100_000)))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            // CameraVideoRealTime = realtime, low-latency, NO B-frames — see the
            // module note on why that matters for PTS pairing.
            .usage_type(UsageType::CameraVideoRealTime)
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 4))
            .skip_frames(false);
        let enc = OhEncoder::with_api_config(openh264::OpenH264API::from_source(), cfg)
            .map_err(|e| {
                log::warn!("wandr-video: openh264 encoder init: {e}");
                CodecError::InitFailed
            })?;
        Ok(Self {
            enc,
            width: w,
            height: h,
            fps,
            pts: 0,
            force_keyframe: false,
            pending: VecDeque::new(),
            scratch: Vec::new(),
            resizer: fast_image_resize::Resizer::new(),
        })
    }
}

impl Encoder for H264Encoder {
    fn encode(&mut self, frame: Rgb24Frame<'_>, force_keyframe: bool) -> Result<(), CodecError> {
        let (w, h) = (self.width, self.height);
        let expected = frame.width as usize * frame.height as usize * 3;
        if frame.data.len() < expected {
            return Err(CodecError::BadFrame);
        }

        // Resize to the encode size only when needed (same policy as convert.rs).
        let rgb: &[u8] = if frame.width == w && frame.height == h {
            &frame.data[..expected]
        } else {
            use fast_image_resize::images::{Image, ImageRef};
            use fast_image_resize::PixelType;
            let src = ImageRef::new(frame.width, frame.height, &frame.data[..expected], PixelType::U8x3)
                .map_err(|_| CodecError::BadFrame)?;
            self.scratch.resize(w as usize * h as usize * 3, 0);
            let mut dst = Image::from_slice_u8(w, h, &mut self.scratch, PixelType::U8x3)
                .map_err(|_| CodecError::BadFrame)?;
            self.resizer.resize(&src, &mut dst, None).map_err(|_| CodecError::BadFrame)?;
            &self.scratch
        };

        if force_keyframe || self.force_keyframe {
            self.enc.force_intra_frame();
            self.force_keyframe = false;
        }

        // openh264 does its own RGB→I420 (its encoder takes a YUVSource).
        let src = RgbSliceU8::new(rgb, (w as usize, h as usize));
        let yuv = YUVBuffer::from_rgb8_source(src);
        let bs = self.enc.encode(&yuv).map_err(|e| {
            log::warn!("wandr-video: openh264 encode: {e}");
            CodecError::BadFrame
        })?;
        let data = bs.to_vec();
        if !data.is_empty() {
            let is_key = matches!(
                bs.frame_type(),
                openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
            );
            self.pending.push_back(Packet {
                data,
                timestamp: crate::rtp_ts(self.pts, self.fps),
                keyframe: is_key,
            });
        }
        self.pts += 1;
        Ok(())
    }

    fn next_packet(&mut self) -> Option<Packet> {
        self.pending.pop_front()
    }

    fn set_bitrate(&mut self, _bps: u32) -> Result<(), CodecError> {
        // openh264 supports live bitrate via SetOption, but the safe wrapper does
        // not expose it in 0.9. Desktop H.264 is not the congestion-controlled
        // call path (that is VP8), so this is a documented no-op for now.
        Ok(())
    }
}

// ── decoder ──────────────────────────────────────────────────────────────────

/// One decoded frame, owned as tightly-packed I420.
struct DecodedFrame {
    buf: Vec<u8>,
    w: u32,
    h: u32,
    pts_us: i64,
}

pub struct H264Decoder {
    dec: OhDecoder,
    /// Input PTS awaiting a decoded frame. Valid pairing ONLY without B-frames
    /// (decode order == presentation order) — see the module note.
    pts_fifo: VecDeque<i64>,
    /// Frames decoded since the last drain, oldest first.
    out: VecDeque<DecodedFrame>,
    /// The frame the most recent `next_frame` returned — kept alive so its borrow
    /// stays valid until the following `next_frame`.
    current: Option<DecodedFrame>,
}

unsafe impl Send for H264Decoder {}

impl H264Decoder {
    pub fn open(_p: &DecoderParams) -> Result<Self, CodecError> {
        let dec = OhDecoder::new().map_err(|e| {
            log::warn!("wandr-video: openh264 decoder init: {e}");
            CodecError::InitFailed
        })?;
        Ok(Self {
            dec,
            pts_fifo: VecDeque::new(),
            out: VecDeque::new(),
            current: None,
        })
    }

}

/// Copy a `DecodedYUV` into an owned, tightly-packed I420 frame (pts unset). Free
/// function so it borrows only the YUV — which borrows the decoder — leaving the
/// rest of `self` free to mutate once this returns.
fn copy_i420(yuv: &openh264::decoder::DecodedYUV<'_>) -> DecodedFrame {
    let (w, h) = yuv.dimensions();
    let (sy, su, sv) = yuv.strides();
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut buf = Vec::with_capacity(w * h + 2 * cw * ch);
    for row in 0..h {
        buf.extend_from_slice(&yuv.y()[row * sy..row * sy + w]);
    }
    for row in 0..ch {
        buf.extend_from_slice(&yuv.u()[row * su..row * su + cw]);
    }
    for row in 0..ch {
        buf.extend_from_slice(&yuv.v()[row * sv..row * sv + cw]);
    }
    DecodedFrame { buf, w: w as u32, h: h as u32, pts_us: 0 }
}

impl Decoder for H264Decoder {
    fn decode(&mut self, chunk: crate::Chunk<'_>) -> Result<(), CodecError> {
        self.pts_fifo.push_back(chunk.timestamp_us);
        // Copy out inside the match so the `self.dec` borrow ends before we touch
        // `self.pts_fifo` / `self.out`.
        let frame = match self.dec.decode(chunk.data) {
            Ok(Some(yuv)) => Some(copy_i420(&yuv)),
            // No frame yet (SPS/PPS only, or buffering) — the PTS stays queued.
            Ok(None) => None,
            Err(e) => {
                log::warn!("wandr-video: openh264 decode: {e}");
                self.pts_fifo.pop_back(); // this packet produced nothing
                return Err(CodecError::BadFrame);
            }
        };
        if let Some(mut f) = frame {
            f.pts_us = self.pts_fifo.pop_front().unwrap_or(0);
            self.out.push_back(f);
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), CodecError> {
        // Drain everything OpenH264 was holding. Copy all out first (the returned
        // Vec borrows `self.dec`), then pair PTS and enqueue in order.
        let frames: Vec<DecodedFrame> = match self.dec.flush_remaining() {
            Ok(yuvs) => yuvs.iter().map(copy_i420).collect(),
            Err(e) => {
                log::warn!("wandr-video: openh264 flush: {e}");
                return Ok(());
            }
        };
        for mut f in frames {
            f.pts_us = self.pts_fifo.pop_front().unwrap_or(0);
            self.out.push_back(f);
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<(), CodecError> {
        // A fresh decoder is the reliable seek: OpenH264 has no documented
        // in-place reset, and this drops all reference state. The caller feeds a
        // keyframe next.
        self.dec = OhDecoder::new().map_err(|_| CodecError::InitFailed)?;
        self.pts_fifo.clear();
        self.out.clear();
        self.current = None;
        Ok(())
    }

    fn next_frame(&mut self) -> Option<I420Ref<'_>> {
        // Move the next decoded frame into `current` so the returned borrow stays
        // valid until the following call; None when drained.
        self.current = self.out.pop_front();
        let f = self.current.as_ref()?;
        let (w, h) = (f.w, f.h);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y_len = (w * h) as usize;
        let c_len = (cw * ch) as usize;
        Some(I420Ref {
            y: &f.buf[..y_len],
            y_stride: w,
            u: &f.buf[y_len..y_len + c_len],
            u_stride: cw,
            v: &f.buf[y_len + c_len..y_len + 2 * c_len],
            v_stride: cw,
            width: w,
            height: h,
            timestamp_us: f.pts_us,
        })
    }
}
