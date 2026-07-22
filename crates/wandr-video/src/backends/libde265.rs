//! Software H.265 / HEVC via libde265 (Struktur AG). The real-time desktop
//! software decoder — measured ~60 fps at 720p single-threaded, ~107 fps
//! multi-threaded, vs ~4 fps for the pure-Rust oxideav-h265 (repros/libde265-bench).
//!
//! LICENCE: libde265 is LGPL-2.1. Per task 117's preference order
//! (pure-Rust → permissive-static → LGPL → HW-only) LGPL is third choice, allowed
//! for a real need — and real-time software HEVC is one. It is built from source
//! and linked STATICALLY (the `static` feature of libde265-sys compiles the
//! vendored C with `cc`), so there is no runtime `.so`.
//!   ‼️ STATIC LGPL carries a relink obligation (LGPL §6): a distributed binary
//!   must let a user relink against a modified libde265. wandr is open-source
//!   (Apache-2.0, buildable from source), which satisfies the spirit, but the
//!   packaging story (object files / a shared-link option) is a task-118 concern.
//!   If that proves awkward, switch libde265-sys to dynamic (`system`) or a
//!   `libloading` bridge — the backend code here is unchanged either way.
//!
//! DECODE-ONLY. PTS is NATIVE here: `push_data` takes the timestamp and
//! `get_image_pts` hands it back on the decoded picture, and libde265 outputs in
//! DISPLAY order — so no FIFO or reorder guessing, each frame carries its exact
//! container PTS. (The host's reorder buffer sees already-sorted input and no-ops.)

use std::collections::VecDeque;
use std::sync::Arc;

use libde265::{ChromaFormat, De265, Decoder as De265Decoder};

use crate::{
    BackendKind, Codec, CodecBackend, CodecError, Decoder, DecoderParams, Encoder, EncoderParams, Frame,
    I420Ref,
};

// ── backend descriptor ───────────────────────────────────────────────────────

pub struct Libde265Backend;

impl CodecBackend for Libde265Backend {
    fn name(&self) -> &'static str {
        "libde265"
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Software
    }
    fn priority(&self) -> u32 {
        // Below any HW backend (~10) but ABOVE the pure-Rust oxideav-h265 (100):
        // when both software HEVC decoders are compiled in, the real-time one wins.
        50
    }
    fn supports_decode(&self, codec: Codec) -> bool {
        codec == Codec::H265
    }
    fn supports_encode(&self, _codec: Codec) -> bool {
        false
    }
    fn open_decoder(&self, p: &DecoderParams) -> Result<Box<dyn Decoder>, CodecError> {
        Ok(Box::new(H265Decoder::new(p)?))
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

pub struct H265Decoder {
    session: Arc<De265>,
    dec: De265Decoder,
    out: VecDeque<DecodedFrame>,
    current: Option<DecodedFrame>,
}

// libde265's Decoder is a heap object with internal worker threads it owns; the
// handle itself is only touched from the store thread. Safe to move, not shared.
unsafe impl Send for H265Decoder {}

impl H265Decoder {
    fn new(_p: &DecoderParams) -> Result<Self, CodecError> {
        let session = De265::new().map_err(|e| {
            log::warn!("wandr-video: libde265 session: {e}");
            CodecError::InitFailed
        })?;
        let mut dec = De265Decoder::new(session.clone());
        // A modest thread pool: real-time even single-threaded, and this keeps
        // headroom for the store thread. 0 threads = synchronous decode.
        let n = std::thread::available_parallelism().map(|n| (n.get() as u32).min(4)).unwrap_or(1);
        let _ = dec.start_worker_threads(n);
        Ok(Self { session, dec, out: VecDeque::new(), current: None })
    }

    /// Drain every displayable picture libde265 has ready into `out`, converting
    /// to tightly-packed 8-bit I420 and reading each frame's native PTS.
    fn drain(&mut self) {
        while let Some(img) = self.dec.get_next_picture() {
            if !matches!(img.get_chroma_format(), ChromaFormat::Chroma420)
                || img.get_bits_per_pixel(0) != 8
            {
                log::warn!("wandr-video: libde265 dropping non-8-bit-4:2:0 frame");
                continue;
            }
            let (w, h) = (img.get_image_width(0) as usize, img.get_image_height(0) as usize);
            let (cw, ch) = (img.get_image_width(1) as usize, img.get_image_height(1) as usize);
            let mut buf = Vec::with_capacity(w * h + 2 * cw * ch);
            // Each plane is (bytes, stride); stride ≥ width, so repack tightly.
            for (ch_idx, pw, ph) in [(0, w, h), (1, cw, ch), (2, cw, ch)] {
                let (data, stride) = img.get_image_plane(ch_idx);
                for row in 0..ph {
                    buf.extend_from_slice(&data[row * stride..row * stride + pw]);
                }
            }
            self.out.push_back(DecodedFrame {
                buf,
                w: w as u32,
                h: h as u32,
                pts_us: img.get_image_pts(),
            });
        }
    }
}

impl Decoder for H265Decoder {
    fn decode(&mut self, chunk: crate::Chunk<'_>) -> Result<(), CodecError> {
        // The pts rides through libde265 and comes back on the picture.
        if let Err(e) = self.dec.push_data(chunk.data, chunk.timestamp_us, None) {
            log::warn!("wandr-video: libde265 push_data: {e}");
            return Err(CodecError::BadFrame);
        }
        if let Err(e) = self.dec.decode() {
            log::warn!("wandr-video: libde265 decode: {e}");
            return Err(CodecError::BadFrame);
        }
        self.drain();
        Ok(())
    }

    fn flush(&mut self) -> Result<(), CodecError> {
        let _ = self.dec.flush_data();
        // At EOS libde265 needs decode() pumped to release the held pictures.
        for _ in 0..64 {
            if self.dec.decode().is_err() {
                break;
            }
            self.drain();
            if self.dec.get_number_of_input_bytes_pending() == 0 {
                self.drain();
                break;
            }
        }
        self.drain();
        Ok(())
    }

    fn reset(&mut self) -> Result<(), CodecError> {
        // Seek: drop reference/DPB state. The caller feeds a keyframe next.
        self.dec.reset();
        self.out.clear();
        self.current = None;
        let _ = &self.session; // kept alive alongside the decoder
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
