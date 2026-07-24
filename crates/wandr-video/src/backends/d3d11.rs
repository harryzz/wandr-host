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

use std::cell::Cell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::io::Cursor;
use std::mem::{size_of, zeroed};

use cros_codecs::codec::h264::parser::{Nalu, NaluType, Parser, Pps, SliceHeader, SliceType, Sps};
use cros_codecs::codec::h264::parser::{MaxLongTermFrameIdx, Slice};
use cros_codecs::codec::h264::picture::{Field, PictureData, Reference};
use cros_codecs::codec::h264::dpb::Dpb;

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
/// DXVA `DXVA_PicParams_H264::RefFrameList` is a fixed 16-entry array — the hard
/// ABI ceiling on simultaneous H.264 references. The decode-surface pool itself is
/// sized PER-STREAM from the SPS (`Dxva::new`), never a fixed count: a stream may
/// use anywhere from 1 to 16 references, and a fixed pool silently starves mid-GOP
/// once a GOP needs more surfaces than it has (this bit us with a 16-ref clip on an
/// 8-slot pool — "pool exhausted" at the 9th reference).
const DXVA_MAX_REFS: u32 = 16;
const DRM_FORMAT_NV12: u32 = 0x3231_564e; // 'N''V''1''2', for parity with the vaapi frame

// ── decode-on-ANGLE's-device handoff (Phase 2b zero-copy) ────────────────────
// The host extracts ANGLE's ID3D11Device (eglQueryDeviceAttribEXT(EGL_D3D11_
// DEVICE_ANGLE)) and sets it here, on the GL thread, before opening a decoder.
// Then decode lands on ANGLE's device and the output NV12 texture imports into
// ANGLE GL as a plain same-device alias — no shared handle, no keyed mutex.
// (Thread-local: the decoder and the GL context share one thread. Unset => the
// backend creates its own device, which is the CPU-readback lane.)
thread_local! {
    static ANGLE_D3D11_DEVICE: Cell<*mut c_void> = const { Cell::new(std::ptr::null_mut()) };
}

/// Set (or clear, with null) the ID3D11Device the d3d11 decoder should decode on.
/// Pass `ID3D11Device::as_raw()`; the backend takes its own reference. Call on the
/// GL thread before opening a decoder. Safe to leave unset — CPU readback still works.
pub fn set_angle_d3d11_device(device: *mut c_void) {
    ANGLE_D3D11_DEVICE.with(|c| c.set(device));
}

fn angle_d3d11_device() -> Option<*mut c_void> {
    ANGLE_D3D11_DEVICE.with(|c| {
        let p = c.get();
        (!p.is_null()).then_some(p)
    })
}

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
    level_idc: u8,
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
            level_idc: s.level_idc as u8,
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
        // device reports the codec = unsupported and software stays the fallback.
        match codec {
            Codec::H264 => probe_h264().unwrap_or(false),
            Codec::H265 => {
                let ok = probe_hevc().unwrap_or(false);
                log::info!("d3d11: HEVC (DXVA HEVC_VLD_MAIN) decode supported by this GPU: {ok}");
                ok
            }
            _ => false,
        }
    }
    fn supports_encode(&self, _codec: Codec) -> bool {
        false
    }
    fn open_decoder(&self, params: &DecoderParams) -> Result<Box<dyn Decoder>, CodecError> {
        // Fallback contract: fail here if HW can't decode, so the registry falls
        // back to software rather than us silently producing nothing.
        match params.codec {
            Codec::H264 => {
                if !probe_h264().unwrap_or(false) {
                    return Err(CodecError::InitFailed);
                }
                Ok(Box::new(D3d11Decoder::new()))
            }
            Codec::H265 => {
                if !probe_hevc().unwrap_or(false) {
                    return Err(CodecError::InitFailed);
                }
                Ok(Box::new(super::hevc::HevcD3d11Decoder::new()))
            }
            _ => Err(CodecError::Unsupported),
        }
    }
    fn open_encoder(&self, _params: &EncoderParams) -> Result<Box<dyn Encoder>, CodecError> {
        Err(CodecError::Unsupported)
    }
}

/// True if the default hardware adapter exposes an HEVC (Main / Main10) VLD
/// decode profile — the increment-1 gate for Windows HW H.265 (task 117 M2).
fn probe_hevc() -> anyhow::Result<bool> {
    use super::hevc_dxva::{HEVC_VLD_MAIN, HEVC_VLD_MAIN10};
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
            if g == HEVC_VLD_MAIN || g == HEVC_VLD_MAIN10 {
                return Ok(true);
            }
        }
        Ok(false)
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
    ready: VecDeque<Decoded>, // display-order output from the DPB, awaiting emit
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
            ready: VecDeque::new(),
            cur: None,
            // Emit GPU-texture frames (for the host's ANGLE zero-copy import)
            // automatically whenever the host pointed us at ANGLE's device — then
            // the output texture is a same-device alias the host can import. With
            // no ANGLE device (headless) we stay on CPU readback. `=1` forces it.
            gpu: std::env::var("WANDR_VIDEO_D3D11_GPU").is_ok() || angle_d3d11_device().is_some(),
        }
    }

}

// SAFETY: same reasoning as the vaapi backend. The cros-codecs reference `Dpb`
// inside the `Dxva` core carries non-atomic `Rc<RefCell<PictureData>>` refcounts
// and so is not `Send`/`Sync` by default, but the whole `D3d11Decoder` — pool,
// DPB, and every `Rc` inside it — is one self-contained object the host owns in a
// wasmtime `ResourceTable` and drives from a single store thread. Moving it
// between threads is sound; sharing it is not, and `Sync` is deliberately NOT
// claimed. No `Rc` clone escapes (outputs are standalone COM textures / I420).
unsafe impl Send for D3d11Decoder {}

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
        let mut first_slice: Option<Slice> = None;
        let mut is_idr = false;

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
                    // The raw NAL WITHOUT its start code (a fixed 3-byte code is
                    // prepended in decode_picture, matching ffmpeg's DXVA layout).
                    let bytes = nalu.as_ref().to_vec();
                    if first_slice.is_none() {
                        is_idr = nalu.header.idr_pic_flag;
                        let s = parser.parse_slice_header(nalu).map_err(|e| {
                            log::warn!("d3d11: slice-header parse failed: {e:?}");
                            CodecError::BadFrame
                        })?;
                        first_slice = Some(s);
                    }
                    slice_nals.push(bytes);
                }
                _ => {}
            }
        }

        if slice_nals.is_empty() {
            return Ok(()); // config-only AU (SPS/PPS), nothing to decode
        }
        let slice = first_slice.ok_or(CodecError::BadFrame)?;
        let hdr = &slice.header;
        let sps_bits = self.sps.clone().ok_or(CodecError::BadFrame)?;
        let pps_bits = self.pps.clone().ok_or(CodecError::BadFrame)?;

        // The reference-DPB drive + POC need the full cros-codecs SPS/PPS (the
        // SpsBits/PpsBits copies only carry the scalars the DXVA pic-params need).
        let cpps = parser
            .get_pps(hdr.pic_parameter_set_id)
            .ok_or(CodecError::BadFrame)?
            .clone();
        let csps = parser
            .get_sps(cpps.seq_parameter_set_id)
            .ok_or(CodecError::BadFrame)?
            .clone();

        if self.core.is_none() {
            // Pool = every DPB reference this stream can hold (SPS num_ref_frames,
            // capped at the 16-entry DXVA RefFrameList) + the picture being decoded
            // + one slack slot. Derived per-stream so a high-reference clip does not
            // starve the pool mid-GOP.
            let slots = (csps.max_num_ref_frames as u32).min(DXVA_MAX_REFS) + 2;
            let mut core =
                Dxva::new(csps.width(), csps.height(), slots).map_err(|_| CodecError::InitFailed)?;
            core.set_dpb_limits(&csps);
            self.core = Some(core);
        }
        let gpu = self.gpu;
        let pts = chunk.timestamp_us;
        let pic = PictureData::new_from_slice(&slice, &csps, pts as u64, None);
        let core = self.core.as_mut().unwrap();
        let bumped = core
            .decode_picture(pic, &csps, &sps_bits, &pps_bits, hdr, is_idr, pts, &slice_nals, gpu)
            .map_err(|e| {
                log::warn!(
                    "d3d11: decode_picture failed: {e:#} (frame_num={} slice_type={:?} idr={} slices={} dpb={})",
                    hdr.frame_num, hdr.slice_type, is_idr, slice_nals.len(), core.dpb.len(),
                );
                CodecError::BadFrame
            })?;
        // The DPB emits pictures in display order (POC); queue them for the host.
        self.ready.extend(bumped);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), CodecError> {
        if let Some(core) = self.core.as_mut() {
            let rest = core.drain_dpb();
            self.ready.extend(rest);
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<(), CodecError> {
        // Seek: drop pending output + DPB state; the caller must feed a keyframe
        // next, per the trait contract.
        self.ready.clear();
        self.cur = None;
        if let Some(core) = self.core.as_mut() {
            core.reset_state();
        }
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

// Previous-picture POC state (spec 8.2.1), carried across pictures. Copied from
// cros-codecs' stateless H.264 decoder so our POC derivation matches it exactly.
struct PrevReferencePicInfo {
    frame_num: u32,
    has_mmco_5: bool,
    top_field_order_cnt: i32,
    pic_order_cnt_msb: i32,
    pic_order_cnt_lsb: i32,
    field: Field,
}
impl Default for PrevReferencePicInfo {
    fn default() -> Self {
        Self {
            frame_num: 0,
            has_mmco_5: false,
            top_field_order_cnt: 0,
            pic_order_cnt_msb: 0,
            pic_order_cnt_lsb: 0,
            field: Field::Frame,
        }
    }
}
impl PrevReferencePicInfo {
    fn fill(&mut self, pic: &PictureData) {
        self.has_mmco_5 = pic.has_mmco_5;
        self.top_field_order_cnt = pic.top_field_order_cnt;
        self.pic_order_cnt_msb = pic.pic_order_cnt_msb;
        self.pic_order_cnt_lsb = pic.pic_order_cnt_lsb;
        self.field = pic.field;
        self.frame_num = pic.frame_num;
    }
}

#[derive(Default)]
struct PrevPicInfo {
    frame_num: u32,
    frame_num_offset: u32,
    has_mmco_5: bool,
}
impl PrevPicInfo {
    fn fill(&mut self, pic: &PictureData) {
        self.frame_num = pic.frame_num;
        self.has_mmco_5 = pic.has_mmco_5;
        self.frame_num_offset = pic.frame_num_offset;
    }
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
    slots: u32, // decode-surface pool size, derived from the SPS (refs + current + slack)
    free: Vec<u8>,
    // cros-codecs DPB (T = a per-picture id). Drives POC, MMCO / sliding-window
    // marking, reference-list construction AND display-order bumping correctly (the
    // hand-rolled sliding window it replaces broke on B-pyramid / MMCO streams).
    dpb: Dpb<u64>,
    prev_ref: PrevReferencePicInfo,
    prev_pic: PrevPicInfo,
    max_lt: MaxLongTermFrameIdx,
    // Decoded pictures awaiting display, keyed by the DPB id (decoupled from the
    // pool slot — a picture's pixels are copied out at decode, its slot is only
    // held while it is a decode reference). `slot_of` maps a *reference* picture's
    // id to the pool slot it occupies, so retired references free their slot.
    pending: std::collections::HashMap<u64, Decoded>,
    slot_of: std::collections::HashMap<u64, u8>,
    next_id: u64,
    width: u32,
    height: u32,
    feedback: u32,
}

impl Dxva {
    fn new(width: u32, height: u32, slots: u32) -> anyhow::Result<Self> {
        unsafe {
            // Decode on ANGLE's device when the host provided one (zero-copy path),
            // else create our own (CPU-readback path).
            let (device, context): (ID3D11Device, ID3D11DeviceContext) = match angle_d3d11_device() {
                Some(ptr) => {
                    let device = ID3D11Device::from_raw_borrowed(&ptr)
                        .ok_or_else(|| anyhow::anyhow!("null ANGLE device"))?
                        .clone();
                    let context = device.GetImmediateContext()?;
                    (device, context)
                }
                None => {
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
                    (
                        device.ok_or_else(|| anyhow::anyhow!("no device"))?,
                        context.ok_or_else(|| anyhow::anyhow!("no context"))?,
                    )
                }
            };
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
                ArraySize: slots,
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
            for s in 0..slots {
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
                slots,
                free: (0..slots as u8).rev().collect(),
                dpb: Dpb::default(),
                prev_ref: PrevReferencePicInfo::default(),
                prev_pic: PrevPicInfo::default(),
                max_lt: MaxLongTermFrameIdx::default(),
                pending: std::collections::HashMap::new(),
                slot_of: std::collections::HashMap::new(),
                next_id: 0,
                width,
                height,
                feedback: 0,
            })
        }
    }

    fn set_dpb_limits(&mut self, sps: &Sps) {
        // Same policy as cros-codecs' apply_sps: the DPB holds up to max_dpb_frames
        // pictures; the reorder limit bounds output latency (bump earlier if it
        // would exceed the DPB size).
        let max_dpb = sps.max_dpb_frames();
        let max_reorder = sps.max_num_order_frames() as usize;
        let max_reorder = if max_reorder > max_dpb { 0 } else { max_reorder };
        self.dpb.set_limits(max_dpb, max_reorder);
    }

    /// Output every buffered picture in display order and empty the DPB (IDR /
    /// MMCO-5 / end-of-stream). Returns them lowest-POC first.
    fn drain_dpb(&mut self) -> Vec<Decoded> {
        let mut out = Vec::new();
        for h in self.dpb.drain() {
            if let Some(id) = h {
                if let Some(d) = self.pending.remove(&id) {
                    out.push(d);
                }
            }
        }
        out
    }

    /// Seek/reset: drop all buffered state. The next access unit must be an IDR.
    fn reset_state(&mut self) {
        self.dpb.clear();
        self.pending.clear();
        self.slot_of.clear();
        self.free = (0..self.slots as u8).rev().collect();
        self.prev_ref = PrevReferencePicInfo::default();
        self.prev_pic = PrevPicInfo::default();
        self.max_lt = MaxLongTermFrameIdx::default();
    }

    /// POC derivation (spec 8.2.1), a faithful copy of cros-codecs'
    /// `compute_pic_order_count` so short/long-term reference ordering matches the
    /// reference decoder for POC types 0/1/2.
    fn compute_pic_order_count(
        &mut self,
        pic: &mut PictureData,
        sps: &Sps,
        is_idr: bool,
    ) -> anyhow::Result<()> {
        match pic.pic_order_cnt_type {
            0 => {
                let prev_pic_order_cnt_msb;
                let prev_pic_order_cnt_lsb;
                if is_idr {
                    prev_pic_order_cnt_lsb = 0;
                    prev_pic_order_cnt_msb = 0;
                } else if self.prev_ref.has_mmco_5 {
                    if !matches!(self.prev_ref.field, Field::Bottom) {
                        prev_pic_order_cnt_msb = 0;
                        prev_pic_order_cnt_lsb = self.prev_ref.top_field_order_cnt;
                    } else {
                        prev_pic_order_cnt_msb = 0;
                        prev_pic_order_cnt_lsb = 0;
                    }
                } else {
                    prev_pic_order_cnt_msb = self.prev_ref.pic_order_cnt_msb;
                    prev_pic_order_cnt_lsb = self.prev_ref.pic_order_cnt_lsb;
                }

                let max_pic_order_cnt_lsb = 1 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4);

                pic.pic_order_cnt_msb = if (pic.pic_order_cnt_lsb < prev_pic_order_cnt_lsb)
                    && (prev_pic_order_cnt_lsb - pic.pic_order_cnt_lsb >= max_pic_order_cnt_lsb / 2)
                {
                    prev_pic_order_cnt_msb + max_pic_order_cnt_lsb
                } else if (pic.pic_order_cnt_lsb > prev_pic_order_cnt_lsb)
                    && (pic.pic_order_cnt_lsb - prev_pic_order_cnt_lsb > max_pic_order_cnt_lsb / 2)
                {
                    prev_pic_order_cnt_msb - max_pic_order_cnt_lsb
                } else {
                    prev_pic_order_cnt_msb
                };

                if !matches!(pic.field, Field::Bottom) {
                    pic.top_field_order_cnt = pic.pic_order_cnt_msb + pic.pic_order_cnt_lsb;
                }
                if !matches!(pic.field, Field::Top) {
                    if matches!(pic.field, Field::Frame) {
                        pic.bottom_field_order_cnt =
                            pic.top_field_order_cnt + pic.delta_pic_order_cnt_bottom;
                    } else {
                        pic.bottom_field_order_cnt = pic.pic_order_cnt_msb + pic.pic_order_cnt_lsb;
                    }
                }
            }
            1 => {
                if self.prev_pic.has_mmco_5 {
                    self.prev_pic.frame_num_offset = 0;
                }
                if is_idr {
                    pic.frame_num_offset = 0;
                } else if self.prev_pic.frame_num > pic.frame_num {
                    pic.frame_num_offset = self.prev_pic.frame_num_offset + sps.max_frame_num();
                } else {
                    pic.frame_num_offset = self.prev_pic.frame_num_offset;
                }

                let mut abs_frame_num = if sps.num_ref_frames_in_pic_order_cnt_cycle != 0 {
                    pic.frame_num_offset + pic.frame_num
                } else {
                    0
                };
                if pic.nal_ref_idc == 0 && abs_frame_num > 0 {
                    abs_frame_num -= 1;
                }

                let mut expected_pic_order_cnt = 0;
                if abs_frame_num > 0 {
                    if sps.num_ref_frames_in_pic_order_cnt_cycle == 0 {
                        anyhow::bail!("invalid num_ref_frames_in_pic_order_cnt_cycle");
                    }
                    let pic_order_cnt_cycle_cnt =
                        (abs_frame_num - 1) / sps.num_ref_frames_in_pic_order_cnt_cycle as u32;
                    expected_pic_order_cnt =
                        pic_order_cnt_cycle_cnt as i32 * sps.expected_delta_per_pic_order_cnt_cycle;
                    for i in 0..sps.num_ref_frames_in_pic_order_cnt_cycle {
                        expected_pic_order_cnt += sps.offset_for_ref_frame[i as usize];
                    }
                }
                if pic.nal_ref_idc == 0 {
                    expected_pic_order_cnt += sps.offset_for_non_ref_pic;
                }

                if matches!(pic.field, Field::Frame) {
                    pic.top_field_order_cnt = expected_pic_order_cnt + pic.delta_pic_order_cnt0;
                    pic.bottom_field_order_cnt = pic.top_field_order_cnt
                        + sps.offset_for_top_to_bottom_field
                        + pic.delta_pic_order_cnt1;
                } else if !matches!(pic.field, Field::Bottom) {
                    pic.top_field_order_cnt = expected_pic_order_cnt + pic.delta_pic_order_cnt0;
                } else {
                    pic.bottom_field_order_cnt = expected_pic_order_cnt
                        + sps.offset_for_top_to_bottom_field
                        + pic.delta_pic_order_cnt0;
                }
            }
            2 => {
                if self.prev_pic.has_mmco_5 {
                    self.prev_pic.frame_num_offset = 0;
                }
                if is_idr {
                    pic.frame_num_offset = 0;
                } else if self.prev_pic.frame_num > pic.frame_num {
                    pic.frame_num_offset = self.prev_pic.frame_num_offset + sps.max_frame_num();
                } else {
                    pic.frame_num_offset = self.prev_pic.frame_num_offset;
                }

                let pic_order_cnt = if is_idr {
                    0
                } else if pic.nal_ref_idc == 0 {
                    2 * (pic.frame_num_offset + pic.frame_num) as i32 - 1
                } else {
                    2 * (pic.frame_num_offset + pic.frame_num) as i32
                };
                if matches!(pic.field, Field::Frame | Field::Top) {
                    pic.top_field_order_cnt = pic_order_cnt;
                }
                if matches!(pic.field, Field::Frame | Field::Bottom) {
                    pic.bottom_field_order_cnt = pic_order_cnt;
                }
            }
            other => anyhow::bail!("invalid pic_order_cnt_type: {other}"),
        }

        pic.pic_order_cnt = match pic.field {
            Field::Frame => std::cmp::min(pic.top_field_order_cnt, pic.bottom_field_order_cnt),
            Field::Top => pic.top_field_order_cnt,
            Field::Bottom => pic.bottom_field_order_cnt,
        };
        Ok(())
    }

    /// Reference-picture marking (spec 8.2.5) via cros-codecs' DPB: adaptive
    /// (MMCO) when signalled, else the sliding window. IDR clears all references.
    fn reference_pic_marking(
        &mut self,
        pic: &mut PictureData,
        sps: &Sps,
        is_idr: bool,
    ) -> anyhow::Result<()> {
        if is_idr {
            self.dpb.mark_all_as_unused_for_ref();
            if pic.ref_pic_marking.long_term_reference_flag {
                pic.set_reference(Reference::LongTerm, false);
                pic.long_term_frame_idx = 0;
                self.max_lt = MaxLongTermFrameIdx::Idx(0);
            } else {
                pic.set_reference(Reference::ShortTerm, false);
                self.max_lt = MaxLongTermFrameIdx::NoLongTermFrameIndices;
            }
            return Ok(());
        }
        if pic.ref_pic_marking.adaptive_ref_pic_marking_mode_flag {
            let markings = pic.ref_pic_marking.clone();
            for marking in &markings.inner {
                match marking.memory_management_control_operation {
                    0 => break,
                    1 => self.dpb.mmco_op_1(pic, marking)?,
                    2 => self.dpb.mmco_op_2(pic, marking)?,
                    3 => self.dpb.mmco_op_3(pic, marking)?,
                    4 => self.max_lt = self.dpb.mmco_op_4(marking),
                    5 => self.max_lt = self.dpb.mmco_op_5(pic),
                    6 => self.dpb.mmco_op_6(pic, marking),
                    other => anyhow::bail!("unknown MMCO {other}"),
                }
            }
        } else {
            self.dpb.sliding_window_marking(pic, sps);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn build_pic_params(
        &self,
        sps: &SpsBits,
        pps: &PpsBits,
        hdr: &SliceHeader,
        is_ref: bool,
        out_slice: u8,
        cur_top: i32,
        cur_bottom: i32,
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
        bf |= ((sps.level_idc >= 31) as u16) << 14; // MinLumaBipredSize8x8Flag
        bf |= (is_intra as u16) << 15;
        pp.wBitFields = bf;

        pp.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8;
        pp.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8;
        pp.Reserved16Bits = 3;
        pp.StatusReportFeedbackNumber = feedback;

        pp.RefFrameList = [INVALID_ENTRY; 16];
        pp.CurrFieldOrderCnt = [cur_top, cur_bottom];
        // Reference frames come straight from the cros DPB. The driver builds
        // RefPicList0/1 itself from these (surface slot + POC + FrameNum/LongTerm
        // idx + used flags); the AssociatedFlag high bit marks long-term.
        let mut used: u32 = 0;
        let mut k = 0usize;
        for e in self.dpb.entries().iter() {
            if k >= 16 {
                break;
            }
            let rp = e.pic.borrow();
            let long_term = matches!(rp.reference(), Reference::LongTerm);
            if !long_term && !matches!(rp.reference(), Reference::ShortTerm) {
                continue; // non-reference (output-buffered) or retired
            }
            // Map the DPB id back to the pool slot the reference occupies.
            let slot = match e.reference.and_then(|id| self.slot_of.get(&id)) {
                Some(&s) => s,
                None => continue,
            };
            pp.RefFrameList[k] = (slot & 0x7F) | if long_term { 0x80 } else { 0 };
            pp.FieldOrderCntList[k] = [rp.top_field_order_cnt, rp.bottom_field_order_cnt];
            pp.FrameNumList[k] = if long_term {
                rp.long_term_frame_idx as u16
            } else {
                rp.frame_num as u16
            };
            used |= 3u32 << (2 * k);
            k += 1;
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

    #[allow(clippy::too_many_arguments)]
    fn decode_picture(
        &mut self,
        mut pic: PictureData,
        sps: &Sps,
        sps_bits: &SpsBits,
        pps: &PpsBits,
        hdr: &SliceHeader,
        is_idr: bool,
        pts: i64,
        slice_nals: &[Vec<u8>],
        gpu: bool,
    ) -> anyhow::Result<Vec<Decoded>> {
        unsafe {
            let mut out: Vec<Decoded> = Vec::new();
            // POC first (needs prev-picture state, before it is overwritten below).
            self.compute_pic_order_count(&mut pic, sps, is_idr)?;

            if is_idr {
                // C.4.4 / init_current_pic: flush prior output, then reset all state.
                out.extend(self.drain_dpb());
                self.reset_state();
            }

            // PicNum/FrameNumWrap for the driver's reference-list construction.
            self.dpb
                .update_pic_nums(u32::from(hdr.frame_num), sps.max_frame_num(), &pic);

            let is_ref = pic.nal_ref_idc != 0;
            let out_slice = self.free.pop().ok_or_else(|| anyhow::anyhow!("pool exhausted"))?;
            self.feedback = self.feedback.wrapping_add(1).max(1);
            let (cur_top, cur_bottom) = (pic.top_field_order_cnt, pic.bottom_field_order_cnt);
            let poc = pic.pic_order_cnt;

            let pp = self.build_pic_params(
                sps_bits, pps, hdr, is_ref, out_slice, cur_top, cur_bottom, self.feedback,
            );
            let qm = DXVA_Qmatrix_H264 {
                bScalingLists4x4: [[16u8; 16]; 6],
                bScalingLists8x8: [[16u8; 64]; 2],
            };

            // All slices of the picture -> one bitstream buffer + a packed
            // (10-byte-stride) SliceControl array; buffer order PP, IQ, BS, SC.
            let n_mbs = (self.width / 16) * (self.height / 16);
            // Annex-B bitstream: a FIXED 3-byte start code before each slice NAL,
            // with BSNALunitDataLocation pointing at the start code and
            // SliceBytesInBuffer covering start-code + NAL (ffmpeg dxva2_h264
            // commit_bitstream_and_slice_buffer). `slice_nals` hold the raw NAL
            // WITHOUT any start code, so the length is exact — a variable-length
            // (3- vs 4-byte) start code left CABAC slices misaligned.
            const START_CODE: [u8; 3] = [0, 0, 1];
            let mut bitstream: Vec<u8> = Vec::new();
            let mut slice_ctl: Vec<DXVA_Slice_H264_Short> = Vec::new();
            for nal in slice_nals {
                let loc = bitstream.len() as u32;
                bitstream.extend_from_slice(&START_CODE);
                bitstream.extend_from_slice(nal);
                slice_ctl.push(DXVA_Slice_H264_Short {
                    BSNALunitDataLocation: loc,
                    SliceBytesInBuffer: (START_CODE.len() + nal.len()) as u32,
                    wBadSliceChopping: 0,
                });
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
            self.put(D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, as_bytes(&pp))?;
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

            // Copy the decoded picture out of the pool slice — read back to I420
            // (CPU) or GPU-copy to its own NV12 texture — so the frame's display
            // lifetime is decoupled from the pool. A reference picture's slice
            // still stays resident in the pool (it is a decode reference) until the
            // DPB retires it below.
            let payload = if gpu {
                Payload::Gpu(self.export_texture(out_slice)?)
            } else {
                Payload::Cpu(self.readback_i420(out_slice)?)
            };

            // Buffer the decoded picture for display, keyed by a DPB id — its pixels
            // are now independent of the pool slot it was decoded into.
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            self.pending.insert(
                id,
                Decoded { poc, pts, width: self.width, height: self.height, payload },
            );

            // Reference marking (8.2.5): MMCO or sliding window retires references.
            if is_ref {
                self.reference_pic_marking(&mut pic, sps, is_idr)?;
                self.prev_ref.fill(&pic);
            }
            self.prev_pic.fill(&pic);
            if pic.has_mmco_5 {
                out.extend(self.drain_dpb());
            }

            // Bump ready pictures out in display order (C.4.5.3). This is ALSO the
            // only path that removes retired references from the DPB.
            for h in self.dpb.bump_as_needed(&pic) {
                if let Some(bid) = h {
                    if let Some(d) = self.pending.remove(&bid) {
                        out.push(d);
                    }
                }
            }

            // Keep the picture in the DPB if it is a reference (which also pins its
            // pool slot) or if there is still room for display reordering; else emit
            // it now (non-reference, no buffer).
            if is_ref {
                self.slot_of.insert(id, out_slice);
                self.dpb
                    .store_picture(pic.into_rc(), Some(id))
                    .map_err(|e| anyhow::anyhow!("dpb store: {e}"))?;
            } else if self.dpb.has_empty_frame_buffer() {
                self.dpb
                    .store_picture(pic.into_rc(), Some(id))
                    .map_err(|e| anyhow::anyhow!("dpb store: {e}"))?;
            } else if let Some(d) = self.pending.remove(&id) {
                out.push(d);
            }

            // Reconcile the pool: a slot is live iff a still-referenced DPB entry
            // holds it (via `slot_of`, which only tracks reference pictures). Retired
            // references and output-buffered non-reference pictures free their slot.
            let live_ids: std::collections::HashSet<u64> =
                self.dpb.entries().iter().filter_map(|e| e.reference).collect();
            self.slot_of.retain(|id, _| live_ids.contains(id));
            let live_slots: std::collections::HashSet<u8> = self.slot_of.values().copied().collect();
            self.free = (0..self.slots as u8).filter(|s| !live_slots.contains(s)).collect();

            Ok(out)
        }
    }

    /// GPU-side copy of one decode surface into its own NV12 texture (Phase-2
    /// output). Stays on the GPU — no CPU roundtrip — and decouples the frame's
    /// lifetime from the pool/DPB. `SHADER_RESOURCE | RENDER_TARGET`: ANGLE's
    /// EGL_ANGLE_image_d3d11_texture import needs the per-plane R8/RG8 views to be
    /// render-targetable (verified in repros/d3d11-angle-import-spike).
    unsafe fn export_texture(&self, slice: u8) -> anyhow::Result<GpuTex> {
        let tdesc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
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
