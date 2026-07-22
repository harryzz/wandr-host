//! AV1 via dav1d (VideoLAN). The desktop software AV1 decoder — measured ~355 fps
//! at 720p (repros/av1-bench), 12x real-time and far ahead of oxideav-av1 (which
//! failed on standard matroska AV1 framing entirely).
//!
//! LICENCE: dav1d is BSD-2 — task 117's PERMISSIVE-static tier (second choice),
//! so AV1 is licence-cleaner than HEVC's LGPL libde265. Built from source and
//! linked STATICALLY: dav1d-sys's internal path meson-builds it
//! (`-Ddefault_library=static`), triggered by
//! `SYSTEM_DEPS_DAV1D_BUILD_INTERNAL=always` in the build env. No runtime `.so`.
//!
//! DECODE-ONLY. PTS is NATIVE — `send_data` takes the timestamp and
//! `Picture::timestamp()` returns it; dav1d outputs display order, so each frame
//! carries its exact container PTS (no FIFO, and the host reorder buffer no-ops).

use std::collections::VecDeque;

use dav1d::{Decoder as Dav1dDecoder, PixelLayout, PlanarImageComponent};

use crate::{
    BackendKind, Codec, CodecBackend, CodecError, Decoder, DecoderParams, Encoder, EncoderParams, Frame,
    I420Ref,
};

// ── backend descriptor ───────────────────────────────────────────────────────

pub struct Dav1dBackend;

impl CodecBackend for Dav1dBackend {
    fn name(&self) -> &'static str {
        "dav1d"
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Software
    }
    fn priority(&self) -> u32 {
        50 // below HW (~10), above any future pure-Rust AV1 fallback (100)
    }
    fn supports_decode(&self, codec: Codec) -> bool {
        codec == Codec::Av1
    }
    fn supports_encode(&self, _codec: Codec) -> bool {
        false
    }
    fn open_decoder(&self, p: &DecoderParams) -> Result<Box<dyn Decoder>, CodecError> {
        Ok(Box::new(Av1Decoder::new(p)?))
    }
    fn open_encoder(&self, _p: &EncoderParams) -> Result<Box<dyn Encoder>, CodecError> {
        Err(CodecError::Unsupported)
    }
}

// ── decoder ──────────────────────────────────────────────────────────────────

struct DecodedFrame {
    buf: Vec<u8>,
    w: u32,
    h: u32,
    pts_us: i64,
}

pub struct Av1Decoder {
    dec: Dav1dDecoder,
    out: VecDeque<DecodedFrame>,
    current: Option<DecodedFrame>,
}

unsafe impl Send for Av1Decoder {}

impl Av1Decoder {
    fn new(_p: &DecoderParams) -> Result<Self, CodecError> {
        let dec = Dav1dDecoder::new().map_err(|e| {
            log::warn!("wandr-video: dav1d init: {e}");
            CodecError::InitFailed
        })?;
        Ok(Self { dec, out: VecDeque::new(), current: None })
    }

    /// Drain every ready picture into `out` as tightly-packed 8-bit I420.
    fn drain(&mut self) {
        loop {
            match self.dec.get_picture() {
                Ok(pic) => {
                    if !matches!(pic.pixel_layout(), PixelLayout::I420) || pic.bit_depth() != 8 {
                        log::warn!("wandr-video: dav1d dropping non-8-bit-4:2:0 frame");
                        continue;
                    }
                    let (w, h) = (pic.width() as usize, pic.height() as usize);
                    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
                    let mut buf = Vec::with_capacity(w * h + 2 * cw * ch);
                    for (comp, pw, ph) in [
                        (PlanarImageComponent::Y, w, h),
                        (PlanarImageComponent::U, cw, ch),
                        (PlanarImageComponent::V, cw, ch),
                    ] {
                        let stride = pic.stride(comp) as usize;
                        let plane = pic.plane(comp); // owns the mapping; keep it alive
                        let data: &[u8] = plane.as_ref();
                        for row in 0..ph {
                            buf.extend_from_slice(&data[row * stride..row * stride + pw]);
                        }
                    }
                    self.out.push_back(DecodedFrame {
                        buf,
                        w: w as u32,
                        h: h as u32,
                        pts_us: pic.timestamp().unwrap_or(0),
                    });
                }
                Err(e) if e.is_again() => break, // no more ready pictures
                Err(_) => break,
            }
        }
    }
}

impl Decoder for Av1Decoder {
    fn decode(&mut self, chunk: crate::Chunk<'_>) -> Result<(), CodecError> {
        // send_data wants an owned, Send buffer; the pts rides through to the
        // picture. Err(Again) = input queue full → drain, then retry.
        let buf = chunk.data.to_vec();
        loop {
            match self.dec.send_data(buf.clone(), None, Some(chunk.timestamp_us), None) {
                Ok(()) => break,
                Err(e) if e.is_again() => self.drain(),
                Err(e) => {
                    log::warn!("wandr-video: dav1d send_data: {e}");
                    return Err(CodecError::BadFrame);
                }
            }
        }
        self.drain();
        Ok(())
    }

    fn flush(&mut self) -> Result<(), CodecError> {
        // No dav1d flush() here — that DISCARDS. At EOS the buffered pictures are
        // released by draining get_picture until Again.
        self.drain();
        Ok(())
    }

    fn reset(&mut self) -> Result<(), CodecError> {
        // Seek: dav1d's flush() drops all pending input/output state. The caller
        // feeds a keyframe next.
        self.dec.flush();
        self.out.clear();
        self.current = None;
        Ok(())
    }

    fn next_frame(&mut self) -> Option<Frame<'_>> {
        self.current = self.out.pop_front();
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
