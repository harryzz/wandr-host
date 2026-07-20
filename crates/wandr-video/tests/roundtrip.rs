//! Encode → decode round-trip on synthetic frames. No camera, no host, no Skia.
//!
//! These assert on DECODED PIXELS, not just "a packet came out". That distinction
//! is the whole point: the three ways this port can go wrong (bitrate in bits
//! instead of kilobits, a BT.601/BT.709 mixup, a limited/full range mixup) all
//! produce perfectly well-formed packets. Only comparing pixels catches them.

use wandr_video::{open_decoder, open_encoder, Codec, DecoderParams, EncoderParams, Rgb24Frame};

const W: u32 = 320;
const H: u32 = 240;

/// A deterministic image with strong colour AND luma structure — a flat grey
/// frame would pass even with a badly wrong colour matrix.
fn test_frame(w: u32, h: u32, t: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 3) as usize;
            buf[i] = ((x * 255 / w.max(1)) as u8).wrapping_add(t as u8); // R ramp
            buf[i + 1] = (y * 255 / h.max(1)) as u8; // G ramp
            buf[i + 2] = if (x / 16 + y / 16) % 2 == 0 { 220 } else { 30 }; // checker
        }
    }
    buf
}

fn enc_cfg(codec: Codec, bitrate_bps: u32) -> EncoderParams {
    EncoderParams { codec, width: W, height: H, bitrate_bps, framerate: 30 }
}

fn dec_cfg(codec: Codec) -> DecoderParams {
    DecoderParams { codec, width: W, height: H }
}

/// Mean absolute error per channel between the source RGB and a decoded RGBA.
fn rgb_vs_rgba_mae(src_rgb: &[u8], dec_rgba: &[u8], w: u32, h: u32) -> f64 {
    let mut sum = 0u64;
    let n = (w * h) as usize;
    for i in 0..n {
        for c in 0..3 {
            let a = src_rgb[i * 3 + c] as i32;
            let b = dec_rgba[i * 4 + c] as i32;
            sum += (a - b).unsigned_abs() as u64;
        }
    }
    sum as f64 / (n * 3) as f64
}

#[test]
fn vp8_roundtrip_preserves_the_image() {
    let mut enc = open_encoder(&enc_cfg(Codec::Vp8, 1_000_000)).expect("open encoder");
    let mut dec = open_decoder(&dec_cfg(Codec::Vp8)).expect("open decoder");

    let mut decoded_count = 0usize;
    let mut last_rgba: Option<Vec<u8>> = None;
    let mut src = Vec::new();

    // A few frames so the encoder leaves its first-keyframe transient.
    for t in 0..8 {
        src = test_frame(W, H, t);
        enc.encode(Rgb24Frame::new(&src, W, H), t == 0).expect("encode");
        while let Some(pkt) = enc.next_packet() {
            if t == 0 {
                assert!(pkt.keyframe, "first frame must be a keyframe");
            }
            assert!(!pkt.data.is_empty(), "empty packet");
            dec.decode(&pkt.data).expect("decode");
            while let Some(frame) = dec.next_frame() {
                assert_eq!((frame.width, frame.height), (W, H));
                let mut rgba = Vec::new();
                wandr_video::i420_to_rgba(&frame, &mut rgba).expect("i420->rgba");
                last_rgba = Some(rgba);
                decoded_count += 1;
            }
        }
    }

    assert!(decoded_count >= 4, "expected several decoded frames, got {decoded_count}");

    // The real assertion, and the threshold is EMPIRICAL — measured by injecting
    // each bug it is meant to catch, because a loose threshold makes this test
    // decorative:
    //     correct (BT.601 + Limited both ways) .... MAE 1.68
    //     decode matrix flipped to BT.709 ......... MAE 7.93
    //     decode range flipped to Full ............ MAE 9.36
    // 4.0 sits ~2.4x above the correct value and ~2x below the nearest bug. If a
    // codec/crate upgrade moves the baseline, re-measure rather than just raising
    // this number — the gap is the whole value of the test.
    let rgba = last_rgba.expect("no decoded frame");
    let mae = rgb_vs_rgba_mae(&src, &rgba, W, H);
    eprintln!("MAE = {mae:.2}");
    assert!(mae < 4.0, "decoded image differs too much from source: MAE {mae:.2}");
}

#[test]
fn vp9_roundtrip_works() {
    let mut enc = open_encoder(&enc_cfg(Codec::Vp9, 1_000_000)).expect("open vp9 encoder");
    let mut dec = open_decoder(&dec_cfg(Codec::Vp9)).expect("open vp9 decoder");

    let mut decoded = 0usize;
    for t in 0..5 {
        let src = test_frame(W, H, t);
        enc.encode(Rgb24Frame::new(&src, W, H), t == 0).expect("encode");
        while let Some(pkt) = enc.next_packet() {
            dec.decode(&pkt.data).expect("decode");
            while dec.next_frame().is_some() {
                decoded += 1;
            }
        }
    }
    assert!(decoded > 0, "vp9 produced no decoded frames");
}

/// The camera resolution routinely differs from the encode size, so the internal
/// resize path must work — and must NOT be silently skipped.
#[test]
fn encodes_when_source_resolution_differs() {
    let mut enc = open_encoder(&enc_cfg(Codec::Vp8, 800_000)).expect("open encoder");
    let mut dec = open_decoder(&dec_cfg(Codec::Vp8)).expect("open decoder");

    let (sw, sh) = (640, 480); // source larger than the 320x240 encode size
    let mut decoded = 0usize;
    for t in 0..4 {
        let src = test_frame(sw, sh, t);
        enc.encode(Rgb24Frame::new(&src, sw, sh), t == 0).expect("encode");
        while let Some(pkt) = enc.next_packet() {
            dec.decode(&pkt.data).expect("decode");
            while let Some(f) = dec.next_frame() {
                assert_eq!((f.width, f.height), (W, H), "decoded at the ENCODE size");
                decoded += 1;
            }
        }
    }
    assert!(decoded > 0, "resize path produced no frames");
}

/// `set_bitrate` was a no-op on the ffmpeg path; libvpx retunes for real.
#[test]
fn set_bitrate_is_accepted_mid_stream() {
    let mut enc = open_encoder(&enc_cfg(Codec::Vp8, 1_000_000)).expect("open encoder");
    let src = test_frame(W, H, 0);
    enc.encode(Rgb24Frame::new(&src, W, H), true).expect("encode");
    while enc.next_packet().is_some() {}
    enc.set_bitrate(300_000).expect("set_bitrate mid-stream");
    enc.encode(Rgb24Frame::new(&src, W, H), false).expect("encode after retune");
    assert!(enc.next_packet().is_some(), "no packet after bitrate change");
}

/// A truncated/garbage payload must surface as an error, not a panic or UB.
#[test]
fn rejects_garbage_payload() {
    let mut dec = open_decoder(&dec_cfg(Codec::Vp8)).expect("open decoder");
    assert!(dec.decode(&[0xde, 0xad, 0xbe, 0xef]).is_err(), "garbage should error");
}
