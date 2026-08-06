//! wandr:video EMBEDDER host impl (task 120) — the fused `wandr:video` split into
//! `wasi:video-codec` (codec BASIC), `wasi:camera` (source types), `wasi:eme` (DRM
//! control, stubbed), and `wandr:video` (the embedder: `present.video-surface` +
//! `capture-encode.call-encoder`). ALL served over the SAME `video.rs` /
//! `video_desktop.rs` backend — this is a re-binding, not new capability:
//!
//!   * `wasi:video-codec` decoder = the backend decoder in decode-to-BUFFER mode
//!     (surface-free, content-bearing `frame`s via `next-decoded`).
//!   * `wandr:video` `video-surface` = an embedder-owned child surface; `present`
//!     retargets a decoded `frame` onto it (desktop) / releases the buffer to the
//!     bound surface (android); `attach` binds a decoder for the AUTO/RTP path.
//!   * `wandr:video` `call-encoder` = the fused camera+encode+PiP `VideoEncoder`.
//!   * diagnostics (`wandr:video-diag`) keep list-decoders / implementation /
//!     decoded-frames off the standard.
//!
//! Handles are host resources in `HostState.table`; dropping a resource runs the
//! backend's ordered camera/codec/surface teardown.

use wasmtime::component::Resource;

use crate::video;
use crate::video_host_bindings::wandr::video as wandr_wit;
use crate::video_host_bindings::wasi::camera as camera_wit;
use crate::video_host_bindings::wasi::eme::eme as eme_wit;
use crate::video_host_bindings::wasi::video_codec as codec_wit;
use crate::video_diag_bindings::wandr::video_diag::diag as diag_wit;
use crate::HostState;

// ── host-derived keep-awake (unchanged from task 93) ─────────────────────────
// A foreground app actively presenting video should hold the screen awake — a
// video playing IS "use". DERIVED from real runtime state: every presented frame
// from the FOREGROUND host pokes the arbiter's `user-activity` (throttled).
#[cfg(target_os = "android")]
const KEEPAWAKE_POKE_EVERY: std::time::Duration = std::time::Duration::from_secs(15);
#[cfg(target_os = "android")]
static KEEPAWAKE_LAST: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);

#[cfg(target_os = "android")]
fn keepawake_on_present() {
    if crate::app_role::role() != crate::app_role::AppRole::Foreground {
        return;
    }
    {
        let now = std::time::Instant::now();
        let mut last = KEEPAWAKE_LAST.lock().unwrap_or_else(|e| e.into_inner());
        match *last {
            Some(t) if now.duration_since(t) < KEEPAWAKE_POKE_EVERY => return,
            _ => *last = Some(now),
        }
    }
    use std::io::Write;
    if let Ok(mut s) =
        crate::arbiter_sock::UnixStream::connect(crate::arbiter_sock::arbiter_sock_path())
    {
        let _ = s.write_all(b"user-activity\n");
        let _ = s.flush();
        let _ = s.shutdown(std::net::Shutdown::Write);
    }
}

#[cfg(not(target_os = "android"))]
fn keepawake_on_present() {}

// ── resource backing structs (mapped via bindgen `with`) ─────────────────────

/// `wasi:video-codec` `frame` — the shared opaque VideoFrame (decoder output /
/// camera capture / encoder input). `Option` so `present` (which consumes it) can
/// leave the handle inert, making a later `drop` a no-op rather than a double-free.
pub struct FrameState(pub Option<video::TakenFrame>);

/// `wasi:video-codec` `video-decoder` — the codec, decode-to-BUFFER (surface-free).
pub struct DecoderState(pub video::VideoDecoder);

/// `wasi:video-codec` `video-encoder` — raw frame-source encode. No proof consumer
/// yet (Signal uses the fused `call-encoder`); advertised `unsupported` until a
/// screen-share / guest-frame encoder ships.
pub struct CodecEncoderState;

/// `wandr:video` `present.video-surface` — an embedder-owned child surface.
pub struct VideoSurfaceState {
    /// Backend surface id (desktop registry / android child-surface slot).
    id: u32,
}

/// `wandr:video` `capture-encode.call-encoder` — the fused camera+encode+PiP path.
pub struct CallEncoderState(pub video::VideoEncoder);

// ── conversions (WIT bindgen ↔ video.rs) ─────────────────────────────────────

fn codec2b(c: codec_wit::types::Codec) -> Result<video::Codec, codec_wit::types::CodecError> {
    match c {
        codec_wit::types::Codec::Vp8 => Ok(video::Codec::Vp8),
        codec_wit::types::Codec::Vp9 => Ok(video::Codec::Vp9),
        codec_wit::types::Codec::H264 => Ok(video::Codec::H264),
        codec_wit::types::Codec::H265 => Ok(video::Codec::H265),
        codec_wit::types::Codec::Av1 => Ok(video::Codec::Av1),
    }
}

/// wandr-video codec -> WIT codec (diagnostics). `None` for anything the WIT
/// vocabulary does not name.
#[cfg(not(target_os = "android"))]
fn codec2w(c: wandr_video::Codec) -> Option<codec_wit::types::Codec> {
    Some(match c {
        wandr_video::Codec::Vp8 => codec_wit::types::Codec::Vp8,
        wandr_video::Codec::Vp9 => codec_wit::types::Codec::Vp9,
        wandr_video::Codec::H264 => codec_wit::types::Codec::H264,
        wandr_video::Codec::H265 => codec_wit::types::Codec::H265,
        wandr_video::Codec::Av1 => codec_wit::types::Codec::Av1,
    })
}

#[cfg(not(target_os = "android"))]
fn accel2b(a: codec_wit::types::Acceleration) -> video::Accel {
    match a {
        codec_wit::types::Acceleration::NoPreference => video::Accel::NoPreference,
        codec_wit::types::Acceleration::PreferHardware => video::Accel::PreferHardware,
        codec_wit::types::Acceleration::PreferSoftware => video::Accel::PreferSoftware,
        codec_wit::types::Acceleration::RequireHardware => video::Accel::RequireHardware,
    }
}

fn layer2b(l: wandr_wit::types::ZLayer) -> video::ZLayer {
    match l {
        wandr_wit::types::ZLayer::BehindUi => video::ZLayer::BehindUi,
        wandr_wit::types::ZLayer::AboveUi => video::ZLayer::AboveUi,
    }
}

fn rect2b(r: wandr_wit::types::VideoRect) -> video::VideoRect {
    video::VideoRect {
        x: r.x as i32,
        y: r.y as i32,
        w: r.width as i32,
        h: r.height as i32,
    }
}

fn rect2w(r: video::VideoRect) -> wandr_wit::types::VideoRect {
    wandr_wit::types::VideoRect {
        x: r.x.max(0) as u32,
        y: r.y.max(0) as u32,
        width: r.w.max(0) as u32,
        height: r.h.max(0) as u32,
    }
}

/// backend error -> `wasi:video-codec` `codec-error` (no surface variant — the
/// codec has no surface; a stray surface error folds to `bad-frame`).
fn err2codec(e: video::VideoError) -> codec_wit::types::CodecError {
    use codec_wit::types::CodecError as C;
    match e {
        video::VideoError::UnsupportedCodec => C::UnsupportedCodec,
        video::VideoError::NoHwCodec => C::NoHwCodec,
        video::VideoError::CodecInitFailed => C::CodecInitFailed,
        video::VideoError::BadFrame => C::BadFrame,
        video::VideoError::QueueFull => C::QueueFull,
        video::VideoError::SurfaceUnavailable => C::BadFrame,
    }
}

/// backend error -> `wandr:video` `surface-error` (the embedder layer).
fn err2surf(e: video::VideoError) -> wandr_wit::types::SurfaceError {
    use wandr_wit::types::SurfaceError as S;
    match e {
        video::VideoError::SurfaceUnavailable => S::SurfaceUnavailable,
        video::VideoError::UnsupportedCodec
        | video::VideoError::NoHwCodec
        | video::VideoError::CodecInitFailed
        | video::VideoError::BadFrame
        | video::VideoError::QueueFull => S::CodecUnavailable,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// wasi:video-codec
// ═══════════════════════════════════════════════════════════════════════════

impl codec_wit::types::Host for HostState {}

impl codec_wit::types::HostFrame for HostState {
    fn timestamp_us(&mut self, self_: Resource<FrameState>) -> i64 {
        self.table
            .get(&self_)
            .ok()
            .and_then(|s| s.0.as_ref().map(|f| f.timestamp_us()))
            .unwrap_or(0)
    }
    fn width(&mut self, self_: Resource<FrameState>) -> u32 {
        self.table
            .get(&self_)
            .ok()
            .and_then(|s| s.0.as_ref().map(|f| f.width()))
            .unwrap_or(0)
    }
    fn height(&mut self, self_: Resource<FrameState>) -> u32 {
        self.table
            .get(&self_)
            .ok()
            .and_then(|s| s.0.as_ref().map(|f| f.height()))
            .unwrap_or(0)
    }
    fn rotation(&mut self, self_: Resource<FrameState>) -> u32 {
        self.table
            .get(&self_)
            .ok()
            .and_then(|s| s.0.as_ref().map(|f| f.rotation()))
            .unwrap_or(0)
    }
    fn read_rgba(&mut self, self_: Resource<FrameState>) -> Option<Vec<u8>> {
        self.table.get(&self_).ok().and_then(|s| s.0.as_ref().and_then(|f| f.read_rgba()))
    }
    fn drop(&mut self, rep: Resource<FrameState>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl codec_wit::decoder::Host for HostState {
    fn probe(&mut self, _config: codec_wit::types::DecoderConfig) -> codec_wit::types::Support {
        // A software decode path always exists on desktop (libvpx / GStreamer); the
        // real HW/SW lane is chosen at `open` by `acceleration`. Android is
        // MediaCodec = the hardware path.
        #[cfg(target_os = "android")]
        {
            codec_wit::types::Support::Hardware
        }
        #[cfg(not(target_os = "android"))]
        {
            codec_wit::types::Support::Software
        }
    }
}

impl codec_wit::decoder::HostVideoDecoder for HostState {
    fn open(
        &mut self,
        config: codec_wit::types::DecoderConfig,
        _keys: Option<Resource<KeySessionStub>>,
    ) -> Result<Resource<DecoderState>, codec_wit::types::CodecError> {
        // Surface-free: rect = None = decode-to-buffer (content-bearing frames). The
        // embedder's `video-surface` owns placement. `_keys` (wasi:eme) is ignored
        // — DRM is stubbed; a cleartext (`none`) open is all the proof apps use.
        let cfg = video::DecoderConfig {
            codec: codec2b(config.codec)?,
            width: 0,
            height: 0,
            rect: None,
            rotation: 0,
            layer: video::ZLayer::AboveUi,
        };
        #[cfg(not(target_os = "android"))]
        let dec = video::VideoDecoder::open_with_accel(&cfg, accel2b(config.acceleration))
            .map_err(err2codec)?;
        #[cfg(target_os = "android")]
        let dec = {
            let _ = config.acceleration;
            video::VideoDecoder::open(&cfg).map_err(err2codec)?
        };
        self.table
            .push(DecoderState(dec))
            .map_err(|_| codec_wit::types::CodecError::CodecInitFailed)
    }

    fn submit(
        &mut self,
        self_: Resource<DecoderState>,
        chunk: codec_wit::types::EncodedChunk,
    ) -> Result<(), codec_wit::types::CodecError> {
        let st = self
            .table
            .get_mut(&self_)
            .map_err(|_| codec_wit::types::CodecError::BadFrame)?;
        st.0.submit_for_playback(&chunk.data, chunk.timestamp_us)
            .map_err(err2codec)
    }

    fn next_decoded(&mut self, self_: Resource<DecoderState>) -> Option<Resource<FrameState>> {
        let taken = self.table.get_mut(&self_).ok()?.0.take_next_decoded()?;
        self.table.push(FrameState(Some(taken))).ok()
    }

    fn flush(&mut self, self_: Resource<DecoderState>) -> Result<(), codec_wit::types::CodecError> {
        let st = self
            .table
            .get_mut(&self_)
            .map_err(|_| codec_wit::types::CodecError::BadFrame)?;
        st.0.finish_playback().map_err(err2codec)
    }

    fn reset(&mut self, self_: Resource<DecoderState>) -> Result<(), codec_wit::types::CodecError> {
        let st = self
            .table
            .get_mut(&self_)
            .map_err(|_| codec_wit::types::CodecError::BadFrame)?;
        st.0.seek_reset().map_err(err2codec)
    }

    fn drop(&mut self, rep: Resource<DecoderState>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl codec_wit::encoder::Host for HostState {
    fn probe(&mut self, _config: codec_wit::types::EncoderConfig) -> codec_wit::types::Support {
        // Raw guest-frame encode is not offered yet (the fused `call-encoder`
        // covers the only consumer). Honest `unsupported` so a guest falls back.
        codec_wit::types::Support::Unsupported
    }
}

impl codec_wit::encoder::HostVideoEncoder for HostState {
    fn open(
        &mut self,
        _config: codec_wit::types::EncoderConfig,
    ) -> Result<Resource<CodecEncoderState>, codec_wit::types::CodecError> {
        // Raw frame-source encode deferred (no proof consumer). Signal encodes via
        // `wandr:video` `call-encoder`.
        Err(codec_wit::types::CodecError::NoHwCodec)
    }
    fn encode(
        &mut self,
        _self_: Resource<CodecEncoderState>,
        _frame: Resource<FrameState>,
    ) -> Result<(), codec_wit::types::CodecError> {
        Err(codec_wit::types::CodecError::NoHwCodec)
    }
    fn next_chunk(
        &mut self,
        _self_: Resource<CodecEncoderState>,
    ) -> Option<codec_wit::types::EncodedChunk> {
        None
    }
    fn request_keyframe(&mut self, _self_: Resource<CodecEncoderState>) {}
    fn set_bitrate(&mut self, _self_: Resource<CodecEncoderState>, _bitrate_bps: u32) {}
    fn drop(&mut self, rep: Resource<CodecEncoderState>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// wasi:camera (types only — the world imports `facing`; no capture session here,
// the fused `call-encoder` owns the camera internally)
// ═══════════════════════════════════════════════════════════════════════════

impl camera_wit::types::Host for HostState {}

// ═══════════════════════════════════════════════════════════════════════════
// wasi:eme — trapping stub (no CDM/DRM backend; proof apps pass `none`)
// ═══════════════════════════════════════════════════════════════════════════

pub struct MediaKeysStub;
pub struct KeySessionStub;

impl eme_wit::Host for HostState {
    fn request_access(
        &mut self,
        _key_system: String,
        _configs: Vec<eme_wit::KeySystemConfig>,
    ) -> Result<Resource<MediaKeysStub>, eme_wit::EmeError> {
        // No CDM implemented (ClearKey/Widevine deferred) — every key-system is
        // unsupported. The resource is therefore never constructed.
        Err(eme_wit::EmeError::UnsupportedKeySystem)
    }
}

impl eme_wit::HostMediaKeys for HostState {
    fn create_session(
        &mut self,
        _self_: Resource<MediaKeysStub>,
        _kind: eme_wit::SessionType,
    ) -> Result<Resource<KeySessionStub>, eme_wit::EmeError> {
        Err(eme_wit::EmeError::InvalidState)
    }
    fn set_server_certificate(
        &mut self,
        _self_: Resource<MediaKeysStub>,
        _cert: Vec<u8>,
    ) -> Result<bool, eme_wit::EmeError> {
        Err(eme_wit::EmeError::InvalidState)
    }
    fn drop(&mut self, rep: Resource<MediaKeysStub>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl eme_wit::HostKeySession for HostState {
    fn generate_request(
        &mut self,
        _self_: Resource<KeySessionStub>,
        _init_data_type: eme_wit::InitDataType,
        _init_data: Vec<u8>,
    ) -> Result<(), eme_wit::EmeError> {
        Err(eme_wit::EmeError::InvalidState)
    }
    fn load(
        &mut self,
        _self_: Resource<KeySessionStub>,
        _session_id: String,
    ) -> Result<bool, eme_wit::EmeError> {
        Err(eme_wit::EmeError::InvalidState)
    }
    fn take_message(
        &mut self,
        _self_: Resource<KeySessionStub>,
    ) -> Option<(eme_wit::MessageType, Vec<u8>)> {
        None
    }
    fn update(
        &mut self,
        _self_: Resource<KeySessionStub>,
        _response: Vec<u8>,
    ) -> Result<(), eme_wit::EmeError> {
        Err(eme_wit::EmeError::InvalidState)
    }
    fn key_statuses(
        &mut self,
        _self_: Resource<KeySessionStub>,
    ) -> Vec<eme_wit::KeyStatusEntry> {
        Vec::new()
    }
    fn session_id(&mut self, _self_: Resource<KeySessionStub>) -> String {
        String::new()
    }
    fn expiration(&mut self, _self_: Resource<KeySessionStub>) -> Option<u64> {
        None
    }
    fn close(&mut self, _self_: Resource<KeySessionStub>) {}
    fn remove(&mut self, _self_: Resource<KeySessionStub>) -> Result<(), eme_wit::EmeError> {
        Err(eme_wit::EmeError::InvalidState)
    }
    fn drop(&mut self, rep: Resource<KeySessionStub>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// wandr:video — the embedder
// ═══════════════════════════════════════════════════════════════════════════

impl wandr_wit::types::Host for HostState {}

impl wandr_wit::present::Host for HostState {}

impl wandr_wit::present::HostVideoSurface for HostState {
    fn open(
        &mut self,
        rect: wandr_wit::types::VideoRect,
        layer: wandr_wit::types::ZLayer,
        degrees: u32,
    ) -> Result<Resource<VideoSurfaceState>, wandr_wit::types::SurfaceError> {
        #[cfg(not(target_os = "android"))]
        let id = video::video_surface_alloc(rect2b(rect), layer2b(layer), degrees);
        #[cfg(target_os = "android")]
        let id = video::video_surface_alloc(rect2b(rect), layer2b(layer), degrees);
        self.table
            .push(VideoSurfaceState { id })
            .map_err(|_| wandr_wit::types::SurfaceError::SurfaceUnavailable)
    }

    fn attach(
        &mut self,
        self_: Resource<VideoSurfaceState>,
        dec: Resource<DecoderState>,
    ) -> Result<(), wandr_wit::types::SurfaceError> {
        let id = self
            .table
            .get(&self_)
            .map_err(|_| wandr_wit::types::SurfaceError::SurfaceUnavailable)?
            .id;
        // AUTO / RTP: bind the decoder to this surface so its auto-rendered output
        // composites here (the guest then submits and never pulls `next-decoded`).
        let st = self
            .table
            .get_mut(&dec)
            .map_err(|_| wandr_wit::types::SurfaceError::CodecUnavailable)?;
        st.0.set_surface_id(id);
        Ok(())
    }

    fn present(
        &mut self,
        self_: Resource<VideoSurfaceState>,
        frame: Resource<FrameState>,
        at_ns: u64,
    ) {
        let Ok(id) = self.table.get(&self_).map(|s| s.id) else {
            let _ = self.table.delete(frame);
            return;
        };
        // GUEST-TIMED: consume the frame and schedule it onto this surface.
        if let Ok(mut fs) = self.table.delete(frame) {
            if let Some(taken) = fs.0.take() {
                taken.present_to(id, at_ns);
                keepawake_on_present();
            }
        }
    }

    fn set_rect(&mut self, self_: Resource<VideoSurfaceState>, rect: wandr_wit::types::VideoRect) {
        if let Ok(s) = self.table.get(&self_) {
            video::video_surface_set_rect(s.id, rect2b(rect));
        }
    }

    fn presented_rect(
        &mut self,
        self_: Resource<VideoSurfaceState>,
    ) -> Option<wandr_wit::types::VideoRect> {
        let id = self.table.get(&self_).ok()?.id;
        video::video_surface_presented_rect(id).map(rect2w)
    }

    fn set_rotation(&mut self, self_: Resource<VideoSurfaceState>, degrees: u32) {
        if let Ok(s) = self.table.get(&self_) {
            video::video_surface_set_rotation(s.id, degrees);
        }
    }

    fn drop(&mut self, rep: Resource<VideoSurfaceState>) -> wasmtime::Result<()> {
        if let Ok(s) = self.table.get(&rep) {
            video::video_surface_remove(s.id);
        }
        self.table.delete(rep)?;
        Ok(())
    }
}

impl wandr_wit::capture_encode::Host for HostState {}

impl wandr_wit::capture_encode::HostCallEncoder for HostState {
    fn open(
        &mut self,
        config: wandr_wit::types::CallEncoderConfig,
    ) -> Result<Resource<CallEncoderState>, wandr_wit::types::SurfaceError> {
        let cfg = video::EncoderConfig {
            codec: codec2b(config.codec.codec).map_err(|_| {
                wandr_wit::types::SurfaceError::CodecUnavailable
            })?,
            width: config.codec.width,
            height: config.codec.height,
            bitrate_bps: config.codec.bitrate_bps,
            framerate: config.codec.framerate,
            facing_front: matches!(config.facing, camera_wit::types::Facing::Front),
            preview: config.preview.map(rect2b),
            preview_layer: layer2b(config.preview_layer),
        };
        let enc = video::VideoEncoder::open(&cfg).map_err(err2surf)?;
        self.table
            .push(CallEncoderState(enc))
            .map_err(|_| wandr_wit::types::SurfaceError::CodecUnavailable)
    }

    fn next_chunk(
        &mut self,
        self_: Resource<CallEncoderState>,
    ) -> Option<codec_wit::types::EncodedChunk> {
        let st = self.table.get_mut(&self_).ok()?;
        st.0.next_frame().map(|f| codec_wit::types::EncodedChunk {
            data: f.data,
            // The backend encoder timestamps in 90 kHz RTP units; the codec chunk
            // is µs. The guest (RTP layer) converts back as needed.
            timestamp_us: (f.timestamp as i64) * 100 / 9,
            keyframe: f.keyframe,
            decrypt: None,
        })
    }

    fn request_keyframe(&mut self, self_: Resource<CallEncoderState>) {
        if let Ok(st) = self.table.get_mut(&self_) {
            st.0.request_keyframe();
        }
    }

    fn set_bitrate(&mut self, self_: Resource<CallEncoderState>, bitrate_bps: u32) {
        if let Ok(st) = self.table.get_mut(&self_) {
            st.0.set_bitrate(bitrate_bps);
        }
    }

    fn set_preview_rect(&mut self, self_: Resource<CallEncoderState>, rect: wandr_wit::types::VideoRect) {
        if let Ok(st) = self.table.get_mut(&self_) {
            st.0.set_preview_rect(rect2b(rect));
        }
    }

    fn display_rotation(&mut self, self_: Resource<CallEncoderState>) -> u32 {
        self.table.get(&self_).map(|st| st.0.display_rotation()).unwrap_or(0)
    }

    fn drop(&mut self, rep: Resource<CallEncoderState>) -> wasmtime::Result<()> {
        self.table.delete(rep)?; // ordered camera/codec teardown
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// wandr:video-diag — the test/diag surface (list-decoders / implementation /
// decoded-frames), off the standard. Borrows the shared `video-decoder`.
// ═══════════════════════════════════════════════════════════════════════════

impl diag_wit::Host for HostState {
    fn list_decoders(&mut self) -> Vec<diag_wit::DecoderInfo> {
        #[cfg(not(target_os = "android"))]
        {
            wandr_video::describe_backends()
                .into_iter()
                .flat_map(|b| {
                    let (name, hardware) = (b.name.to_string(), b.is_hardware());
                    b.decode.into_iter().filter_map(move |c| {
                        Some(diag_wit::DecoderInfo {
                            codec: codec2w(c)?,
                            name: name.clone(),
                            hardware,
                        })
                    })
                })
                .collect()
        }
        #[cfg(target_os = "android")]
        {
            Vec::new()
        }
    }

    fn implementation(&mut self, dec: Resource<DecoderState>) -> diag_wit::DecoderInfo {
        #[cfg(not(target_os = "android"))]
        {
            let (name, hardware) = self
                .table
                .get(&dec)
                .map(|st| st.0.backend())
                .unwrap_or(("unknown", false));
            diag_wit::DecoderInfo {
                codec: codec_wit::types::Codec::H264,
                name: name.to_string(),
                hardware,
            }
        }
        #[cfg(target_os = "android")]
        {
            let _ = dec;
            diag_wit::DecoderInfo {
                codec: codec_wit::types::Codec::H264,
                name: "mediacodec".to_string(),
                hardware: true,
            }
        }
    }

    fn decoded_frames(&mut self, dec: Resource<DecoderState>) -> u64 {
        self.table.get(&dec).map(|st| st.0.decoded_frames()).unwrap_or(0)
    }
}
