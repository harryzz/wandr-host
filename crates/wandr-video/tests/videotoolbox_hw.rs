//! Hardware H.264 + HEVC decode via the VideoToolbox backend, end-to-end through
//! the public `Decoder` trait. SKIPS (passes) where VideoToolbox has no HW decoder
//! for the codec, so it is safe on a runner without one; it only asserts on a
//! machine that actually decodes. Bit-exactness is checked against the ffmpeg
//! framehash reference (NV12 CRC32) that ships beside the cros-codecs test vectors.
#![cfg(all(feature = "videotoolbox", target_os = "macos"))]

use wandr_video::{Chunk, Codec, DecoderParams, Preferences};

static H264_CLIP: &[u8] =
    include_bytes!("../../../vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264");
static H264_CRC: &str =
    include_str!("../../../vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264.crc");
static H265_CLIP: &[u8] =
    include_bytes!("../../../vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265");
static H265_CRC: &str =
    include_str!("../../../vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265.crc");

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Split an Annex-B elementary stream into access units (one coded picture each,
/// with its preceding parameter sets / SEI). A new AU begins at a VCL slice that is
/// the first slice of a picture. H.264: NAL header 1 byte, VCL type 1/5,
/// first_mb_in_slice==0 is the top bit after the header. HEVC: NAL header 2 bytes,
/// VCL type 0..=31, first_slice_segment_in_pic_flag is the top bit after it.
fn split_access_units(data: &[u8], hevc: bool) -> Vec<Vec<u8>> {
    let mut sc = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            sc.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    sc.push(data.len());

    let mut aus: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut cur_has_vcl = false;
    for w in sc.windows(2) {
        let nal = &data[w[0]..w[1]];
        let (is_vcl, first_slice) = if hevc {
            let t = (nal[3] >> 1) & 0x3f;
            let is_vcl = t <= 31;
            (is_vcl, is_vcl && nal.len() > 5 && (nal[5] & 0x80) != 0)
        } else {
            let t = nal[3] & 0x1f;
            let is_vcl = t == 1 || t == 5;
            (is_vcl, is_vcl && nal.len() > 4 && (nal[4] & 0x80) != 0)
        };
        if is_vcl && first_slice && cur_has_vcl {
            aus.push(std::mem::take(&mut cur));
            cur_has_vcl = false;
        }
        cur.extend_from_slice(nal);
        if is_vcl {
            cur_has_vcl = true;
        }
    }
    if !cur.is_empty() {
        aus.push(cur);
    }
    aus
}

/// Repack tightly-packed I420 to NV12 so it matches the ffmpeg `-pix_fmt nv12` CRC.
fn i420_to_nv12_crc(y: &[u8], u: &[u8], v: &[u8], w: usize, h: usize) -> u32 {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut nv12 = Vec::with_capacity(w * h + 2 * cw * ch);
    nv12.extend_from_slice(&y[..w * h]);
    for i in 0..cw * ch {
        nv12.push(u[i]);
        nv12.push(v[i]);
    }
    crc32(&nv12)
}

/// Decode the clip through the trait and return the NV12 CRC of every frame.
fn decode_crcs(codec: Codec, clip: &[u8], hevc: bool) -> Option<Vec<u32>> {
    let params = DecoderParams { codec, width: 320, height: 240 };
    let mut dec = match wandr_video::open_decoder_with(&params, Preferences::default()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP: no {codec:?} HW decoder available ({e:?})");
            return None;
        }
    };
    let aus = split_access_units(clip, hevc);
    let mut got: Vec<u32> = Vec::new();
    let mut scratch = Vec::new();
    let drain = |dec: &mut Box<dyn wandr_video::Decoder>, got: &mut Vec<u32>, scratch: &mut Vec<u8>| {
        while let Some(frame) = dec.next_frame() {
            let (w, h) = frame.dimensions();
            let r = frame.read_i420(scratch).expect("read_i420");
            let (cw, ch) = ((w as usize).div_ceil(2), (h as usize).div_ceil(2));
            let yl = (w * h) as usize;
            let cl = cw * ch;
            got.push(i420_to_nv12_crc(&r.y[..yl], &r.u[..cl], &r.v[..cl], w as usize, h as usize));
        }
    };
    for (i, au) in aus.iter().enumerate() {
        dec.decode(Chunk::new(au, (i as i64) * 40_000)).expect("decode");
        drain(&mut dec, &mut got, &mut scratch);
    }
    dec.flush().expect("flush");
    drain(&mut dec, &mut got, &mut scratch);
    Some(got)
}

/// VideoToolbox emits in DECODE order (it does not reorder to display order — the
/// host does that by sorting on each frame's PTS, video_desktop::queue_decoded).
/// The elementary stream has no container PTS, so the harness feeds a decode-order
/// timestamp and cannot assert display ORDER here. It asserts that every frame
/// decoded BIT-EXACT — a multiset match against the display-order reference — which
/// is the decode-correctness proof. Ordering is proven by the on-screen playback
/// path, where the demuxer supplies real PTS (finding #6).
fn assert_bit_exact(name: &str, got: Vec<u32>, crc_ref: &str) {
    let mut want: Vec<u32> =
        crc_ref.split_whitespace().map(|w| u32::from_str_radix(w, 16).unwrap()).collect();
    assert_eq!(got.len(), want.len(), "{name}: frame count");
    let mut got = got;
    got.sort_unstable();
    want.sort_unstable();
    let mismatches = got.iter().zip(&want).filter(|(g, w)| g != w).count();
    assert_eq!(mismatches, 0, "{name}: {mismatches}/{} frames not bit-exact (as a set)", want.len());
    eprintln!("videotoolbox {name}: all {} frames decode bit-exact", got.len());
}

#[test]
fn videotoolbox_decodes_h264_bit_exact() {
    let Some(got) = decode_crcs(Codec::H264, H264_CLIP, false) else { return };
    assert_bit_exact("h264", got, H264_CRC);
}

#[test]
fn videotoolbox_decodes_hevc_bit_exact() {
    let Some(got) = decode_crcs(Codec::H265, H265_CLIP, true) else { return };
    assert_bit_exact("hevc", got, H265_CRC);
}
