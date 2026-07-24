//! Hardware HEVC/H.265 decode on Windows via DXVA2 / `ID3D11VideoDecoder`
//! (task 117 M2). The H.265 peer of `d3d11.rs` — same D3D11 device/pool/readback
//! machinery, cros-codecs' pure-Rust HEVC parser + `PictureData::new_from_slice`
//! (POC, 8.3.1), and a thin driver here for the RPS derivation (8.3.2), the DPB,
//! and display-order bumping. Decodes each picture through the fixed-function
//! decoder and reads the NV12 result back to tight I420 (the CPU lane, mirroring
//! VA-API's readback path).
//!
//! ‼️ `DXVA_Slice_HEVC_Short` is 10 bytes (`#pragma pack(1)`), the DXVA HEVC
//! picture-params bitfield words are packed LSB-first, and the bitstream carries a
//! fixed 3-byte start code per slice NAL — all mirroring the proven H.264 backend.
#![allow(non_snake_case, non_camel_case_types)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::mem::{size_of, zeroed};
use std::rc::Rc;

use cros_codecs::codec::h265::dpb::{Dpb, DpbEntry};
use cros_codecs::codec::h265::parser::{Nalu, NaluType, Parser, Pps, ShortTermRefPicSet, Slice, Sps};
use cros_codecs::codec::h265::picture::{PictureData, Reference};

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

use super::hevc_dxva::*;
use crate::{Chunk, CodecError, Decoder, Frame, I420Ref};

const START_CODE: [u8; 3] = [0, 0, 1];

/// One decoded picture in display-buffering order (CPU I420 readback).
struct Decoded {
    poc: i32,
    pts: i64,
    width: u32,
    height: u32,
    i420: Vec<u8>,
}

pub struct HevcD3d11Decoder {
    // Raw VPS/SPS/PPS NALs kept so each decode() can build a FRESH cros-codecs
    // Parser primed with them (the Parser holds Rc and is not Send).
    vps_nal: Option<Vec<u8>>,
    sps_nal: Option<Vec<u8>>,
    pps_nal: Option<Vec<u8>>,
    core: Option<HevcDxva>,
    ready: VecDeque<Decoded>,
    cur: Option<Vec<u8>>,
}

impl HevcD3d11Decoder {
    pub fn new() -> Self {
        Self {
            vps_nal: None,
            sps_nal: None,
            pps_nal: None,
            core: None,
            ready: VecDeque::new(),
            cur: None,
        }
    }
}

// SAFETY: identical reasoning to the H.264 d3d11 backend — the whole decoder
// (pool, DPB, every Rc inside it) is one self-contained object the host owns and
// drives from a single store thread. Moving it between threads is sound; sharing
// is not (Sync is NOT claimed). No Rc clone escapes (outputs are standalone I420).
unsafe impl Send for HevcD3d11Decoder {}

impl Decoder for HevcD3d11Decoder {
    fn decode(&mut self, chunk: Chunk<'_>) -> Result<(), CodecError> {
        let mut parser = Parser::default();
        for nal in [&self.vps_nal, &self.sps_nal, &self.pps_nal].into_iter().flatten() {
            let mut c = Cursor::new(nal.as_slice());
            if let Ok(n) = Nalu::next(&mut c) {
                match n.header.type_ {
                    NaluType::VpsNut => {
                        let _ = parser.parse_vps(&n);
                    }
                    NaluType::SpsNut => {
                        let _ = parser.parse_sps(&n);
                    }
                    NaluType::PpsNut => {
                        let _ = parser.parse_pps(&n);
                    }
                    _ => {}
                }
            }
        }

        // One chunk = one access unit (the guest demuxes). Collect its slices.
        let mut cursor = Cursor::new(chunk.data);
        let mut slice_nals: Vec<Vec<u8>> = Vec::new();
        let mut first_slice: Option<Slice> = None;

        while let Ok(nalu) = Nalu::next(&mut cursor) {
            let t = nalu.header.type_;
            match t {
                NaluType::VpsNut => {
                    self.vps_nal = Some(nalu.data.to_vec());
                    parser.parse_vps(&nalu).map_err(|_| CodecError::BadFrame)?;
                }
                NaluType::SpsNut => {
                    self.sps_nal = Some(nalu.data.to_vec());
                    parser.parse_sps(&nalu).map_err(|_| CodecError::BadFrame)?;
                }
                NaluType::PpsNut => {
                    self.pps_nal = Some(nalu.data.to_vec());
                    parser.parse_pps(&nalu).map_err(|_| CodecError::BadFrame)?;
                }
                _ if is_vcl(t) => {
                    let bytes = nalu.as_ref().to_vec();
                    if first_slice.is_none() {
                        let s = parser.parse_slice_header(nalu).map_err(|e| {
                            log::warn!("hevc: slice-header parse failed: {e:?}");
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
            return Ok(()); // config-only AU (VPS/SPS/PPS)
        }
        let slice = first_slice.ok_or(CodecError::BadFrame)?;
        let pps = parser
            .get_pps(slice.header.pic_parameter_set_id)
            .ok_or(CodecError::BadFrame)?
            .clone();
        let sps = pps.sps.clone();

        if self.core.is_none() {
            let max_dpb = std::cmp::min(sps.max_dpb_size(), 16) as u32;
            let slots = max_dpb + 2;
            let core =
                HevcDxva::new(u32::from(sps.width()), u32::from(sps.height()), slots, max_dpb as usize)
                    .map_err(|e| {
                    log::warn!("hevc: DXVA init failed: {e:#}");
                    CodecError::InitFailed
                })?;
            self.core = Some(core);
            log::info!(
                "hevc: DXVA HEVC decoder created {}x{} ({} slots)",
                sps.width(),
                sps.height(),
                slots
            );
        }
        let core = self.core.as_mut().unwrap();
        let max_poc_lsb = 1i32 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
        core.max_pic_order_cnt_lsb = max_poc_lsb;

        let pic = PictureData::new_from_slice(
            &slice,
            core.first_picture_in_bitstream,
            core.first_picture_after_eos,
            core.prev_tid_0_pic.as_ref(),
            max_poc_lsb,
        );

        // 8.1.3: an IRAP sets NoRaslOutputFlag; a RASL leading picture that follows
        // such an IRAP references unavailable pre-IRAP frames and is not decodable —
        // drop it (this is what produced the negative-POC "no ref" warnings).
        if pic.nalu_type.is_irap() {
            core.irap_no_rasl_output_flag = pic.no_rasl_output_flag;
        } else if pic.nalu_type.is_rasl() && core.irap_no_rasl_output_flag {
            core.first_picture_in_bitstream = false;
            core.first_picture_after_eos = false;
            return Ok(());
        }

        let bumped = core
            .decode_picture(pic, &sps, &pps, &slice, chunk.timestamp_us, &slice_nals)
            .map_err(|e| {
                log::warn!("hevc: decode_picture failed: {e:#}");
                CodecError::BadFrame
            })?;
        self.ready.extend(bumped);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), CodecError> {
        if let Some(core) = self.core.as_mut() {
            self.ready.extend(core.drain_dpb());
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<(), CodecError> {
        self.ready.clear();
        self.cur = None;
        if let Some(core) = self.core.as_mut() {
            core.reset_state();
        }
        Ok(())
    }

    fn frames_in_flight_limit(&self) -> Option<usize> {
        None // CPU readback: a held frame does not starve the decoder
    }

    fn next_frame(&mut self) -> Option<Frame<'_>> {
        let d = self.ready.pop_front()?;
        let (w, h, pts) = (d.width, d.height, d.pts);
        self.cur = Some(d.i420);
        let buf = self.cur.as_ref().unwrap();
        let (cw, ch) = ((w as usize).div_ceil(2), (h as usize).div_ceil(2));
        let yl = (w * h) as usize;
        let cl = cw * ch;
        Some(Frame::cpu(I420Ref {
            y: &buf[..yl],
            y_stride: w,
            u: &buf[yl..yl + cl],
            u_stride: cw as u32,
            v: &buf[yl + cl..yl + 2 * cl],
            v_stride: cw as u32,
            width: w,
            height: h,
            timestamp_us: pts,
        }))
    }
}

/// HEVC VCL NAL types are 0..=31 (non-VCL are 32..=63). cros-codecs' `NaluType`
/// is `#[repr(u8)]` with the spec discriminants, so the range test is exact.
fn is_vcl(t: NaluType) -> bool {
    (t as u8) <= 31
}

// ── the DXVA core: device, decoder, surface pool, per-picture decode ─────────

/// The per-picture RPS (8.3.2), derived into DPB references matched by POC.
#[derive(Default)]
struct RpsRefs {
    st_curr_before: Vec<DpbEntry<u64>>,
    st_curr_after: Vec<DpbEntry<u64>>,
    lt_curr: Vec<DpbEntry<u64>>,
}

struct HevcDxva {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vcontext: ID3D11VideoContext,
    decoder: ID3D11VideoDecoder,
    views: Vec<ID3D11VideoDecoderOutputView>,
    pool: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    slots: u32,
    free: Vec<u8>,
    dpb: Dpb<u64>,
    pending: HashMap<u64, Decoded>,
    slot_of: HashMap<u64, u8>,
    next_id: u64,
    width: u32,
    height: u32,
    feedback: u32,
    // 8.3.1 / 8.3.2 decode state, carried across pictures.
    prev_tid_0_pic: Option<PictureData>,
    first_picture_in_bitstream: bool,
    first_picture_after_eos: bool,
    /// NoRaslOutputFlag of the last IRAP — RASL leading pictures that follow such
    /// an IRAP reference unavailable pre-IRAP frames and are dropped (8.1.3).
    irap_no_rasl_output_flag: bool,
    max_pic_order_cnt_lsb: i32,
    rps: RpsRefs,
}

impl HevcDxva {
    fn new(width: u32, height: u32, slots: u32, max_dpb: usize) -> anyhow::Result<Self> {
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
            let device = device.ok_or_else(|| anyhow::anyhow!("no device"))?;
            let context = context.ok_or_else(|| anyhow::anyhow!("no context"))?;
            let vdevice: ID3D11VideoDevice = device.cast()?;
            let vcontext: ID3D11VideoContext = context.cast()?;

            let desc = D3D11_VIDEO_DECODER_DESC {
                Guid: HEVC_VLD_MAIN,
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
                // Prefer short-slice bitstream (ConfigBitstreamRaw == 2), like H.264.
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
            anyhow::ensure!(have, "no HEVC decoder config");
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
                    DecodeProfile: HEVC_VLD_MAIN,
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
                dpb: {
                    let mut d = Dpb::default();
                    d.set_max_num_pics(max_dpb);
                    d
                },
                pending: HashMap::new(),
                slot_of: HashMap::new(),
                next_id: 0,
                width,
                height,
                feedback: 0,
                prev_tid_0_pic: None,
                first_picture_in_bitstream: true,
                first_picture_after_eos: true,
                irap_no_rasl_output_flag: false,
                max_pic_order_cnt_lsb: 0,
                rps: RpsRefs::default(),
            })
        }
    }

    fn reset_state(&mut self) {
        self.dpb.clear();
        self.pending.clear();
        self.slot_of.clear();
        self.free = (0..self.slots as u8).rev().collect();
        self.prev_tid_0_pic = None;
        self.first_picture_in_bitstream = true;
        self.first_picture_after_eos = true;
        self.irap_no_rasl_output_flag = false;
        self.rps = RpsRefs::default();
    }

    fn drain_dpb(&mut self) -> Vec<Decoded> {
        let mut out = Vec::new();
        for e in self.dpb.drain() {
            if let Some(d) = self.pending.remove(&e.1) {
                out.push(d);
            }
        }
        self.dpb.clear();
        self.slot_of.clear();
        self.free = (0..self.slots as u8).rev().collect();
        out.sort_by_key(|d| d.poc);
        out
    }

    /// The short-term RPS in force for this slice (8.3.2, Note 2).
    fn st_rps<'a>(hdr_idx: u8, sps: &'a Sps, slice: &'a Slice) -> &'a ShortTermRefPicSet {
        if slice.header.curr_rps_idx == sps.num_short_term_ref_pic_sets {
            &slice.header.short_term_ref_pic_set
        } else {
            &sps.short_term_ref_pic_set[usize::from(hdr_idx)]
        }
    }

    /// 8.3.2 — derive RefPicSetStCurrBefore/After and RefPicSetLtCurr as DPB
    /// entries matched by POC, and mark DPB pictures not in any set unused.
    fn decode_rps(&mut self, slice: &Slice, sps: &Sps, cur_pic: &PictureData) -> anyhow::Result<()> {
        let hdr = &slice.header;
        self.rps = RpsRefs::default();

        if cur_pic.nalu_type.is_irap() && cur_pic.no_rasl_output_flag {
            self.dpb.mark_all_as_unused_for_ref();
        }
        if slice.nalu.header.type_.is_idr() {
            return Ok(()); // IDR: empty RPS
        }

        let curr_st_rps = Self::st_rps(hdr.curr_rps_idx, sps, slice);
        // Split short-term refs into used-by-current (curr) and not (foll). The
        // `foll` refs are NOT used by this picture but MUST stay in the DPB — a
        // later picture references them (8.3.2). Dropping them was the bug that
        // made future frames fail to find their references.
        let mut poc_st_curr_before = Vec::new();
        let mut poc_st_curr_after = Vec::new();
        let mut poc_st_foll = Vec::new();
        for i in 0..usize::from(curr_st_rps.num_negative_pics) {
            let poc = cur_pic.pic_order_cnt_val + curr_st_rps.delta_poc_s0[i];
            if curr_st_rps.used_by_curr_pic_s0[i] {
                poc_st_curr_before.push(poc);
            } else {
                poc_st_foll.push(poc);
            }
        }
        for i in 0..usize::from(curr_st_rps.num_positive_pics) {
            let poc = cur_pic.pic_order_cnt_val + curr_st_rps.delta_poc_s1[i];
            if curr_st_rps.used_by_curr_pic_s1[i] {
                poc_st_curr_after.push(poc);
            } else {
                poc_st_foll.push(poc);
            }
        }

        // Long-term (8.3.2), split curr / foll the same way.
        let mask = self.max_pic_order_cnt_lsb - 1;
        let mut poc_lt_curr: Vec<(i32, bool)> = Vec::new();
        let mut poc_lt_foll: Vec<(i32, bool)> = Vec::new();
        for i in 0..usize::from(hdr.num_long_term_sps + hdr.num_long_term_pics) {
            let mut poc_lt = hdr.poc_lsb_lt[i] as i32;
            let msb_present = hdr.delta_poc_msb_present_flag[i];
            if msb_present {
                poc_lt += cur_pic.pic_order_cnt_val;
                poc_lt -= hdr.delta_poc_msb_cycle_lt[i] as i32 * self.max_pic_order_cnt_lsb;
                poc_lt -= cur_pic.pic_order_cnt_val & mask;
            }
            if hdr.used_by_curr_pic_lt[i] {
                poc_lt_curr.push((poc_lt, msb_present));
            } else {
                poc_lt_foll.push((poc_lt, msb_present));
            }
        }

        // Match POCs to DPB entries. `keep` = every referenced picture (curr AND
        // foll); pictures in no set are marked unused-for-reference.
        let mut keep: HashSet<i32> = HashSet::new();
        for poc in poc_st_curr_before {
            if let Some(r) = self.dpb.find_short_term_ref_by_poc(poc) {
                keep.insert(r.0.borrow().pic_order_cnt_val);
                self.rps.st_curr_before.push(r);
            } else {
                log::debug!("hevc: unavailable short-term ref poc {poc} (handled by accelerator)");
            }
        }
        for poc in poc_st_curr_after {
            if let Some(r) = self.dpb.find_short_term_ref_by_poc(poc) {
                keep.insert(r.0.borrow().pic_order_cnt_val);
                self.rps.st_curr_after.push(r);
            } else {
                log::debug!("hevc: unavailable short-term ref poc {poc} (handled by accelerator)");
            }
        }
        for poc in poc_st_foll {
            if let Some(r) = self.dpb.find_short_term_ref_by_poc(poc) {
                keep.insert(r.0.borrow().pic_order_cnt_val);
            }
        }
        let find_lt = |dpb: &Dpb<u64>, poc: i32, msb: bool| {
            if msb {
                dpb.find_ref_by_poc(poc)
            } else {
                dpb.find_ref_by_poc_masked(poc, mask)
            }
        };
        for (poc, msb) in poc_lt_curr {
            if let Some(r) = find_lt(&self.dpb, poc, msb) {
                r.0.borrow_mut().set_reference(Reference::LongTerm);
                keep.insert(r.0.borrow().pic_order_cnt_val);
                self.rps.lt_curr.push(r);
            } else {
                log::debug!("hevc: unavailable long-term ref poc {poc} (handled by accelerator)");
            }
        }
        for (poc, msb) in poc_lt_foll {
            if let Some(r) = find_lt(&self.dpb, poc, msb) {
                r.0.borrow_mut().set_reference(Reference::LongTerm);
                keep.insert(r.0.borrow().pic_order_cnt_val);
            }
        }

        for e in self.dpb.entries() {
            let poc = e.0.borrow().pic_order_cnt_val;
            if !keep.contains(&poc) {
                e.0.borrow_mut().set_reference(Reference::None);
            }
        }
        Ok(())
    }

    /// C.5.2.2 — before decoding the current picture, output/evict pictures to
    /// make room. Returns any pictures bumped to display order.
    fn update_dpb_before_decoding(&mut self, sps: &Sps, cur_pic: &PictureData) -> Vec<Decoded> {
        let mut out = Vec::new();
        if cur_pic.nalu_type.is_irap()
            && cur_pic.no_rasl_output_flag
            && !self.first_picture_after_eos
        {
            if cur_pic.no_output_of_prior_pics_flag {
                self.dpb.clear();
                self.pending.clear();
                self.slot_of.clear();
                self.free = (0..self.slots as u8).rev().collect();
            } else {
                out.extend(self.drain_dpb());
            }
        } else {
            self.dpb.remove_unused();
            while self.dpb.needs_bumping(sps) {
                match self.dpb.bump(false) {
                    Some(e) => {
                        if let Some(d) = self.pending.remove(&e.1) {
                            out.push(d);
                        }
                    }
                    None => break,
                }
            }
        }
        out
    }

    fn build_pic_params(
        &self,
        sps: &Sps,
        pps: &Pps,
        cur_pic: &PictureData,
        out_slice: u8,
        ref_list: &[(u8, i32); 15],
        n_refs: usize,
    ) -> DXVA_PicParams_HEVC {
        let mut pp: DXVA_PicParams_HEVC = unsafe { zeroed() };
        pp.PicWidthInMinCbsY =
            sps.pic_width_in_luma_samples / (1u16 << (sps.log2_min_luma_coding_block_size_minus3 + 3));
        pp.PicHeightInMinCbsY = sps.pic_height_in_luma_samples
            / (1u16 << (sps.log2_min_luma_coding_block_size_minus3 + 3));

        pp.wFormatAndSequenceInfoFlags = format_seq_flags(
            sps.chroma_format_idc as u16,
            sps.separate_colour_plane_flag as u16,
            sps.bit_depth_luma_minus8 as u16,
            sps.bit_depth_chroma_minus8 as u16,
            sps.log2_max_pic_order_cnt_lsb_minus4 as u16,
            0,
            0,
        );
        pp.CurrPic = out_slice & 0x7F;
        pp.sps_max_dec_pic_buffering_minus1 =
            sps.max_dec_pic_buffering_minus1[usize::from(sps.max_sub_layers_minus1)];
        pp.log2_min_luma_coding_block_size_minus3 = sps.log2_min_luma_coding_block_size_minus3;
        pp.log2_diff_max_min_luma_coding_block_size = sps.log2_diff_max_min_luma_coding_block_size;
        pp.log2_min_transform_block_size_minus2 = sps.log2_min_luma_transform_block_size_minus2;
        pp.log2_diff_max_min_transform_block_size = sps.log2_diff_max_min_luma_transform_block_size;
        pp.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter;
        pp.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra;
        pp.num_short_term_ref_pic_sets = sps.num_short_term_ref_pic_sets;
        pp.num_long_term_ref_pics_sps = sps.num_long_term_ref_pics_sps;
        pp.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
        pp.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
        pp.init_qp_minus26 = pps.init_qp_minus26;
        pp.ucNumDeltaPocsOfRefRpsIdx = 0;
        pp.wNumBitsForShortTermRPSInSlice = cur_pic.short_term_ref_pic_set_size_bits as u16;

        pp.dwCodingParamToolFlags = coding_param_tool_flags(
            sps.scaling_list_enabled_flag as u32,
            sps.amp_enabled_flag as u32,
            sps.sample_adaptive_offset_enabled_flag as u32,
            sps.pcm_enabled_flag as u32,
            sps.pcm_sample_bit_depth_luma_minus1 as u32,
            sps.pcm_sample_bit_depth_chroma_minus1 as u32,
            sps.log2_min_pcm_luma_coding_block_size_minus3 as u32,
            sps.log2_diff_max_min_pcm_luma_coding_block_size as u32,
            sps.pcm_loop_filter_disabled_flag as u32,
            sps.long_term_ref_pics_present_flag as u32,
            sps.temporal_mvp_enabled_flag as u32,
            sps.strong_intra_smoothing_enabled_flag as u32,
            pps.dependent_slice_segments_enabled_flag as u32,
            pps.output_flag_present_flag as u32,
            pps.num_extra_slice_header_bits as u32,
            pps.sign_data_hiding_enabled_flag as u32,
            pps.cabac_init_present_flag as u32,
        );
        pp.dwCodingSettingPicturePropertyFlags = coding_setting_flags(
            pps.constrained_intra_pred_flag as u32,
            pps.transform_skip_enabled_flag as u32,
            pps.cu_qp_delta_enabled_flag as u32,
            pps.slice_chroma_qp_offsets_present_flag as u32,
            pps.weighted_pred_flag as u32,
            pps.weighted_bipred_flag as u32,
            pps.transquant_bypass_enabled_flag as u32,
            pps.tiles_enabled_flag as u32,
            pps.entropy_coding_sync_enabled_flag as u32,
            pps.uniform_spacing_flag as u32,
            pps.loop_filter_across_tiles_enabled_flag as u32,
            pps.loop_filter_across_slices_enabled_flag as u32,
            pps.deblocking_filter_override_enabled_flag as u32,
            pps.deblocking_filter_disabled_flag as u32,
            pps.lists_modification_present_flag as u32,
            pps.slice_segment_header_extension_present_flag as u32,
            cur_pic.nalu_type.is_irap() as u32,
            cur_pic.nalu_type.is_idr() as u32,
            (n_refs == 0) as u32, // IntraPicFlag: no references => intra
        );
        pp.pps_cb_qp_offset = pps.cb_qp_offset;
        pp.pps_cr_qp_offset = pps.cr_qp_offset;
        pp.num_tile_columns_minus1 = pps.num_tile_columns_minus1;
        pp.num_tile_rows_minus1 = pps.num_tile_rows_minus1;
        for (i, v) in pps.column_width_minus1.iter().take(19).enumerate() {
            pp.column_width_minus1[i] = *v as u16;
        }
        for (i, v) in pps.row_height_minus1.iter().take(21).enumerate() {
            pp.row_height_minus1[i] = *v as u16;
        }
        pp.diff_cu_qp_delta_depth = pps.diff_cu_qp_delta_depth;
        pp.pps_beta_offset_div2 = pps.beta_offset_div2;
        pp.pps_tc_offset_div2 = pps.tc_offset_div2;
        pp.log2_parallel_merge_level_minus2 = pps.log2_parallel_merge_level_minus2;
        pp.CurrPicOrderCntVal = cur_pic.pic_order_cnt_val;

        // RefPicList[15] = the current DPB references (slot | long-term flag), with
        // their POC in PicOrderCntValList; the driver builds RefPicListL0/L1 itself.
        pp.RefPicList = [HEVC_INVALID_ENTRY; 15];
        pp.PicOrderCntValList = [0; 15];
        for i in 0..n_refs.min(15) {
            pp.RefPicList[i] = ref_list[i].0 & 0x7F;
            pp.PicOrderCntValList[i] = ref_list[i].1;
        }

        // RefPicSetStCurrBefore/After/LtCurr[8] = indices into RefPicList of the
        // pictures in each set (matched by POC).
        pp.RefPicSetStCurrBefore = [HEVC_INVALID_ENTRY; 8];
        pp.RefPicSetStCurrAfter = [HEVC_INVALID_ENTRY; 8];
        pp.RefPicSetLtCurr = [HEVC_INVALID_ENTRY; 8];
        let idx_of = |poc: i32| -> u8 {
            (0..n_refs.min(15))
                .find(|&i| ref_list[i].1 == poc)
                .map(|i| i as u8)
                .unwrap_or(HEVC_INVALID_ENTRY)
        };
        for (i, e) in self.rps.st_curr_before.iter().take(8).enumerate() {
            pp.RefPicSetStCurrBefore[i] = idx_of(e.0.borrow().pic_order_cnt_val);
        }
        for (i, e) in self.rps.st_curr_after.iter().take(8).enumerate() {
            pp.RefPicSetStCurrAfter[i] = idx_of(e.0.borrow().pic_order_cnt_val);
        }
        for (i, e) in self.rps.lt_curr.iter().take(8).enumerate() {
            pp.RefPicSetLtCurr[i] = idx_of(e.0.borrow().pic_order_cnt_val);
        }

        pp.StatusReportFeedbackNumber = self.feedback;
        pp
    }

    fn decode_picture(
        &mut self,
        mut pic: PictureData,
        sps: &Sps,
        pps: &Pps,
        slice: &Slice,
        pts: i64,
        slice_nals: &[Vec<u8>],
    ) -> anyhow::Result<Vec<Decoded>> {
        unsafe {
            let mut out: Vec<Decoded> = Vec::new();

            // 8.3.2 RPS, then C.5.2.2 make room in the DPB before decoding.
            self.decode_rps(slice, sps, &pic)?;
            out.extend(self.update_dpb_before_decoding(sps, &pic));

            // Snapshot the current DPB references for RefPicList (slot + POC).
            let mut ref_list = [(0u8, 0i32); 15];
            let mut n_refs = 0usize;
            for e in self.dpb.get_all_references() {
                if n_refs >= 15 {
                    break;
                }
                let poc = e.0.borrow().pic_order_cnt_val;
                let slot = match self.slot_of.get(&e.1) {
                    Some(&s) => s,
                    None => continue,
                };
                let long_term = matches!(*e.0.borrow().reference(), Reference::LongTerm);
                ref_list[n_refs] = (slot | if long_term { 0x80 } else { 0 }, poc);
                n_refs += 1;
            }

            let out_slice = self.free.pop().ok_or_else(|| anyhow::anyhow!("pool exhausted"))?;
            self.feedback = self.feedback.wrapping_add(1).max(1);
            let poc = pic.pic_order_cnt_val;

            let pp = self.build_pic_params(sps, pps, &pic, out_slice, &ref_list, n_refs);
            let qm = DXVA_Qmatrix_HEVC::flat();

            // Bitstream: fixed 3-byte start code per slice NAL + short slice control.
            let mut bitstream: Vec<u8> = Vec::new();
            let mut slice_ctl: Vec<DXVA_Slice_HEVC_Short> = Vec::new();
            for nal in slice_nals {
                let loc = bitstream.len() as u32;
                bitstream.extend_from_slice(&START_CODE);
                bitstream.extend_from_slice(nal);
                slice_ctl.push(DXVA_Slice_HEVC_Short {
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
                slice_ctl.len() * size_of::<DXVA_Slice_HEVC_Short>(),
            );

            self.vcontext.DecoderBeginFrame(&self.decoder, &self.views[out_slice as usize], 0, None)?;
            self.put(D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, as_bytes(&pp))?;
            self.put(D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX, as_bytes(&qm))?;
            let bs_padded = self.put_bitstream(&bitstream)?;
            self.put(D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL, sc_bytes)?;
            let descs = [
                buf_desc(D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, size_of::<DXVA_PicParams_HEVC>()),
                buf_desc(D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX, size_of::<DXVA_Qmatrix_HEVC>()),
                buf_desc(D3D11_VIDEO_DECODER_BUFFER_BITSTREAM, bs_padded),
                buf_desc(D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL, sc_bytes.len()),
            ];
            self.vcontext.SubmitDecoderBuffers(&self.decoder, &descs)?;
            self.vcontext.DecoderEndFrame(&self.decoder)?;

            let i420 = self.readback_i420(out_slice)?;
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            self.pending.insert(
                id,
                Decoded { poc, pts, width: self.width, height: self.height, i420 },
            );

            // This picture is a reference until the RPS retires it. HEVC marks the
            // current picture short-term (it can be referenced by later pictures).
            pic.set_reference(Reference::ShortTerm);
            self.slot_of.insert(id, out_slice);
            self.dpb
                .store_picture(Rc::new(RefCell::new(pic)), id)
                .map_err(|e| anyhow::anyhow!("dpb store: {e}"))?;

            // Bump display-ready pictures (C.5.2.2 additional bumping).
            while self.dpb.needs_additional_bumping(sps) {
                match self.dpb.bump(false) {
                    Some(e) => {
                        if let Some(d) = self.pending.remove(&e.1) {
                            out.push(d);
                        }
                    }
                    None => break,
                }
            }

            // prevTid0Pic (8.3.1).
            self.first_picture_in_bitstream = false;
            self.first_picture_after_eos = false;

            // Reconcile the pool: a slot is live iff a still-referenced DPB entry
            // (or a pending, not-yet-bumped picture) holds it.
            let live_ids: HashSet<u64> = self.dpb.entries().iter().map(|e| e.1).collect();
            self.slot_of.retain(|id, _| live_ids.contains(id));
            let live_slots: HashSet<u8> = self.slot_of.values().copied().collect();
            self.free = (0..self.slots as u8).filter(|s| !live_slots.contains(s)).collect();

            Ok(out)
        }
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
        self.vcontext.GetDecoderBuffer(
            &self.decoder,
            D3D11_VIDEO_DECODER_BUFFER_BITSTREAM,
            &mut size,
            &mut ptr,
        )?;
        anyhow::ensure!(size as usize >= padded, "bitstream buffer too small");
        std::ptr::copy_nonoverlapping(nal.as_ptr(), ptr as *mut u8, nal.len());
        if padded > nal.len() {
            std::ptr::write_bytes((ptr as *mut u8).add(nal.len()), 0, padded - nal.len());
        }
        self.vcontext
            .ReleaseDecoderBuffer(&self.decoder, D3D11_VIDEO_DECODER_BUFFER_BITSTREAM)?;
        Ok(padded)
    }

    unsafe fn readback_i420(&self, slice: u8) -> anyhow::Result<Vec<u8>> {
        self.context
            .CopySubresourceRegion(&self.staging, 0, 0, 0, 0, &self.pool, slice as u32, None);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        let out = pack_nv12_i420(m.pData as *const u8, m.RowPitch as usize, self.width, self.height);
        self.context.Unmap(&self.staging, 0);
        Ok(out)
    }
}

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

fn buf_desc(ty: D3D11_VIDEO_DECODER_BUFFER_TYPE, size: usize) -> D3D11_VIDEO_DECODER_BUFFER_DESC {
    let mut d: D3D11_VIDEO_DECODER_BUFFER_DESC = unsafe { zeroed() };
    d.BufferType = ty;
    d.DataSize = size as u32;
    d
}

fn as_bytes<T>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, size_of::<T>()) }
}
