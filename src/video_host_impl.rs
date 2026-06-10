//! `wandr:video` host impl (task 93 Phase 1) — wraps the `video.rs` NDK
//! backend (camera + HW AMediaCodec) in the WIT resources. Encoder/decoder
//! handles are host resources in `HostState.table`; dropping the WIT resource
//! (or the guest's whole store) runs the backend's ordered camera/codec
//! teardown — the cameraserver-wedge guarantee.
//!
//! Phase 1 scope: encoder (camera self-view → pulled VP8) + decoder
//! decode-to-buffer. `set-rect` is recorded but not yet composited — the
//! decode-to-surface arbiter `Role::Video` path is Phase 4.

use wasmtime::component::Resource;

use crate::video;
use crate::video_host_bindings::wandr::video as wit;
use crate::HostState;

// ── resource backing structs (mapped via bindgen `with`) ─────────────────────

pub struct EncoderState(pub video::VideoEncoder);
pub struct DecoderState {
    pub dec: video::VideoDecoder,
    /// Display geometry for Phase 4 compositing; stored, not yet applied.
    pub rect: wit::types::VideoRect,
}

// ── conversions (WIT bindgen ↔ video.rs) ─────────────────────────────────────

fn codec2b(c: wit::types::Codec) -> Result<video::Codec, wit::types::VideoError> {
    match c {
        wit::types::Codec::Vp8 => Ok(video::Codec::Vp8),
        wit::types::Codec::Vp9 => Ok(video::Codec::Vp9),
        // No HW H264/H265 path wired (and Signal negotiates VP8/VP9).
        wit::types::Codec::H264 | wit::types::Codec::H265 => {
            Err(wit::types::VideoError::UnsupportedCodec)
        }
    }
}

fn err2w(e: video::VideoError) -> wit::types::VideoError {
    use wit::types::VideoError as W;
    match e {
        video::VideoError::UnsupportedCodec => W::UnsupportedCodec,
        video::VideoError::NoHwCodec => W::NoHwCodec,
        video::VideoError::CodecInitFailed => W::CodecInitFailed,
        video::VideoError::BadFrame => W::BadFrame,
        video::VideoError::QueueFull => W::QueueFull,
        video::VideoError::SurfaceUnavailable => W::SurfaceUnavailable,
    }
}

// ── interface Host markers + resource impls ──────────────────────────────────

impl wit::types::Host for HostState {}

impl wit::encoder::Host for HostState {}
impl wit::encoder::HostVideoEncoder for HostState {
    fn open(
        &mut self,
        config: wit::types::EncoderConfig,
    ) -> Result<Resource<EncoderState>, wit::types::VideoError> {
        let cfg = video::EncoderConfig {
            codec: codec2b(config.codec)?,
            width: config.width,
            height: config.height,
            bitrate_bps: config.bitrate_bps,
            framerate: config.framerate,
            facing_front: matches!(config.facing, wit::types::CameraFacing::Front),
        };
        if !config.source_camera {
            // Guest-supplied YUV (screen-share) is a future mode.
            return Err(wit::types::VideoError::UnsupportedCodec);
        }
        let enc = video::VideoEncoder::open(&cfg).map_err(err2w)?;
        self.table
            .push(EncoderState(enc))
            .map_err(|_| wit::types::VideoError::CodecInitFailed)
    }

    fn next_frame(&mut self, self_: Resource<EncoderState>) -> Option<wit::types::EncodedFrame> {
        let st = self.table.get_mut(&self_).ok()?;
        st.0.next_frame().map(|f| wit::types::EncodedFrame {
            data: f.data,
            timestamp: f.timestamp,
            keyframe: f.keyframe,
        })
    }

    fn request_keyframe(&mut self, self_: Resource<EncoderState>) {
        if let Ok(st) = self.table.get_mut(&self_) {
            st.0.request_keyframe();
        }
    }

    fn set_bitrate(&mut self, self_: Resource<EncoderState>, bitrate_bps: u32) {
        if let Ok(st) = self.table.get_mut(&self_) {
            st.0.set_bitrate(bitrate_bps);
        }
    }

    fn drop(&mut self, rep: Resource<EncoderState>) -> wasmtime::Result<()> {
        self.table.delete(rep)?; // VideoEncoder::drop = ordered camera/codec teardown
        Ok(())
    }
}

impl wit::decoder::Host for HostState {}
impl wit::decoder::HostVideoDecoder for HostState {
    fn open(
        &mut self,
        config: wit::types::DecoderConfig,
    ) -> Result<Resource<DecoderState>, wit::types::VideoError> {
        let cfg = video::DecoderConfig {
            codec: codec2b(config.codec)?,
            width: config.width,
            height: config.height,
        };
        let dec = video::VideoDecoder::open(&cfg).map_err(err2w)?;
        self.table
            .push(DecoderState { dec, rect: config.rect })
            .map_err(|_| wit::types::VideoError::CodecInitFailed)
    }

    fn submit(
        &mut self,
        self_: Resource<DecoderState>,
        frame: wit::types::EncodedFrame,
    ) -> Result<(), wit::types::VideoError> {
        let st = self
            .table
            .get_mut(&self_)
            .map_err(|_| wit::types::VideoError::BadFrame)?;
        st.dec.submit(&frame.data, frame.timestamp).map_err(err2w)
    }

    fn set_rect(&mut self, self_: Resource<DecoderState>, rect: wit::types::VideoRect) {
        if let Ok(st) = self.table.get_mut(&self_) {
            st.rect = rect;
        }
    }

    fn ready(&mut self, self_: Resource<DecoderState>) -> bool {
        self.table
            .get(&self_)
            .map(|st| st.dec.decoded_frames() > 0)
            .unwrap_or(false)
    }

    fn decoded_frames(&mut self, self_: Resource<DecoderState>) -> u64 {
        self.table
            .get(&self_)
            .map(|st| st.dec.decoded_frames())
            .unwrap_or(0)
    }

    fn drop(&mut self, rep: Resource<DecoderState>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}
