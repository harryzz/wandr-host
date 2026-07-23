//! Hardware H.264 decode via the DXVA2/D3D11 backend, end-to-end through the
//! public `Decoder` trait. SKIPS (passes) where there is no usable video device,
//! so it is safe on a GPU-less CI runner; it only asserts on a box that actually
//! decodes. Bit-exactness is checked against the ffmpeg framehash reference that
//! ships beside the cros-codecs H.264 test vector.
#![cfg(all(feature = "d3d11", target_os = "windows"))]

use wandr_video::{Chunk, Codec, DecoderParams, Preferences};

static CLIP: &[u8] = include_bytes!(
    "../../../vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
);
static CRC: &str = include_str!(
    "../../../vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264.crc"
);

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
/// with its preceding SPS/PPS/SEI). A new AU begins at a VCL slice whose
/// first_mb_in_slice == 0 (the first ue(v) bit after the NAL header is 1).
fn split_access_units(data: &[u8]) -> Vec<Vec<u8>> {
    // start-code positions (00 00 01)
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
        let (start, end) = (w[0], w[1]);
        let nal = &data[start..end];
        let hdr = nal[3];
        let ntype = hdr & 0x1f;
        let is_vcl = ntype == 1 || ntype == 5;
        let first_mb_zero = is_vcl && (nal[4] & 0x80) != 0;
        if is_vcl && first_mb_zero && cur_has_vcl {
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

/// Repack tightly-packed I420 (Y,U,V planar) to NV12 (Y + interleaved UV) so it
/// matches the ffmpeg `-pix_fmt nv12` reference CRC.
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

#[test]
fn d3d11_decodes_test25fps_bit_exact() {
    let refs: Vec<&str> = CRC.split_whitespace().collect();

    let params = DecoderParams { codec: Codec::H264, width: 320, height: 240 };
    let mut dec = match wandr_video::open_decoder_with(&params, Preferences::default()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP: no H.264 decoder available ({e:?})");
            return;
        }
    };

    let aus = split_access_units(CLIP);
    let mut got: Vec<u32> = Vec::new();
    let mut scratch = Vec::new();
    let drain = |dec: &mut Box<dyn wandr_video::Decoder>, got: &mut Vec<u32>, scratch: &mut Vec<u8>| {
        while let Some(frame) = dec.next_frame() {
            let (w, h) = frame.dimensions();
            let r = frame.read_i420(scratch).expect("read_i420");
            let (cw, ch) = ((w as usize).div_ceil(2), (h as usize).div_ceil(2));
            let yl = (w * h) as usize;
            let cl = cw * ch;
            got.push(i420_to_nv12_crc(
                &r.y[..yl], &r.u[..cl], &r.v[..cl], w as usize, h as usize,
            ));
        }
    };

    for (i, au) in aus.iter().enumerate() {
        dec.decode(Chunk::new(au, (i as i64) * 40_000)).expect("decode");
        drain(&mut dec, &mut got, &mut scratch);
    }
    dec.flush().expect("flush");
    drain(&mut dec, &mut got, &mut scratch);

    assert_eq!(got.len(), refs.len(), "frame count");
    let mut mismatches = 0;
    for (i, (g, w)) in got.iter().zip(refs.iter()).enumerate() {
        let want = u32::from_str_radix(w, 16).unwrap();
        if *g != want {
            if mismatches < 8 {
                eprintln!("frame {i}: got {g:08x} want {w}");
            }
            mismatches += 1;
        }
    }
    assert_eq!(mismatches, 0, "{mismatches}/{} frames not bit-exact", refs.len());
    eprintln!("d3d11: all {} frames bit-exact in display order", got.len());
}
