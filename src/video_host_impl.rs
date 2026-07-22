//! `wandr:video` host impl (task 93 Phase 1) — wraps the `video.rs` NDK
//! backend (camera + HW AMediaCodec) in the WIT resources. Encoder/decoder
//! handles are host resources in `HostState.table`; dropping the WIT resource
//! (or the guest's whole store) runs the backend's ordered camera/codec
//! teardown — the cameraserver-wedge guarantee.
//!
//! Phase 1 scope: encoder (camera self-view → pulled VP8) + decoder
//! decode-to-buffer. Phase 4 shipped decode-to-surface as CHILD surfaces of the
//! app's own surface (the SurfaceView model) — NOT the arbiter `Role::Video`
//! this comment used to promise, which was decided against; see the design
//! decision on `world video-client` in contracts/wit/video.wit.

use wasmtime::component::Resource;

use crate::video;
use crate::video_host_bindings::wandr::video as wit;
use crate::HostState;

// ── resource backing structs (mapped via bindgen `with`) ─────────────────────

pub struct EncoderState(pub video::VideoEncoder);
pub struct DecoderState(pub video::VideoDecoder);

// ── conversions (WIT bindgen ↔ video.rs) ─────────────────────────────────────

fn codec2b(c: wit::types::Codec) -> Result<video::Codec, wit::types::VideoError> {
    match c {
        wit::types::Codec::Vp8 => Ok(video::Codec::Vp8),
        wit::types::Codec::Vp9 => Ok(video::Codec::Vp9),
        // Desktop software (openh264 / oxideav-h265) or Android MediaCodec HW.
        wit::types::Codec::H264 => Ok(video::Codec::H264),
        wit::types::Codec::H265 => Ok(video::Codec::H265),
        // Playback-only (task 117 M2): dav1d on desktop, MediaCodec on Android.
        wit::types::Codec::Av1 => Ok(video::Codec::Av1),
    }
}

/// wandr-video codec -> WIT codec. `None` for anything the WIT vocabulary does
/// not name, so a new backend codec cannot silently masquerade as another.
#[cfg(not(target_os = "android"))]
fn codec2w(c: wandr_video::Codec) -> Option<wit::types::Codec> {
    Some(match c {
        wandr_video::Codec::Vp8 => wit::types::Codec::Vp8,
        wandr_video::Codec::Vp9 => wit::types::Codec::Vp9,
        wandr_video::Codec::H264 => wit::types::Codec::H264,
        wandr_video::Codec::H265 => wit::types::Codec::H265,
        wandr_video::Codec::Av1 => wit::types::Codec::Av1,
    })
}

#[cfg(not(target_os = "android"))]
fn accel2b(a: wit::decoder::Acceleration) -> video::Accel {
    match a {
        wit::decoder::Acceleration::NoPreference => video::Accel::NoPreference,
        wit::decoder::Acceleration::PreferHardware => video::Accel::PreferHardware,
        wit::decoder::Acceleration::PreferSoftware => video::Accel::PreferSoftware,
        wit::decoder::Acceleration::RequireHardware => video::Accel::RequireHardware,
    }
}

fn layer2b(l: wit::types::ZLayer) -> video::ZLayer {
    match l {
        wit::types::ZLayer::BehindUi => video::ZLayer::BehindUi,
        wit::types::ZLayer::AboveUi => video::ZLayer::AboveUi,
    }
}

fn rect2b(r: wit::types::VideoRect) -> video::VideoRect {
    video::VideoRect {
        x: r.x as i32,
        y: r.y as i32,
        w: r.width as i32,
        h: r.height as i32,
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
            preview: config.preview.map(rect2b),
            preview_layer: layer2b(config.preview_layer),
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

    fn set_preview_rect(&mut self, self_: Resource<EncoderState>, rect: wit::types::VideoRect) {
        if let Ok(st) = self.table.get_mut(&self_) {
            st.0.set_preview_rect(rect2b(rect));
        }
    }

    fn set_preview_visible(&mut self, self_: Resource<EncoderState>, visible: bool) {
        if let Ok(st) = self.table.get_mut(&self_) {
            st.0.set_preview_visible(visible);
        }
    }

    fn display_rotation(&mut self, self_: Resource<EncoderState>) -> u32 {
        self.table.get(&self_).map(|st| st.0.display_rotation()).unwrap_or(0)
    }

    fn drop(&mut self, rep: Resource<EncoderState>) -> wasmtime::Result<()> {
        self.table.delete(rep)?; // VideoEncoder::drop = ordered camera/codec teardown
        Ok(())
    }
}

impl wit::decoder::Host for HostState {
    /// PROBED per call, not cached at startup: a driver can appear or disappear
    /// (a GPU reset, a container gaining /dev/dri) and a guest asking "can you
    /// decode this" deserves the current answer. The probe itself is cached inside
    /// the backend, so this is cheap after the first call.
    fn list_decoders(&mut self) -> Vec<wit::decoder::DecoderInfo> {
        #[cfg(not(target_os = "android"))]
        {
            wandr_video::describe_backends()
                .into_iter()
                .flat_map(|b| {
                    let (name, hardware) = (b.name.to_string(), b.is_hardware());
                    b.decode.into_iter().filter_map(move |c| {
                        Some(wit::decoder::DecoderInfo {
                            codec: codec2w(c)?,
                            name: name.clone(),
                            hardware,
                        })
                    })
                })
                .collect()
        }
        // Android decodes everything through MediaCodec, which IS the hardware
        // path; there is no registry to enumerate and no software alternative
        // linked in. Reporting the codec set it supports would mean asking
        // MediaCodec, which is a bigger job than this verb is worth today —
        // an empty list honestly says "not enumerable here" rather than lying.
        #[cfg(target_os = "android")]
        {
            Vec::new()
        }
    }
}
impl wit::decoder::HostVideoDecoder for HostState {
    fn open(
        &mut self,
        config: wit::types::DecoderConfig,
    ) -> Result<Resource<DecoderState>, wit::types::VideoError> {
        self.open_accelerated(config, wit::decoder::Acceleration::NoPreference)
    }

    fn open_accelerated(
        &mut self,
        config: wit::types::DecoderConfig,
        accel: wit::decoder::Acceleration,
    ) -> Result<Resource<DecoderState>, wit::types::VideoError> {
        let cfg = video::DecoderConfig {
            codec: codec2b(config.codec)?,
            width: config.width,
            height: config.height,
            // An empty rect = decode-to-buffer (the backend filters it).
            rect: Some(rect2b(config.rect)),
            rotation: config.rotation,
            layer: layer2b(config.layer),
        };
        #[cfg(not(target_os = "android"))]
        let dec = video::VideoDecoder::open_with_accel(&cfg, accel2b(accel)).map_err(err2w)?;
        // Android is MediaCodec-only: every decoder there IS the hardware path, so
        // a preference has nothing to choose between and is accepted as satisfied.
        #[cfg(target_os = "android")]
        let dec = {
            let _ = accel;
            video::VideoDecoder::open(&cfg).map_err(err2w)?
        };
        self.table
            .push(DecoderState(dec))
            .map_err(|_| wit::types::VideoError::CodecInitFailed)
    }

    fn implementation(&mut self, self_: Resource<DecoderState>) -> wit::decoder::DecoderInfo {
        #[cfg(not(target_os = "android"))]
        {
            let (name, hardware) = self
                .table
                .get(&self_)
                .map(|st| st.0.backend())
                .unwrap_or(("unknown", false));
            wit::decoder::DecoderInfo {
                codec: wit::types::Codec::H264,
                name: name.to_string(),
                hardware,
            }
        }
        #[cfg(target_os = "android")]
        {
            let _ = self_;
            wit::decoder::DecoderInfo {
                codec: wit::types::Codec::H264,
                name: "mediacodec".to_string(),
                hardware: true,
            }
        }
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
        st.0.submit(&frame.data, frame.timestamp).map_err(err2w)
    }

    fn set_rect(&mut self, self_: Resource<DecoderState>, rect: wit::types::VideoRect) {
        if let Ok(st) = self.table.get_mut(&self_) {
            st.0.set_rect(rect2b(rect));
        }
    }

    fn set_visible(&mut self, self_: Resource<DecoderState>, visible: bool) {
        if let Ok(st) = self.table.get_mut(&self_) {
            st.0.set_visible(visible);
        }
    }

    fn set_rotation(&mut self, self_: Resource<DecoderState>, degrees: u32) {
        if let Ok(st) = self.table.get_mut(&self_) {
            st.0.set_rotation(degrees);
        }
    }

    fn ready(&mut self, self_: Resource<DecoderState>) -> bool {
        self.table
            .get(&self_)
            .map(|st| st.0.decoded_frames() > 0)
            .unwrap_or(false)
    }

    fn decoded_frames(&mut self, self_: Resource<DecoderState>) -> u64 {
        self.table
            .get(&self_)
            .map(|st| st.0.decoded_frames())
            .unwrap_or(0)
    }

    // ── PLAYBACK (task 117 M2 stage 1) ───────────────────────────────────────

    fn submit_timed(
        &mut self,
        self_: Resource<DecoderState>,
        frame: wit::types::TimedFrame,
    ) -> Result<(), wit::types::VideoError> {
        let st = self
            .table
            .get_mut(&self_)
            .map_err(|_| wit::types::VideoError::BadFrame)?;
        st.0.submit_for_playback(&frame.data, frame.timestamp_us)
            .map_err(err2w)
    }

    fn next_decoded(
        &mut self,
        self_: Resource<DecoderState>,
    ) -> Option<Resource<DecodedFrameState>> {
        let taken = self.table.get_mut(&self_).ok()?.0.take_next_decoded()?;
        // If the table is full the frame is dropped rather than leaked — the
        // guest simply sees `none` and retries.
        self.table.push(DecodedFrameState(Some(taken))).ok()
    }

    fn flush(&mut self, self_: Resource<DecoderState>) -> Result<(), wit::types::VideoError> {
        let st = self
            .table
            .get_mut(&self_)
            .map_err(|_| wit::types::VideoError::BadFrame)?;
        st.0.finish_playback().map_err(err2w)
    }

    fn reset(&mut self, self_: Resource<DecoderState>) -> Result<(), wit::types::VideoError> {
        let st = self
            .table
            .get_mut(&self_)
            .map_err(|_| wit::types::VideoError::BadFrame)?;
        st.0.seek_reset().map_err(err2w)
    }

    fn drop(&mut self, rep: Resource<DecoderState>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

/// The `decoded-frame` resource: one decoded frame the guest holds while it
/// decides when to show it. `Option` because `present`/`discard` consume the
/// frame but WIT resource methods take `&self` — taking it out leaves the
/// handle inert, and a later `drop` is then a no-op rather than a double-free.
pub struct DecodedFrameState(Option<video::TakenFrame>);

impl wit::decoder::HostDecodedFrame for HostState {
    fn timestamp_us(&mut self, self_: Resource<DecodedFrameState>) -> i64 {
        self.table
            .get(&self_)
            .ok()
            .and_then(|s| s.0.as_ref().map(|f| f.timestamp_us()))
            .unwrap_or(0)
    }

    fn present(&mut self, self_: Resource<DecodedFrameState>, at_ns: u64) {
        if let Ok(st) = self.table.get_mut(&self_) {
            if let Some(frame) = st.0.take() {
                video::schedule_present(at_ns, frame);
            }
        }
    }

    fn discard(&mut self, self_: Resource<DecodedFrameState>) {
        if let Ok(st) = self.table.get_mut(&self_) {
            st.0 = None; // released without painting
        }
    }

    /// Dropping without `present` or `discard` is equivalent to `discard` — the
    /// buffer goes with the state, so a frame can never be leaked.
    fn drop(&mut self, rep: Resource<DecodedFrameState>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}
