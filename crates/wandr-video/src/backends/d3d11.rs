//! Hardware H.264 decode on Windows via DXVA2 / `ID3D11VideoDecoder`.
//!
//! The Windows peer of the VA-API backend. It reuses cros-codecs' pure-Rust
//! H.264 parser (the `codec` module builds on Windows; cros-codecs' *driver* is
//! Linux-gated, so the POC + sliding-window DPB + display-order reordering are a
//! thin driver here), then decodes each picture through the fixed-function
//! decoder and reads the NV12 result back to I420. DXVA2 runs on the SAME D3D11
//! device our ANGLE renderer uses, so a future zero-copy path (Phase 2) needs no
//! cross-API bridge — but this first cut is CPU-readback, mirroring VA-API's
//! readback lane.
//!
//! Verified bit-exact on real hardware (Intel UHD 620) against the ffmpeg
//! framehash reference for the cros-codecs H.264 test vectors, including
//! multi-slice pictures, P+B references, POC types 0/2, and multi-GOP streams
//! (repros/d3d11-video-decode-spike). ‼️ The one non-obvious trap: dxva.h is
//! `#pragma pack(1)`, so `DXVA_Slice_H264_Short` is 10 bytes — a `repr(C)` (12,
//! u32-aligned) version misaligns a multi-entry slice array and the driver
//! silently decodes only the first slice.
//!
//! LIMITATIONS (v1): reference management is sliding-window + short-term only —
//! MMCO / long-term references are not yet handled (they need cros-codecs'
//! `codec::h264::dpb` marking, the conformance follow-up). POC type 1 is
//! unsupported (vanishingly rare). Both cases affect only unusual encoders; the
//! common playback stream (what a player feeds) is covered.
#![allow(non_snake_case, non_camel_case_types)]

use std::collections::VecDeque;
use std::io::Cursor;
use std::mem::{size_of, zeroed};

use cros_codecs::codec::h264::parser::{Nalu, NaluType, Parser, Pps, SliceHeader, SliceType, Sps};

use windows::core::{Interface, GUID};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

use crate::{
    BackendKind, Chunk, Codec, CodecBackend, CodecError, ColorInfo, D3d11View, Decoder,
    DecoderParams, Encoder, EncoderParams, Frame, GpuFrame, GpuFrameOwner, I420Ref,
};

// D3D11_DECODER_PROFILE_H264_VLD_NOFGT = {1b81be68-a0c7-11d3-b984-00c04f2e73c5}
const H264_VLD_NOFGT: GUID = GUID::from_u128(0x1b81be68_a0c7_11d3_b984_00c04f2e73c5);
const H264_VLD_FGT: GUID = GUID::from_u128(0x1b81be69_a0c7_11d3_b984_00c04f2e73c5);
const INVALID_ENTRY: u8 = 0xFF;
const POOL: u32 = 8; // decode surface pool (refs + in-flight)
const DRM_FORMAT_NV12: u32 = 0x3231_564e; // 'N''V''1''2', for parity with the vaapi frame

// ── hand-defined DXVA structs (dxva.h is #pragma pack(1)) ─────────────────────

type DXVA_PicEntry = u8; // { Index7Bits:7, AssociatedFlag:1 }; 0xFF = invalid

// No internal padding at natural alignment, so repr(C) already matches dxva.h.
#[repr(C)]
#[derive(Clone, Copy)]
struct DXVA_PicParams_H264 {
    wFrameWidthInMbsMinus1: u16,
    wFrameHeightInMbsMinus1: u16,
    CurrPic: DXVA_PicEntry,
    num_ref_frames: u8,
    wBitFields: u16,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    Reserved16Bits: u16,
    StatusReportFeedbackNumber: u32,
    RefFrameList: [DXVA_PicEntry; 16],
    CurrFieldOrderCnt: [i32; 2],
    FieldOrderCntList: [[i32; 2]; 16],
    pic_init_qs_minus26: i8,
    chroma_qp_index_offset: i8,
    second_chroma_qp_index_offset: i8,
    ContinuationFlag: u8,
    pic_init_qp_minus26: i8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    Reserved8BitsA: u8,
    FrameNumList: [u16; 16],
    UsedForReferenceFlags: u32,
    NonExistingFrameFlags: u16,
    frame_num: u16,
    log2_max_frame_num_minus4: u8,
    pic_order_cnt_type: u8,
    log2_max_pic_order_cnt_lsb_minus4: u8,
    delta_pic_order_always_zero_flag: u8,
    direct_8x8_inference_flag: u8,
    entropy_coding_mode_flag: u8,
    pic_order_present_flag: u8,
    num_slice_groups_minus1: u8,
    slice_group_map_type: u8,
    deblocking_filter_control_present_flag: u8,
    redundant_pic_cnt_present_flag: u8,
    Reserved8BitsB: u8,
    slice_group_change_rate_minus1: u16,
    SliceGroupMap: [u8; 810],
}

// ‼️ 10 bytes, NOT 12: packed so a multi-entry array has the right stride.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct DXVA_Slice_H264_Short {
    BSNALunitDataLocation: u32,
    SliceBytesInBuffer: u32,
    wBadSliceChopping: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DXVA_Qmatrix_H264 {
    bScalingLists4x4: [[u8; 16]; 6],
    bScalingLists8x8: [[u8; 64]; 2],
}

// ── owned SPS/PPS fields (cros-codecs Sps/Pps are not Clone) ──────────────────

#[derive(Clone)]
struct SpsBits {
    pic_width_in_mbs_minus1: u16,
    pic_height_in_map_units_minus1: u16,
    frame_mbs_only_flag: bool,
    chroma_format_idc: u8,
    direct_8x8_inference_flag: bool,
    max_num_ref_frames: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    log2_max_frame_num_minus4: u8,
    pic_order_cnt_type: u8,
    log2_max_pic_order_cnt_lsb_minus4: u8,
    delta_pic_order_always_zero_flag: bool,
    reorder_window: usize,
}
impl SpsBits {
    fn from(s: &Sps) -> Self {
        Self {
            pic_width_in_mbs_minus1: s.pic_width_in_mbs_minus1,
            pic_height_in_map_units_minus1: s.pic_height_in_map_units_minus1,
            frame_mbs_only_flag: s.frame_mbs_only_flag,
            chroma_format_idc: s.chroma_format_idc,
            direct_8x8_inference_flag: s.direct_8x8_inference_flag,
            max_num_ref_frames: s.max_num_ref_frames,
            bit_depth_luma_minus8: s.bit_depth_luma_minus8,
            bit_depth_chroma_minus8: s.bit_depth_chroma_minus8,
            log2_max_frame_num_minus4: s.log2_max_frame_num_minus4,
            pic_order_cnt_type: s.pic_order_cnt_type,
            log2_max_pic_order_cnt_lsb_minus4: s.log2_max_pic_order_cnt_lsb_minus4,
            delta_pic_order_always_zero_flag: s.delta_pic_order_always_zero_flag,
            reorder_window: s.max_num_order_frames() as usize,
        }
    }
    fn width(&self) -> u32 {
        (self.pic_width_in_mbs_minus1 as u32 + 1) * 16
    }
    fn height(&self) -> u32 {
        (self.pic_height_in_map_units_minus1 as u32 + 1) * 16
    }
}

#[derive(Clone)]
struct PpsBits {
    entropy_coding_mode_flag: bool,
    bottom_field_pic_order_in_frame_present_flag: bool,
    weighted_pred_flag: bool,
    weighted_bipred_idc: u8,
    transform_8x8_mode_flag: bool,
    constrained_intra_pred_flag: bool,
    deblocking_filter_control_present_flag: bool,
    redundant_pic_cnt_present_flag: bool,
    num_slice_groups_minus1: u32,
    num_ref_idx_l0_default_active_minus1: u8,
    num_ref_idx_l1_default_active_minus1: u8,
    pic_init_qp_minus26: i8,
    pic_init_qs_minus26: i8,
    chroma_qp_index_offset: i8,
    second_chroma_qp_index_offset: i8,
}
impl PpsBits {
    fn from(p: &Pps) -> Self {
        Self {
            entropy_coding_mode_flag: p.entropy_coding_mode_flag,
            bottom_field_pic_order_in_frame_present_flag: p.bottom_field_pic_order_in_frame_present_flag,
            weighted_pred_flag: p.weighted_pred_flag,
            weighted_bipred_idc: p.weighted_bipred_idc,
            transform_8x8_mode_flag: p.transform_8x8_mode_flag,
            constrained_intra_pred_flag: p.constrained_intra_pred_flag,
            deblocking_filter_control_present_flag: p.deblocking_filter_control_present_flag,
            redundant_pic_cnt_present_flag: p.redundant_pic_cnt_present_flag,
            num_slice_groups_minus1: p.num_slice_groups_minus1,
            num_ref_idx_l0_default_active_minus1: p.num_ref_idx_l0_default_active_minus1,
            num_ref_idx_l1_default_active_minus1: p.num_ref_idx_l1_default_active_minus1,
            pic_init_qp_minus26: p.pic_init_qp_minus26,
            pic_init_qs_minus26: p.pic_init_qs_minus26,
            chroma_qp_index_offset: p.chroma_qp_index_offset,
            second_chroma_qp_index_offset: p.second_chroma_qp_index_offset,
        }
    }
}

// ── the backend (factory) ────────────────────────────────────────────────────

pub struct D3d11Backend;

impl CodecBackend for D3d11Backend {
    fn name(&self) -> &'static str {
        "d3d11"
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Hardware
    }
    fn priority(&self) -> u32 {
        10 // hardware first
    }
    fn supports_decode(&self, codec: Codec) -> bool {
        // Probe the actual driver: a build with the feature but no usable video
        // device reports H.264 = unsupported and software stays the fallback.
        codec == Codec::H264 && probe_h264().unwrap_or(false)
    }
    fn supports_encode(&self, _codec: Codec) -> bool {
        false
    }
    fn open_decoder(&self, params: &DecoderParams) -> Result<Box<dyn Decoder>, CodecError> {
        if params.codec != Codec::H264 {
            return Err(CodecError::Unsupported);
        }
        // Fallback contract: fail here if HW can't decode, so the registry falls
        // back to software rather than us silently producing nothing.
        if !probe_h264().unwrap_or(false) {
            return Err(CodecError::InitFailed);
        }
        Ok(Box::new(D3d11Decoder::new()))
    }
    fn open_encoder(&self, _params: &EncoderParams) -> Result<Box<dyn Encoder>, CodecError> {
        Err(CodecError::Unsupported)
    }
}

/// True if the default hardware adapter exposes an H.264 VLD decode profile.
fn probe_h264() -> anyhow::Result<bool> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
        let vdevice: ID3D11VideoDevice = device.ok_or_else(|| anyhow::anyhow!("no device"))?.cast()?;
        let n = vdevice.GetVideoDecoderProfileCount();
        for i in 0..n {
            let g = vdevice.GetVideoDecoderProfile(i)?;
            if g == H264_VLD_NOFGT || g == H264_VLD_FGT {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

// ── the decoder (parser + reorder + DXVA core) ───────────────────────────────

/// One decoded picture in display-buffering order — either read back to CPU
/// I420, or kept as its own D3D11 NV12 texture for the host to import into ANGLE.
struct Decoded {
    poc: i32,
    pts: i64,
    width: u32,
    height: u32,
    payload: Payload,
}

enum Payload {
    Cpu(Vec<u8>), // tightly-packed I420: Y (w*h) ++ U (cw*ch) ++ V (cw*ch)
    Gpu(GpuTex),
}

/// A per-frame NV12 texture (a copy OUT of the decode pool, so the pool slice is
/// free to be reused immediately and the frame's lifetime is decoupled from the
/// DPB). Copying stays on the GPU — no CPU roundtrip — which is the point.
struct GpuTex {
    texture: ID3D11Texture2D,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}
// COM D3D11 resources are free-threaded (agile); the windows crate marks them
// Send+Sync, so GpuTex is Send.

/// Owns a per-frame NV12 texture for a `GpuFrame`: hands the host a D3D11 view for
/// zero-copy import, or reads it back to I420 on demand (the fallback lane).
struct D3d11Owner {
    tex: GpuTex,
    width: u32,
    height: u32,
}

impl GpuFrameOwner for D3d11Owner {
    fn read_i420(&self, out: &mut Vec<u8>) -> Result<(), CodecError> {
        unsafe {
            let i420 = readback_nv12_texture(&self.tex.device, &self.tex.context, &self.tex.texture, self.width, self.height)
                .map_err(|_| CodecError::BadFrame)?;
            out.clear();
            out.extend_from_slice(&i420);
            Ok(())
        }
    }
    fn d3d11(&self) -> Option<D3d11View> {
        Some(D3d11View {
            texture: self.tex.texture.clone(),
            device: self.tex.device.clone(),
            array_slice: 0,
        })
    }
}


pub struct D3d11Decoder {
    // Raw SPS/PPS NALs kept so each decode() can build a FRESH cros-codecs Parser
    // primed with them — the Parser holds Rc<Sps>/Rc<Pps> and so is not Send, but
    // the trait requires Send, so it must never be stored across calls.
    sps_nal: Option<Vec<u8>>,
    pps_nal: Option<Vec<u8>>,
    sps: Option<SpsBits>,
    pps: Option<PpsBits>,
    core: Option<Dxva>,
    reorder_window: usize,
    reorder: Vec<Decoded>,   // decoded, awaiting display-order emit
    ready: VecDeque<Decoded>, // emit order (display)
    cur: Option<Vec<u8>>,     // CPU I420 currently borrowed by next_frame
    /// Emit `Frame::gpu` (D3D11 texture) instead of `Frame::cpu` (readback I420).
    /// Opt-in via `WANDR_VIDEO_D3D11_GPU=1` until the host's ANGLE import (Phase
    /// 2b) consumes it; the CPU path stays the verified default.
    gpu: bool,
}

impl D3d11Decoder {
    fn new() -> Self {
        Self {
            sps_nal: None,
            pps_nal: None,
            sps: None,
            pps: None,
            core: None,
            reorder_window: 4,
            reorder: Vec::new(),
            ready: VecDeque::new(),
            cur: None,
            gpu: std::env::var("WANDR_VIDEO_D3D11_GPU").is_ok(),
        }
    }

    /// Emit the lowest-POC buffered frame into `ready` (Annex-C bumping).
    fn bump_one(&mut self) {
        if let Some((i, _)) = self.reorder.iter().enumerate().min_by_key(|(_, d)| d.poc) {
            let d = self.reorder.remove(i);
            self.ready.push_back(d);
        }
    }

    /// Emit everything buffered, lowest POC first (GOP boundary / end of stream).
    fn drain(&mut self) {
        self.reorder.sort_by_key(|d| d.poc);
        for d in self.reorder.drain(..) {
            self.ready.push_back(d);
        }
    }
}

impl Decoder for D3d11Decoder {
    fn decode(&mut self, chunk: Chunk<'_>) -> Result<(), CodecError> {
        // A fresh Parser per call (it holds Rc and is not Send). Prime it with the
        // last-seen SPS/PPS so a slice-only access unit still parses.
        let mut parser = Parser::default();
        if let Some(b) = &self.sps_nal {
            let mut c = Cursor::new(b.as_slice());
            if let Ok(n) = Nalu::next(&mut c) {
                let _ = parser.parse_sps(&n);
            }
        }
        if let Some(b) = &self.pps_nal {
            let mut c = Cursor::new(b.as_slice());
            if let Ok(n) = Nalu::next(&mut c) {
                let _ = parser.parse_pps(&n);
            }
        }

        // One chunk = one access unit (the guest demuxes). Collect its slices.
        let mut cursor = Cursor::new(chunk.data);
        let mut slice_nals: Vec<Vec<u8>> = Vec::new();
        let mut hdr0: Option<SliceHeader> = None;
        let mut is_idr = false;
        let mut ref_idc = 0u8;

        while let Ok(nalu) = Nalu::next(&mut cursor) {
            match nalu.header.type_ {
                NaluType::Sps => {
                    self.sps_nal = Some(nalu.data.to_vec());
                    let s = parser.parse_sps(&nalu).map_err(|_| CodecError::BadFrame)?;
                    self.sps = Some(SpsBits::from(s));
                }
                NaluType::Pps => {
                    self.pps_nal = Some(nalu.data.to_vec());
                    let p = parser.parse_pps(&nalu).map_err(|_| CodecError::BadFrame)?;
                    self.pps = Some(PpsBits::from(p));
                }
                NaluType::SliceIdr | NaluType::Slice => {
                    let bytes = nalu.data.to_vec();
                    if hdr0.is_none() {
                        is_idr = nalu.header.idr_pic_flag;
                        ref_idc = nalu.header.ref_idc;
                        let h = parser.parse_slice_header(nalu).map_err(|_| CodecError::BadFrame)?;
                        hdr0 = Some(h.header);
                    }
                    slice_nals.push(bytes);
                }
                _ => {}
            }
        }

        if slice_nals.is_empty() {
            return Ok(()); // config-only AU (SPS/PPS), nothing to decode
        }
        let sps = self.sps.clone().ok_or(CodecError::BadFrame)?;
        let pps = self.pps.clone().ok_or(CodecError::BadFrame)?;
        if sps.pic_order_cnt_type == 1 {
            return Err(CodecError::Unsupported); // POC type 1 not implemented
        }
        let hdr = hdr0.ok_or(CodecError::BadFrame)?;

        if self.core.is_none() {
            self.core = Some(Dxva::new(sps.width(), sps.height()).map_err(|_| CodecError::InitFailed)?);
            self.reorder_window = sps.reorder_window.max(1);
        }
        let gpu = self.gpu;
        let core = self.core.as_mut().unwrap();
        let (payload, poc) = core
            .decode_picture(&sps, &pps, &hdr, is_idr, ref_idc, &slice_nals, gpu)
            .map_err(|_| CodecError::BadFrame)?;

        // Display-order reorder: an IDR ends the previous GOP (POC resets to 0).
        if is_idr {
            self.drain();
        }
        self.reorder.push(Decoded {
            poc,
            pts: chunk.timestamp_us,
            width: sps.width(),
            height: sps.height(),
            payload,
        });
        while self.reorder.len() > self.reorder_window {
            self.bump_one();
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), CodecError> {
        self.drain();
        Ok(())
    }

    fn reset(&mut self) -> Result<(), CodecError> {
        // Seek: drop pending output; the DPB self-clears on the next IDR (caller
        // must feed a keyframe next, per the trait contract).
        self.reorder.clear();
        self.ready.clear();
        self.cur = None;
        Ok(())
    }

    fn frames_in_flight_limit(&self) -> Option<usize> {
        // Output is a CPU copy (readback), not a pool surface — a held frame does
        // not starve the decoder, exactly like the software backends.
        None
    }

    fn next_frame(&mut self) -> Option<Frame<'_>> {
        let d = self.ready.pop_front()?;
        let (w, h, pts) = (d.width, d.height, d.pts);
        match d.payload {
            // GPU: hand out an owned frame carrying the D3D11 texture. No borrow
            // of `self` is needed — the frame owns its texture.
            Payload::Gpu(tex) => {
                let owner = D3d11Owner { tex, width: w, height: h };
                let gf = GpuFrame::new(
                    w,
                    h,
                    pts,
                    DRM_FORMAT_NV12,
                    0,
                    Vec::new(),
                    ColorInfo::for_resolution(w, h),
                    Box::new(owner),
                );
                Some(Frame::gpu(gf))
            }
            // CPU: park the I420 in `self.cur` so the returned borrow stays valid
            // until the next call.
            Payload::Cpu(data) => {
                self.cur = Some(data);
                let buf = self.cur.as_ref().unwrap();
                let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
                let yl = (w * h) as usize;
                let cl = (cw * ch) as usize;
                Some(Frame::cpu(I420Ref {
                    y: &buf[..yl],
                    y_stride: w,
                    u: &buf[yl..yl + cl],
                    u_stride: cw,
                    v: &buf[yl + cl..yl + 2 * cl],
                    v_stride: cw,
                    width: w,
                    height: h,
                    timestamp_us: pts,
                }))
            }
        }
    }
}

// ── the DXVA core: device, decoder, surface pool, per-picture decode ─────────

#[derive(Clone, Copy)]
struct DpbRef {
    slice: u8,
    frame_num: u16,
    top_poc: i32,
    bottom_poc: i32,
}

struct Dxva {
    // Held for Phase-2 zero-copy (share this D3D11 device with ANGLE, or export a
    // shared texture from it); unused on the CPU-readback path.
    #[allow(dead_code)]
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vcontext: ID3D11VideoContext,
    decoder: ID3D11VideoDecoder,
    views: Vec<ID3D11VideoDecoderOutputView>,
    pool: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    free: Vec<u8>,
    dpb: Vec<DpbRef>,
    prev_poc_msb: i32,
    prev_poc_lsb: i32,
    prev_frame_num: i32,
    prev_frame_num_offset: i32,
    width: u32,
    height: u32,
    feedback: u32,
}

impl Dxva {
    fn new(width: u32, height: u32) -> anyhow::Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;
            let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device: ID3D11Device = device.ok_or_else(|| anyhow::anyhow!("no device"))?;
            let context: ID3D11DeviceContext = context.ok_or_else(|| anyhow::anyhow!("no context"))?;
            let vdevice: ID3D11VideoDevice = device.cast()?;
            let vcontext: ID3D11VideoContext = context.cast()?;

            let desc = D3D11_VIDEO_DECODER_DESC {
                Guid: H264_VLD_NOFGT,
                SampleWidth: width,
                SampleHeight: height,
                OutputFormat: DXGI_FORMAT_NV12,
            };
            let cfg_count = vdevice.GetVideoDecoderConfigCount(&desc)?;
            let mut config: D3D11_VIDEO_DECODER_CONFIG = zeroed();
            let mut have = false;
            for i in 0..cfg_count {
                let mut c: D3D11_VIDEO_DECODER_CONFIG = zeroed();
                vdevice.GetVideoDecoderConfig(&desc, i, &mut c)?;
                if c.ConfigBitstreamRaw == 2 {
                    config = c;
                    have = true;
                    break;
                }
                if !have {
                    config = c;
                    have = true;
                }
            }
            anyhow::ensure!(have, "no decoder config");
            let decoder = vdevice.CreateVideoDecoder(&desc, &config)?;

            let tdesc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: POOL,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_DECODER.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut pool = None;
            device.CreateTexture2D(&tdesc, None, Some(&mut pool))?;
            let pool: ID3D11Texture2D = pool.ok_or_else(|| anyhow::anyhow!("no pool"))?;
            let mut views = Vec::new();
            for s in 0..POOL {
                let ovdesc = D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC {
                    DecodeProfile: H264_VLD_NOFGT,
                    ViewDimension: D3D11_VDOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VDOV { ArraySlice: s },
                    },
                };
                let mut v = None;
                vdevice.CreateVideoDecoderOutputView(&pool, &ovdesc, Some(&mut v))?;
                views.push(v.ok_or_else(|| anyhow::anyhow!("no view"))?);
            }
            let sdesc = D3D11_TEXTURE2D_DESC {
                ArraySize: 1,
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..tdesc
            };
            let mut staging = None;
            device.CreateTexture2D(&sdesc, None, Some(&mut staging))?;

            Ok(Self {
                device,
                context,
                vcontext,
                decoder,
                views,
                pool,
                staging: staging.ok_or_else(|| anyhow::anyhow!("no staging"))?,
                free: (0..POOL as u8).rev().collect(),
                dpb: Vec::new(),
                prev_poc_msb: 0,
                prev_poc_lsb: 0,
                prev_frame_num: 0,
                prev_frame_num_offset: 0,
                width,
                height,
                feedback: 0,
            })
        }
    }

    fn compute_poc(&mut self, sps: &SpsBits, hdr: &SliceHeader, is_idr: bool, is_ref: bool) -> i32 {
        let fnum = hdr.frame_num as i32;
        match sps.pic_order_cnt_type {
            0 => {
                let (prev_msb, prev_lsb) =
                    if is_idr { (0, 0) } else { (self.prev_poc_msb, self.prev_poc_lsb) };
                let max_lsb = 1i32 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
                let lsb = hdr.pic_order_cnt_lsb as i32;
                let msb = if lsb < prev_lsb && (prev_lsb - lsb) >= max_lsb / 2 {
                    prev_msb + max_lsb
                } else if lsb > prev_lsb && (lsb - prev_lsb) > max_lsb / 2 {
                    prev_msb - max_lsb
                } else {
                    prev_msb
                };
                if is_ref {
                    self.prev_poc_msb = msb;
                    self.prev_poc_lsb = lsb;
                }
                msb + lsb
            }
            _ => {
                // type 2 (8.2.1.3): POC derived from frame_num, frames only.
                let max_fn = 1i32 << (sps.log2_max_frame_num_minus4 + 4);
                let offset = if is_idr {
                    0
                } else if self.prev_frame_num > fnum {
                    self.prev_frame_num_offset + max_fn
                } else {
                    self.prev_frame_num_offset
                };
                let temp = if is_idr {
                    0
                } else if !is_ref {
                    2 * (offset + fnum) - 1
                } else {
                    2 * (offset + fnum)
                };
                self.prev_frame_num = fnum;
                self.prev_frame_num_offset = offset;
                temp
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_pic_params(
        &self,
        sps: &SpsBits,
        pps: &PpsBits,
        hdr: &SliceHeader,
        is_ref: bool,
        out_slice: u8,
        top_poc: i32,
        feedback: u32,
    ) -> DXVA_PicParams_H264 {
        let mut pp: DXVA_PicParams_H264 = unsafe { zeroed() };
        pp.wFrameWidthInMbsMinus1 = sps.pic_width_in_mbs_minus1;
        let interlaced = !sps.frame_mbs_only_flag as u16;
        pp.wFrameHeightInMbsMinus1 = ((sps.pic_height_in_map_units_minus1 + 1) << interlaced) - 1;
        pp.CurrPic = out_slice & 0x7F;
        pp.num_ref_frames = sps.max_num_ref_frames;

        let is_intra = matches!(hdr.slice_type, SliceType::I);
        let mut bf: u16 = 0;
        bf |= (hdr.field_pic_flag as u16) << 0;
        bf |= ((sps.chroma_format_idc as u16) & 0x3) << 4;
        bf |= (is_ref as u16) << 6;
        bf |= (pps.constrained_intra_pred_flag as u16) << 7;
        bf |= (pps.weighted_pred_flag as u16) << 8;
        bf |= ((pps.weighted_bipred_idc as u16) & 0x3) << 9;
        bf |= 1u16 << 11; // MbsConsecutiveFlag
        bf |= (sps.frame_mbs_only_flag as u16) << 12;
        bf |= (pps.transform_8x8_mode_flag as u16) << 13;
        bf |= (sps.direct_8x8_inference_flag as u16) << 14;
        bf |= (is_intra as u16) << 15;
        pp.wBitFields = bf;

        pp.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8;
        pp.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8;
        pp.Reserved16Bits = 3;
        pp.StatusReportFeedbackNumber = feedback;

        pp.RefFrameList = [INVALID_ENTRY; 16];
        pp.CurrFieldOrderCnt = [top_poc, top_poc];
        let mut used: u32 = 0;
        for (k, r) in self.dpb.iter().enumerate().take(16) {
            pp.RefFrameList[k] = r.slice & 0x7F;
            pp.FieldOrderCntList[k] = [r.top_poc, r.bottom_poc];
            pp.FrameNumList[k] = r.frame_num;
            used |= 3u32 << (2 * k);
        }
        pp.UsedForReferenceFlags = used;

        pp.pic_init_qs_minus26 = pps.pic_init_qs_minus26;
        pp.chroma_qp_index_offset = pps.chroma_qp_index_offset;
        pp.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset;
        pp.ContinuationFlag = 1;
        pp.pic_init_qp_minus26 = pps.pic_init_qp_minus26;
        pp.num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
        pp.num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
        pp.frame_num = hdr.frame_num;
        pp.log2_max_frame_num_minus4 = sps.log2_max_frame_num_minus4;
        pp.pic_order_cnt_type = sps.pic_order_cnt_type;
        pp.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_pic_order_cnt_lsb_minus4;
        pp.delta_pic_order_always_zero_flag = sps.delta_pic_order_always_zero_flag as u8;
        pp.direct_8x8_inference_flag = sps.direct_8x8_inference_flag as u8;
        pp.entropy_coding_mode_flag = pps.entropy_coding_mode_flag as u8;
        pp.pic_order_present_flag = pps.bottom_field_pic_order_in_frame_present_flag as u8;
        pp.num_slice_groups_minus1 = pps.num_slice_groups_minus1 as u8;
        pp.deblocking_filter_control_present_flag = pps.deblocking_filter_control_present_flag as u8;
        pp.redundant_pic_cnt_present_flag = pps.redundant_pic_cnt_present_flag as u8;
        pp
    }

    fn decode_picture(
        &mut self,
        sps: &SpsBits,
        pps: &PpsBits,
        hdr: &SliceHeader,
        is_idr: bool,
        ref_idc: u8,
        slice_nals: &[Vec<u8>],
        gpu: bool,
    ) -> anyhow::Result<(Payload, i32)> {
        unsafe {
            if is_idr {
                self.dpb.clear();
                self.free = (0..POOL as u8).rev().collect();
                self.prev_poc_msb = 0;
                self.prev_poc_lsb = 0;
                self.prev_frame_num = 0;
                self.prev_frame_num_offset = 0;
            }
            let is_ref = ref_idc != 0;
            let out_slice = self.free.pop().ok_or_else(|| anyhow::anyhow!("pool exhausted"))?;
            let top_poc = self.compute_poc(sps, hdr, is_idr, is_ref);
            self.feedback = self.feedback.wrapping_add(1).max(1);

            let pic = self.build_pic_params(sps, pps, hdr, is_ref, out_slice, top_poc, self.feedback);
            let qm = DXVA_Qmatrix_H264 {
                bScalingLists4x4: [[16u8; 16]; 6],
                bScalingLists8x8: [[16u8; 64]; 2],
            };

            // All slices of the picture -> one bitstream buffer + a packed
            // (10-byte-stride) SliceControl array; buffer order PP, IQ, BS, SC.
            let n_mbs = (self.width / 16) * (self.height / 16);
            let mut bitstream: Vec<u8> = Vec::new();
            let mut slice_ctl: Vec<DXVA_Slice_H264_Short> = Vec::new();
            for nal in slice_nals {
                slice_ctl.push(DXVA_Slice_H264_Short {
                    BSNALunitDataLocation: bitstream.len() as u32,
                    SliceBytesInBuffer: nal.len() as u32,
                    wBadSliceChopping: 0,
                });
                bitstream.extend_from_slice(nal);
            }
            let padding = (128 - (bitstream.len() & 127)) & 127;
            if let Some(last) = slice_ctl.last_mut() {
                let mut e = *last;
                e.SliceBytesInBuffer += padding as u32;
                *last = e;
            }
            let sc_bytes = std::slice::from_raw_parts(
                slice_ctl.as_ptr() as *const u8,
                slice_ctl.len() * size_of::<DXVA_Slice_H264_Short>(),
            );

            self.vcontext.DecoderBeginFrame(&self.decoder, &self.views[out_slice as usize], 0, None)?;
            self.put(D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, as_bytes(&pic))?;
            self.put(D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX, as_bytes(&qm))?;
            let bs_padded = self.put_bitstream(&bitstream)?;
            self.put(D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL, sc_bytes)?;
            let descs = [
                buf_desc(D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, size_of::<DXVA_PicParams_H264>(), 0),
                buf_desc(D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX, size_of::<DXVA_Qmatrix_H264>(), 0),
                buf_desc(D3D11_VIDEO_DECODER_BUFFER_BITSTREAM, bs_padded, n_mbs),
                buf_desc(D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL, sc_bytes.len(), 0),
            ];
            self.vcontext.SubmitDecoderBuffers(&self.decoder, &descs)?;
            self.vcontext.DecoderEndFrame(&self.decoder)?;

            // Take the decoded picture out of the pool slice: either read it back
            // to I420 (CPU), or copy it (GPU-side) to a per-frame texture. Either
            // way the pool slice is then free for the DPB logic below to reuse.
            let payload = if gpu {
                Payload::Gpu(self.export_texture(out_slice)?)
            } else {
                Payload::Cpu(self.readback_i420(out_slice)?)
            };

            // Sliding-window DPB (short-term only). Reference pics stay resident.
            if is_ref {
                if self.dpb.len() >= sps.max_num_ref_frames.max(1) as usize {
                    if let Some((idx, evicted)) = self
                        .dpb
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, r)| r.frame_num)
                        .map(|(i, r)| (i, *r))
                    {
                        self.free.push(evicted.slice);
                        self.dpb.remove(idx);
                    }
                }
                self.dpb.push(DpbRef { slice: out_slice, frame_num: hdr.frame_num, top_poc, bottom_poc: top_poc });
            } else {
                self.free.push(out_slice);
            }
            Ok((payload, top_poc))
        }
    }

    /// GPU-side copy of one decode surface into its own NV12 texture (Phase-2
    /// output). Stays on the GPU — no CPU roundtrip — and decouples the frame's
    /// lifetime from the pool/DPB. `BIND_SHADER_RESOURCE` so ANGLE can sample it.
    unsafe fn export_texture(&self, slice: u8) -> anyhow::Result<GpuTex> {
        let tdesc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        self.device.CreateTexture2D(&tdesc, None, Some(&mut tex))?;
        let texture: ID3D11Texture2D = tex.ok_or_else(|| anyhow::anyhow!("no frame texture"))?;
        self.context.CopySubresourceRegion(&texture, 0, 0, 0, 0, &self.pool, slice as u32, None);
        Ok(GpuTex {
            texture,
            device: self.device.clone(),
            context: self.context.clone(),
        })
    }

    unsafe fn put(&self, ty: D3D11_VIDEO_DECODER_BUFFER_TYPE, data: &[u8]) -> anyhow::Result<()> {
        let mut size: u32 = 0;
        let mut ptr = std::ptr::null_mut();
        self.vcontext.GetDecoderBuffer(&self.decoder, ty, &mut size, &mut ptr)?;
        anyhow::ensure!(size as usize >= data.len(), "decoder buffer too small");
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
        self.vcontext.ReleaseDecoderBuffer(&self.decoder, ty)?;
        Ok(())
    }

    unsafe fn put_bitstream(&self, nal: &[u8]) -> anyhow::Result<usize> {
        let padded = (nal.len() + 127) & !127;
        let mut size: u32 = 0;
        let mut ptr = std::ptr::null_mut();
        self.vcontext.GetDecoderBuffer(&self.decoder, D3D11_VIDEO_DECODER_BUFFER_BITSTREAM, &mut size, &mut ptr)?;
        anyhow::ensure!(size as usize >= padded, "bitstream buffer too small");
        std::ptr::copy_nonoverlapping(nal.as_ptr(), ptr as *mut u8, nal.len());
        if padded > nal.len() {
            std::ptr::write_bytes((ptr as *mut u8).add(nal.len()), 0, padded - nal.len());
        }
        self.vcontext.ReleaseDecoderBuffer(&self.decoder, D3D11_VIDEO_DECODER_BUFFER_BITSTREAM)?;
        Ok(padded)
    }

    /// Copy one decode-pool slice to the reusable staging texture and pack it to
    /// tight I420 (the CPU output path — reuses `self.staging` every frame).
    unsafe fn readback_i420(&self, slice: u8) -> anyhow::Result<Vec<u8>> {
        self.context.CopySubresourceRegion(&self.staging, 0, 0, 0, 0, &self.pool, slice as u32, None);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        let out = pack_nv12_i420(m.pData as *const u8, m.RowPitch as usize, self.width, self.height);
        self.context.Unmap(&self.staging, 0);
        Ok(out)
    }
}

/// Read a standalone NV12 texture (a per-frame GPU output) back to tight I420 —
/// the fallback for a `GpuFrame` when the host can't import it. Allocates a
/// one-shot staging texture, so it's for the fallback lane, not steady state.
unsafe fn readback_nv12_texture(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> anyhow::Result<Vec<u8>> {
    let sdesc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    device.CreateTexture2D(&sdesc, None, Some(&mut staging))?;
    let staging: ID3D11Texture2D = staging.ok_or_else(|| anyhow::anyhow!("no staging"))?;
    context.CopyResource(&staging, tex);
    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
    context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
    let out = pack_nv12_i420(m.pData as *const u8, m.RowPitch as usize, width, height);
    context.Unmap(&staging, 0);
    Ok(out)
}

/// A mapped NV12 surface (`base`, `stride`) -> tightly-packed I420 (Y, then
/// planar U, V from the interleaved chroma), stripping row padding.
unsafe fn pack_nv12_i420(base: *const u8, stride: usize, width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let uv = base.add(stride * h);
    let mut out = vec![0u8; w * h + 2 * cw * ch];
    for y in 0..h {
        let src = std::slice::from_raw_parts(base.add(y * stride), w);
        out[y * w..y * w + w].copy_from_slice(src);
    }
    let (u_off, v_off) = (w * h, w * h + cw * ch);
    for y in 0..ch {
        let row = std::slice::from_raw_parts(uv.add(y * stride), cw * 2);
        for x in 0..cw {
            out[u_off + y * cw + x] = row[2 * x];
            out[v_off + y * cw + x] = row[2 * x + 1];
        }
    }
    out
}

fn buf_desc(ty: D3D11_VIDEO_DECODER_BUFFER_TYPE, size: usize, n_mbs: u32) -> D3D11_VIDEO_DECODER_BUFFER_DESC {
    let mut d: D3D11_VIDEO_DECODER_BUFFER_DESC = unsafe { zeroed() };
    d.BufferType = ty;
    d.DataSize = size as u32;
    d.NumMBsInBuffer = n_mbs;
    d
}

fn as_bytes<T>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, size_of::<T>()) }
}
