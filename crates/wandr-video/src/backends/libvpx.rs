//! Software VP8/VP9 via libvpx (BSD-3), statically linked from `vendor/libvpx`.
//!
//! This replaces the ffmpeg `libvpx` wrapper the desktop host used to call. Most
//! of the mapping is mechanical, but four things differ from ffmpeg's wrapper and
//! are the reason this file has as many comments as it does:
//!
//!  1. `rc_target_bitrate` is KILOBITS/s; ffmpeg's `set_bit_rate` took bits/s.
//!  2. `vpx_codec_encode` is SYNCHRONOUS — every packet for a frame is available
//!     immediately after it returns. There is no send/receive queue.
//!  3. The packet buffer is owned by the encoder and invalidated by the NEXT
//!     `vpx_codec_encode`, so packets must be copied out (they already were).
//!  4. `rc_end_usage` is set to CBR. ffmpeg defaulted to VBR — this is a
//!     deliberate deviation for call traffic, not a port bug.
//!
//! ‼️ `mem::zeroed()` on `vpx_codec_enc_cfg_t` is undefined behavior (it has a
//! niche field) and aborts under rustc's zero-init check. `MaybeUninit` +
//! `vpx_codec_enc_config_default` is the only correct way to build it.

use std::collections::VecDeque;
use std::mem::MaybeUninit;
use std::ptr;

use vpx_sys as vpx;

use crate::convert::{chroma_dims, rgb24_into_i420, Rgb24Frame};
use crate::{
    Codec, Decoder, DecoderParams, Packet, Encoder, EncoderParams, I420Ref, CodecError,
};

/// libvpx's realtime deadline (`VPX_DL_REALTIME`) — the replacement for ffmpeg's
/// `deadline=realtime` private option.
const DL_REALTIME: std::os::raw::c_ulong = 1;
/// `VPX_EFLAG_FORCE_KF` — per-call and non-sticky, like ffmpeg's `pict_type = I`.
const EFLAG_FORCE_KF: i64 = 1;
/// `VPX_FRAME_IS_KEY`.
const FRAME_IS_KEY: u32 = 0x1;

/// Speed/quality knob. 8 is the WebRTC realtime range for both VP8 and VP9.
const CPU_USED: i32 = 8;

fn enc_iface(codec: Codec) -> *const vpx::vpx_codec_iface_t {
    unsafe {
        match codec {
            Codec::Vp8 => vpx::vpx_codec_vp8_cx(),
            Codec::Vp9 => vpx::vpx_codec_vp9_cx(),
        }
    }
}

fn dec_iface(codec: Codec) -> *const vpx::vpx_codec_iface_t {
    unsafe {
        match codec {
            Codec::Vp8 => vpx::vpx_codec_vp8_dx(),
            Codec::Vp9 => vpx::vpx_codec_vp9_dx(),
        }
    }
}

// ── encoder ──────────────────────────────────────────────────────────────────

pub struct LibvpxEncoder {
    ctx: vpx::vpx_codec_ctx_t,
    /// Kept because `vpx_codec_enc_config_set` needs the WHOLE config, not a delta.
    cfg: vpx::vpx_codec_enc_cfg_t,
    img: *mut vpx::vpx_image_t,
    width: u32,
    height: u32,
    fps: u32,
    pts: i64,
    pending: VecDeque<Packet>,
    /// Resized-RGB scratch, reused across frames so steady state doesn't allocate.
    scratch: Vec<u8>,
    resizer: fast_image_resize::Resizer,
}

// The codec context is a plain struct with no thread affinity or TLS — safe to
// move between threads as long as it is not shared. (The old blanket
// `unsafe impl Send for VideoEncoder` in video_desktop.rs existed only because
// ffmpeg's Context is !Send; this one is genuinely justified.)
unsafe impl Send for LibvpxEncoder {}

impl LibvpxEncoder {
    pub fn open(params: &EncoderParams) -> Result<Self, CodecError> {
        let (w, h, fps) = (params.width, params.height, params.framerate.max(1));
        let iface = enc_iface(params.codec);

        unsafe {
            let mut cfg = MaybeUninit::<vpx::vpx_codec_enc_cfg_t>::uninit();
            if vpx::vpx_codec_enc_config_default(iface, cfg.as_mut_ptr(), 0)
                != vpx::vpx_codec_err_t::VPX_CODEC_OK
            {
                log::warn!("wandr-video: enc_config_default failed");
                return Err(CodecError::InitFailed);
            }
            let mut cfg = cfg.assume_init();

            cfg.g_w = w;
            cfg.g_h = h;
            cfg.g_timebase.num = 1;
            cfg.g_timebase.den = fps as i32;
            // ‼️ KILObits/s — see the module header.
            cfg.rc_target_bitrate = params.bitrate_bps.max(100_000) / 1000;
            cfg.g_lag_in_frames = 0; // was ffmpeg's "lag-in-frames"=0
            cfg.g_pass = vpx::vpx_enc_pass::VPX_RC_ONE_PASS;
            cfg.rc_end_usage = vpx::vpx_rc_mode::VPX_CBR;
            cfg.kf_mode = vpx::vpx_kf_mode::VPX_KF_AUTO;
            cfg.kf_min_dist = 0;
            cfg.kf_max_dist = fps * 4; // was ffmpeg's set_gop(fps*4)
            cfg.rc_min_quantizer = 2;
            cfg.rc_max_quantizer = 56;
            cfg.rc_dropframe_thresh = 0;
            // Buffer model in ms — standard WebRTC-ish CBR tuning.
            cfg.rc_buf_sz = 1000;
            cfg.rc_buf_initial_sz = 500;
            cfg.rc_buf_optimal_sz = 600;
            // Error resilience OFF, matching today's ffmpeg default: Signal uses
            // NACK, and WebRTC only enables it without NACK or with temporal
            // layers. Turning it on costs ~10% quality for nothing here.
            cfg.g_error_resilient = 0;
            cfg.g_threads = std::thread::available_parallelism()
                .map(|n| (n.get() as u32).min(4))
                .unwrap_or(1);

            let mut ctx = MaybeUninit::<vpx::vpx_codec_ctx_t>::uninit();
            let r = vpx::vpx_codec_enc_init_ver(
                ctx.as_mut_ptr(),
                iface,
                &cfg,
                0,
                vpx::VPX_ENCODER_ABI_VERSION as i32,
            );
            if r != vpx::vpx_codec_err_t::VPX_CODEC_OK {
                log::warn!("wandr-video: enc_init_ver -> {r:?}");
                return Err(CodecError::InitFailed);
            }
            let mut ctx = ctx.assume_init();

            vpx::vpx_codec_control_(
                &mut ctx,
                vpx::vp8e_enc_control_id::VP8E_SET_CPUUSED as i32,
                CPU_USED,
            );

            let img = vpx::vpx_img_alloc(
                ptr::null_mut(),
                vpx::vpx_img_fmt::VPX_IMG_FMT_I420,
                w,
                h,
                32,
            );
            if img.is_null() {
                vpx::vpx_codec_destroy(&mut ctx);
                log::warn!("wandr-video: vpx_img_alloc failed for {w}x{h}");
                return Err(CodecError::InitFailed);
            }

            log::info!(
                "wandr-video: libvpx {:?} encoder {w}x{h}@{fps} {} kbps",
                params.codec,
                cfg.rc_target_bitrate
            );

            Ok(Self {
                ctx,
                cfg,
                img,
                width: w,
                height: h,
                fps,
                pts: 0,
                pending: VecDeque::new(),
                scratch: Vec::new(),
                resizer: fast_image_resize::Resizer::new(),
            })
        }
    }

    /// Drain every packet libvpx produced for the frame just encoded.
    unsafe fn drain(&mut self) {
        let mut iter: vpx::vpx_codec_iter_t = ptr::null();
        loop {
            let pkt = vpx::vpx_codec_get_cx_data(&mut self.ctx, &mut iter);
            if pkt.is_null() {
                break;
            }
            if (*pkt).kind != vpx::vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
                continue;
            }
            let f = (*pkt).data.frame;
            if f.buf.is_null() || f.sz == 0 {
                continue;
            }
            // The buffer is invalidated by the next vpx_codec_encode — copy out.
            let data = std::slice::from_raw_parts(f.buf as *const u8, f.sz).to_vec();
            self.pending.push_back(Packet {
                data,
                timestamp: crate::rtp_ts(f.pts, self.fps),
                keyframe: (f.flags & FRAME_IS_KEY) != 0,
            });
        }
    }
}

impl Encoder for LibvpxEncoder {
    fn encode(&mut self, frame: Rgb24Frame<'_>, force_keyframe: bool) -> Result<(), CodecError> {
        let (w, h) = (self.width, self.height);
        let (cw, ch) = chroma_dims(w, h);

        unsafe {
            let img = &*self.img;
            let (ys, us, vs) = (img.stride[0] as u32, img.stride[1] as u32, img.stride[2] as u32);
            // Write the converted frame straight into libvpx's own planes.
            let y = std::slice::from_raw_parts_mut(img.planes[0], ys as usize * h as usize);
            let u = std::slice::from_raw_parts_mut(img.planes[1], us as usize * ch as usize);
            let v = std::slice::from_raw_parts_mut(img.planes[2], vs as usize * ch as usize);
            let _ = cw;

            rgb24_into_i420(
                frame,
                w,
                h,
                y,
                ys,
                u,
                us,
                v,
                vs,
                &mut self.scratch,
                &mut self.resizer,
            )?;

            let flags: i64 = if force_keyframe { EFLAG_FORCE_KF } else { 0 };
            let r = vpx::vpx_codec_encode(&mut self.ctx, self.img, self.pts, 1, flags, DL_REALTIME);
            if r != vpx::vpx_codec_err_t::VPX_CODEC_OK {
                log::warn!("wandr-video: vpx_codec_encode -> {r:?}");
                return Err(CodecError::BadFrame);
            }
            self.drain();
        }
        self.pts += 1;
        Ok(())
    }

    fn next_packet(&mut self) -> Option<Packet> {
        self.pending.pop_front()
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError> {
        // Unlike the ffmpeg path (where this was a no-op), libvpx retunes rate
        // control mid-stream cheaply and without forcing a keyframe — so the
        // desktop encoder now actually honors REMB/TWCC congestion control.
        self.cfg.rc_target_bitrate = bps.max(100_000) / 1000;
        unsafe {
            let r = vpx::vpx_codec_enc_config_set(&mut self.ctx, &self.cfg);
            if r != vpx::vpx_codec_err_t::VPX_CODEC_OK {
                log::warn!("wandr-video: enc_config_set -> {r:?}");
                return Err(CodecError::InitFailed);
            }
        }
        Ok(())
    }
}

impl Drop for LibvpxEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.img.is_null() {
                vpx::vpx_img_free(self.img);
            }
            vpx::vpx_codec_destroy(&mut self.ctx);
        }
    }
}

// ── decoder ──────────────────────────────────────────────────────────────────

pub struct LibvpxDecoder {
    ctx: vpx::vpx_codec_ctx_t,
    /// Iterator state for `vpx_codec_get_frame`; reset on every `decode`.
    iter: vpx::vpx_codec_iter_t,
}

unsafe impl Send for LibvpxDecoder {}

impl LibvpxDecoder {
    pub fn open(params: &DecoderParams) -> Result<Self, CodecError> {
        unsafe {
            let mut ctx = MaybeUninit::<vpx::vpx_codec_ctx_t>::uninit();
            let dcfg = vpx::vpx_codec_dec_cfg_t {
                threads: 2,
                // 0 = derive from the stream. VP8/VP9 keyframes carry their own
                // dimensions, which is why the old lazy-scaler-rebuild machinery
                // is gone: we read d_w/d_h per frame instead.
                w: 0,
                h: 0,
            };
            let r = vpx::vpx_codec_dec_init_ver(
                ctx.as_mut_ptr(),
                dec_iface(params.codec),
                &dcfg,
                0,
                vpx::VPX_DECODER_ABI_VERSION as i32,
            );
            if r != vpx::vpx_codec_err_t::VPX_CODEC_OK {
                log::warn!("wandr-video: dec_init_ver -> {r:?}");
                return Err(CodecError::InitFailed);
            }
            Ok(Self { ctx: ctx.assume_init(), iter: ptr::null() })
        }
    }
}

impl Decoder for LibvpxDecoder {
    fn decode(&mut self, data: &[u8]) -> Result<(), CodecError> {
        unsafe {
            let r = vpx::vpx_codec_decode(
                &mut self.ctx,
                data.as_ptr(),
                data.len() as u32,
                ptr::null_mut(),
                0,
            );
            if r != vpx::vpx_codec_err_t::VPX_CODEC_OK {
                log::warn!("wandr-video: vpx_codec_decode -> {r:?}");
                return Err(CodecError::BadFrame);
            }
        }
        self.iter = ptr::null();
        Ok(())
    }

    fn next_frame(&mut self) -> Option<I420Ref<'_>> {
        unsafe {
            let img = vpx::vpx_codec_get_frame(&mut self.ctx, &mut self.iter);
            if img.is_null() {
                return None;
            }
            let img = &*img;
            // Only I420 is handled. VP9 can emit I422/I444/high-bitdepth; the
            // vendored libvpx is built --disable-vp9-highbitdepth, and
            // misinterpreting planes would be worse than dropping the frame.
            if img.fmt != vpx::vpx_img_fmt::VPX_IMG_FMT_I420 {
                log::warn!("wandr-video: dropping non-I420 frame ({:?})", img.fmt);
                return None;
            }
            let (w, h) = (img.d_w, img.d_h);
            let (_, ch) = chroma_dims(w, h);
            let (ys, us, vs) = (img.stride[0] as u32, img.stride[1] as u32, img.stride[2] as u32);
            Some(I420Ref {
                y: std::slice::from_raw_parts(img.planes[0], ys as usize * h as usize),
                y_stride: ys,
                u: std::slice::from_raw_parts(img.planes[1], us as usize * ch as usize),
                u_stride: us,
                v: std::slice::from_raw_parts(img.planes[2], vs as usize * ch as usize),
                v_stride: vs,
                width: w,
                height: h,
            })
        }
    }
}

impl Drop for LibvpxDecoder {
    fn drop(&mut self) {
        unsafe {
            vpx::vpx_codec_destroy(&mut self.ctx);
        }
    }
}
