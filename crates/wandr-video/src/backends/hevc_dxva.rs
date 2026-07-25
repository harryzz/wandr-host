//! DXVA (D3D11) HEVC/H.265 decode structures — task 117 M2 Windows HW HEVC.
//!
//! The hand-defined DXVA HEVC picture-params / qmatrix / slice structs (dxva.h is
//! `#pragma pack(1)`; these match its layout byte-for-byte). Filled from
//! cros-codecs' HEVC parser (`codec::h265`), which also runs the HEVC decode
//! process (POC, RefPicSet, DPB bumping) — so this backend only maps parsed
//! syntax → DXVA buffers, mirroring the VA-API HEVC backend
//! (`decoder/stateless/h265/vaapi.rs`) and the existing DXVA **H.264** backend
//! (`backends/d3d11.rs`).
//!
//! ‼️ The three bitfield WORDS (`wFormatAndSequenceInfoFlags`,
//! `dwCodingParamToolFlags`, `dwCodingSettingPicturePropertyFlags`) are packed by
//! hand (LSB-first, matching the C bitfield order on x86) exactly like the H.264
//! `wBitFields` — a wrong bit is silent wrong output, so each is built explicitly.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use windows::core::GUID;

// D3D11_DECODER_PROFILE_HEVC_VLD_MAIN   = {5b11d51b-2f4c-4452-bcc3-09f2a1160cc0}
// D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10 = {107af0e0-ef1a-4d19-aba8-67a163073d13}
pub const HEVC_VLD_MAIN: GUID = GUID::from_u128(0x5b11d51b_2f4c_4452_bcc3_09f2a1160cc0);
pub const HEVC_VLD_MAIN10: GUID = GUID::from_u128(0x107af0e0_ef1a_4d19_aba8_67a163073d13);

/// `DXVA_PicEntry_HEVC` = 8 bits: `{ Index7Bits:7, AssociatedFlag:1 }`; 0xFF = invalid.
pub type DXVA_PicEntry_HEVC = u8;
pub const HEVC_INVALID_ENTRY: u8 = 0xFF;
/// `RefPicList` is a fixed 15-entry array — the HEVC DXVA reference ceiling
/// (vs H.264's 16). `RefPicSetStCurrBefore/After/LtCurr` are 8 each.
pub const HEVC_MAX_REFS: usize = 15;

/// `DXVA_PicParams_HEVC` (dxva.h). `#[repr(C)]` with the exact field order; the
/// three C bitfield unions are represented as their plain integer alias
/// (`wFormatAndSequenceInfoFlags` / `dwCodingParamToolFlags` /
/// `dwCodingSettingPicturePropertyFlags`) and packed by the builder below.
///
/// No internal padding at natural alignment (u16/u32/arrays stay aligned), so
/// `repr(C)` already matches the `#pragma pack(1)` header — same reasoning as
/// `DXVA_PicParams_H264`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DXVA_PicParams_HEVC {
    pub PicWidthInMinCbsY: u16,
    pub PicHeightInMinCbsY: u16,
    /// union { bitfields } / `wFormatAndSequenceInfoFlags`
    pub wFormatAndSequenceInfoFlags: u16,
    pub CurrPic: DXVA_PicEntry_HEVC,
    pub sps_max_dec_pic_buffering_minus1: u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub num_short_term_ref_pic_sets: u8,
    pub num_long_term_ref_pics_sps: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i8,
    pub ucNumDeltaPocsOfRefRpsIdx: u8,
    pub wNumBitsForShortTermRPSInSlice: u16,
    pub ReservedBits2: u16,
    /// union { bitfields } / `dwCodingParamToolFlags`
    pub dwCodingParamToolFlags: u32,
    /// union { bitfields } / `dwCodingSettingPicturePropertyFlags`
    pub dwCodingSettingPicturePropertyFlags: u32,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u16; 19],
    pub row_height_minus1: [u16; 21],
    pub diff_cu_qp_delta_depth: u8,
    pub pps_beta_offset_div2: i8,
    pub pps_tc_offset_div2: i8,
    pub log2_parallel_merge_level_minus2: u8,
    pub CurrPicOrderCntVal: i32,
    pub RefPicList: [DXVA_PicEntry_HEVC; 15],
    pub ReservedBits5: u8,
    pub PicOrderCntValList: [i32; 15],
    pub RefPicSetStCurrBefore: [u8; 8],
    pub RefPicSetStCurrAfter: [u8; 8],
    pub RefPicSetLtCurr: [u8; 8],
    pub ReservedBits6: u16,
    pub ReservedBits7: u16,
    pub StatusReportFeedbackNumber: u32,
}

/// `DXVA_Qmatrix_HEVC` (dxva.h) — the HEVC scaling lists. Default (flat) values
/// are 16 everywhere and the DC coefficients are 16 when no list is signalled.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DXVA_Qmatrix_HEVC {
    pub ucScalingLists0: [[u8; 16]; 6],
    pub ucScalingLists1: [[u8; 64]; 6],
    pub ucScalingLists2: [[u8; 64]; 6],
    pub ucScalingLists3: [[u8; 64]; 2],
    pub ucScalingListDCCoefSizeID2: [u8; 6],
    pub ucScalingListDCCoefSizeID3: [u8; 2],
}

impl DXVA_Qmatrix_HEVC {
    /// Flat default scaling lists (all 16) — correct when `scaling_list_enabled_flag`
    /// is 0 (no scaling data), which is the common case (incl. bbb-h265).
    pub fn flat() -> Self {
        Self {
            ucScalingLists0: [[16; 16]; 6],
            ucScalingLists1: [[16; 64]; 6],
            ucScalingLists2: [[16; 64]; 6],
            ucScalingLists3: [[16; 64]; 2],
            ucScalingListDCCoefSizeID2: [16; 6],
            ucScalingListDCCoefSizeID3: [16; 2],
        }
    }
}

/// `DXVA_Slice_HEVC_Short` (dxva.h) — byte-identical to `DXVA_Slice_H264_Short`
/// (10 bytes, packed so a multi-slice array has the right stride).
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct DXVA_Slice_HEVC_Short {
    pub BSNALunitDataLocation: u32,
    pub SliceBytesInBuffer: u32,
    pub wBadSliceChopping: u16,
}

// ── bitfield packers (LSB-first, matching the C bitfield layout) ─────────────

/// Build `wFormatAndSequenceInfoFlags` (a `USHORT` union of 16 bits, LSB-first).
#[allow(clippy::too_many_arguments)]
pub fn format_seq_flags(
    chroma_format_idc: u16,
    separate_colour_plane_flag: u16,
    bit_depth_luma_minus8: u16,
    bit_depth_chroma_minus8: u16,
    log2_max_pic_order_cnt_lsb_minus4: u16,
    no_pic_reordering_flag: u16,
    no_bi_pred_flag: u16,
) -> u16 {
    let mut w: u16 = 0;
    w |= (chroma_format_idc & 0x3) << 0; // :2
    w |= (separate_colour_plane_flag & 0x1) << 2; // :1
    w |= (bit_depth_luma_minus8 & 0x7) << 3; // :3
    w |= (bit_depth_chroma_minus8 & 0x7) << 6; // :3
    w |= (log2_max_pic_order_cnt_lsb_minus4 & 0xF) << 9; // :4
    w |= (no_pic_reordering_flag & 0x1) << 13; // :1
    w |= (no_bi_pred_flag & 0x1) << 14; // :1
    // ReservedBits1 :1 at bit 15
    w
}

/// Build `dwCodingParamToolFlags` (a `UINT32` union, LSB-first).
#[allow(clippy::too_many_arguments)]
pub fn coding_param_tool_flags(
    scaling_list_enabled_flag: u32,
    amp_enabled_flag: u32,
    sample_adaptive_offset_enabled_flag: u32,
    pcm_enabled_flag: u32,
    pcm_sample_bit_depth_luma_minus1: u32,
    pcm_sample_bit_depth_chroma_minus1: u32,
    log2_min_pcm_luma_coding_block_size_minus3: u32,
    log2_diff_max_min_pcm_luma_coding_block_size: u32,
    pcm_loop_filter_disabled_flag: u32,
    long_term_ref_pics_present_flag: u32,
    sps_temporal_mvp_enabled_flag: u32,
    strong_intra_smoothing_enabled_flag: u32,
    dependent_slice_segments_enabled_flag: u32,
    output_flag_present_flag: u32,
    num_extra_slice_header_bits: u32,
    sign_data_hiding_enabled_flag: u32,
    cabac_init_present_flag: u32,
) -> u32 {
    let mut d: u32 = 0;
    let mut b = 0;
    macro_rules! put {
        ($v:expr, $w:expr) => {{
            d |= ($v & ((1u32 << $w) - 1)) << b;
            b += $w;
        }};
    }
    put!(scaling_list_enabled_flag, 1);
    put!(amp_enabled_flag, 1);
    put!(sample_adaptive_offset_enabled_flag, 1);
    put!(pcm_enabled_flag, 1);
    put!(pcm_sample_bit_depth_luma_minus1, 4);
    put!(pcm_sample_bit_depth_chroma_minus1, 4);
    put!(log2_min_pcm_luma_coding_block_size_minus3, 2);
    put!(log2_diff_max_min_pcm_luma_coding_block_size, 2);
    put!(pcm_loop_filter_disabled_flag, 1);
    put!(long_term_ref_pics_present_flag, 1);
    put!(sps_temporal_mvp_enabled_flag, 1);
    put!(strong_intra_smoothing_enabled_flag, 1);
    put!(dependent_slice_segments_enabled_flag, 1);
    put!(output_flag_present_flag, 1);
    put!(num_extra_slice_header_bits, 3);
    put!(sign_data_hiding_enabled_flag, 1);
    put!(cabac_init_present_flag, 1);
    // ReservedBits3 :5
    d
}

/// Build `dwCodingSettingPicturePropertyFlags` (a `UINT32` union, LSB-first).
#[allow(clippy::too_many_arguments)]
pub fn coding_setting_flags(
    constrained_intra_pred_flag: u32,
    transform_skip_enabled_flag: u32,
    cu_qp_delta_enabled_flag: u32,
    pps_slice_chroma_qp_offsets_present_flag: u32,
    weighted_pred_flag: u32,
    weighted_bipred_flag: u32,
    transquant_bypass_enabled_flag: u32,
    tiles_enabled_flag: u32,
    entropy_coding_sync_enabled_flag: u32,
    uniform_spacing_flag: u32,
    loop_filter_across_tiles_enabled_flag: u32,
    pps_loop_filter_across_slices_enabled_flag: u32,
    deblocking_filter_override_enabled_flag: u32,
    pps_deblocking_filter_disabled_flag: u32,
    lists_modification_present_flag: u32,
    slice_segment_header_extension_present_flag: u32,
    irap_pic_flag: u32,
    idr_pic_flag: u32,
    intra_pic_flag: u32,
) -> u32 {
    let mut d: u32 = 0;
    let mut b = 0;
    macro_rules! put {
        ($v:expr, $w:expr) => {{
            d |= ($v & ((1u32 << $w) - 1)) << b;
            b += $w;
        }};
    }
    put!(constrained_intra_pred_flag, 1);
    put!(transform_skip_enabled_flag, 1);
    put!(cu_qp_delta_enabled_flag, 1);
    put!(pps_slice_chroma_qp_offsets_present_flag, 1);
    put!(weighted_pred_flag, 1);
    put!(weighted_bipred_flag, 1);
    put!(transquant_bypass_enabled_flag, 1);
    put!(tiles_enabled_flag, 1);
    put!(entropy_coding_sync_enabled_flag, 1);
    put!(uniform_spacing_flag, 1);
    put!(loop_filter_across_tiles_enabled_flag, 1);
    put!(pps_loop_filter_across_slices_enabled_flag, 1);
    put!(deblocking_filter_override_enabled_flag, 1);
    put!(pps_deblocking_filter_disabled_flag, 1);
    put!(lists_modification_present_flag, 1);
    put!(slice_segment_header_extension_present_flag, 1);
    put!(irap_pic_flag, 1);
    put!(idr_pic_flag, 1);
    put!(intra_pic_flag, 1);
    // ReservedBits4 :13
    d
}

// Compile-time layout guards: these sizes match dxva.h (packed). If a field type
// drifts, the build fails here rather than silently feeding the driver garbage.
const _: () = {
    assert!(core::mem::size_of::<DXVA_Slice_HEVC_Short>() == 10);
    // PicParams: no compile-time size assert (driver tolerates the documented
    // layout); the field-order + types above are the contract.
    assert!(core::mem::size_of::<DXVA_Qmatrix_HEVC>() == 6 * 16 + 6 * 64 + 6 * 64 + 2 * 64 + 6 + 2);
};
