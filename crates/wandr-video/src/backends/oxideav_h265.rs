//! Software H.265 / HEVC via `oxideav-h265` (pure-Rust, MIT). The desktop
//! software decoder for H.265; Android decodes HEVC in hardware via MediaCodec
//! (`OMX.qcom.video.decoder.hevc`, present on the device — measured).
//!
//! WHY oxideav-h265 and not libde265/rust_h265: task 117 M2 flagged the H.265
//! software gap as real — `libde265` is LGPL (the licence we escaped) and
//! `rust_h265` is v0.1. `oxideav-h265` is a pure-Rust MIT decoder at ~99%
//! conformance by its README, and the spike decoded a real HEVC file 300/300
//! (repros/oxideav-spike). We use ONLY the codec crate — oxideav's registry and
//! its (unfinished) HW backends are bypassed entirely.
//!
//! DECODE-ONLY. We do not encode HEVC (calls use VP8).
//!
//! PTS: `SequenceDecoder::take_decoded()` returns pictures in DECODE order — the
//! module doc says "output order" but that is only true of the whole-sequence
//! `finish()` path; measured, the streaming `take_decoded` emits decode order
//! (POC sequence 0,5,3,1,2,4,… for a hierarchical-B GOP). So, exactly like
//! openh264, decode order in == decode order out and we pair by FIFO. The host's
//! present queue reorders to display order (a call never reorders; that is
//! player policy). It carries no per-picture timestamp, hence the FIFO.

use std::collections::VecDeque;

use oxideav_h265::{Plane, SequenceDecoder};

use crate::{
    BackendKind, Codec, CodecBackend, CodecError, Decoder, DecoderParams, Encoder, EncoderParams,
    I420Ref,
};

// ── backend descriptor ───────────────────────────────────────────────────────

pub struct OxideH265Backend;

impl CodecBackend for OxideH265Backend {
    fn name(&self) -> &'static str {
        "oxideav-h265"
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Software
    }
    fn priority(&self) -> u32 {
        100
    }
    fn supports_decode(&self, codec: Codec) -> bool {
        codec == Codec::H265
    }
    fn supports_encode(&self, _codec: Codec) -> bool {
        false // decode-only
    }
    fn open_decoder(&self, p: &DecoderParams) -> Result<Box<dyn Decoder>, CodecError> {
        Ok(Box::new(H265Decoder::new(p)))
    }
    fn open_encoder(&self, _p: &EncoderParams) -> Result<Box<dyn Encoder>, CodecError> {
        Err(CodecError::Unsupported)
    }
}

// ── decoder ──────────────────────────────────────────────────────────────────

struct DecodedFrame {
    buf: Vec<u8>, // tightly-packed I420
    w: u32,
    h: u32,
    pts_us: i64,
}

pub struct H265Decoder {
    dec: SequenceDecoder,
    /// Fed container PTS in decode order. Output is decode order too, so each
    /// output picture takes the front — see the module note.
    pts: VecDeque<i64>,
    out: VecDeque<DecodedFrame>,
    current: Option<DecodedFrame>,
}

unsafe impl Send for H265Decoder {}

impl H265Decoder {
    fn new(_p: &DecoderParams) -> Self {
        H265Decoder {
            dec: SequenceDecoder::new(),
            pts: VecDeque::new(),
            out: VecDeque::new(),
            current: None,
        }
    }

    /// Drain whatever the decoder has produced, converting each output picture to
    /// tightly-packed 8-bit I420 and pairing it with the front PTS (decode order).
    fn drain(&mut self) {
        for frame in self.dec.take_decoded() {
            if !frame.output {
                continue; // decoded-but-not-output (reference only)
            }
            let pic = &frame.picture;
            // Only 8-bit 4:2:0 is handled — the common playback case (HEVC Main).
            // Higher bit depths / 4:2:2 / 4:4:4 would need scaling/repack; drop
            // them rather than misinterpret, as the libvpx backend does for VP9.
            if pic.chroma_array_type() != 1
                || pic.bit_depth_luma() != 8
                || pic.bit_depth_chroma() != 8
            {
                log::warn!(
                    "wandr-video: dropping HEVC frame (chroma={} depth={}/{}) — only 8-bit 4:2:0",
                    pic.chroma_array_type(),
                    pic.bit_depth_luma(),
                    pic.bit_depth_chroma()
                );
                let _ = self.pts.pop_front();
                continue;
            }
            let (w, h) = pic.plane_dims(Plane::Luma);
            let (cw, ch) = pic.plane_dims(Plane::Cb);
            let mut buf = Vec::with_capacity(w * h + 2 * cw * ch);
            // Samples are i32 (0..=255 for 8-bit); pack to u8, plane by plane.
            for &s in pic.plane(Plane::Luma) {
                buf.push(s as u8);
            }
            for &s in pic.plane(Plane::Cb) {
                buf.push(s as u8);
            }
            for &s in pic.plane(Plane::Cr) {
                buf.push(s as u8);
            }
            let pts_us = self.pts.pop_front().unwrap_or(0);
            self.out.push_back(DecodedFrame { buf, w: w as u32, h: h as u32, pts_us });
        }
    }
}

impl Decoder for H265Decoder {
    fn decode(&mut self, chunk: crate::Chunk<'_>) -> Result<(), CodecError> {
        self.pts.push_back(chunk.timestamp_us);
        match self.dec.push_annexb(chunk.data) {
            Ok(()) => {
                self.drain();
                Ok(())
            }
            Err(e) => {
                log::warn!("wandr-video: oxideav-h265 push: {e:?}");
                let _ = self.pts.pop_back(); // this AU produced nothing
                Err(CodecError::BadFrame)
            }
        }
    }

    fn flush(&mut self) -> Result<(), CodecError> {
        if let Err(e) = self.dec.flush() {
            log::warn!("wandr-video: oxideav-h265 flush: {e:?}");
        }
        self.drain();
        Ok(())
    }

    fn reset(&mut self) -> Result<(), CodecError> {
        // Fresh decoder = the reliable seek; the caller feeds a keyframe next.
        self.dec = SequenceDecoder::new();
        self.pts.clear();
        self.out.clear();
        self.current = None;
        Ok(())
    }

    fn next_frame(&mut self) -> Option<I420Ref<'_>> {
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
