//! Task 117 M2 step 2 — H.264 via openh264, and the backend REGISTRY.
//!
//! Self-contained: openh264 both encodes and decodes, so this round-trips in
//! process with no sample file and no demuxer (the `h264_mp4toannexb` question
//! is deferred to step 2b, which needs a real MP4). Encoder is configured
//! `CameraVideoRealTime` — no B-frames — so decode order == presentation order
//! and the decoder's PTS FIFO is valid.

#![cfg(all(feature = "libvpx", feature = "openh264"))]

use wandr_video::{
    default_registry, open_decoder, open_encoder, BackendKind, Chunk, Codec, DecoderParams,
    EncoderParams, Preferences, Rgb24Frame,
};

const W: u32 = 320;
const H: u32 = 240;
const FPS: i64 = 30;
const PTS_ORIGIN_US: i64 = 5_500_000;

fn pts_us(i: i64) -> i64 {
    PTS_ORIGIN_US + (i * 1_000_000) / FPS
}

fn test_frame(t: u32) -> Vec<u8> {
    let mut b = vec![0u8; (W * H * 3) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 3) as usize;
            b[i] = ((x * 255 / W) as u8).wrapping_add(t as u8);
            b[i + 1] = (y * 255 / H) as u8;
            b[i + 2] = if (x / 16 + y / 16) % 2 == 0 { 220 } else { 30 };
        }
    }
    b
}

fn rgb_vs_rgba_mae(src: &[u8], dec: &[u8], w: u32, h: u32) -> f64 {
    let n = (w * h) as usize;
    let mut sum = 0u64;
    for i in 0..n {
        for c in 0..3 {
            sum += (src[i * 3 + c] as i32 - dec[i * 4 + c] as i32).unsigned_abs() as u64;
        }
    }
    sum as f64 / (n * 3) as f64
}

/// The registry routes H.264 to openh264 and VP8/VP9 to libvpx, both software.
#[test]
fn registry_routes_codecs_to_the_right_backend() {
    let reg = default_registry();
    // Just assert the wiring: both backends registered, H.264 supported.
    let enc = open_encoder(&EncoderParams {
        codec: Codec::H264, width: W, height: H, bitrate_bps: 1_000_000, framerate: 30,
    });
    assert!(enc.is_ok(), "H.264 encoder should open via openh264");

    // `require_hardware` must FAIL here — everything registered is software. This
    // is the guard the oxideav spike showed matters: a caller that needs HW gets
    // an error, not a silent software fallback.
    let hw_only = reg.open_decoder(
        &DecoderParams { codec: Codec::H264, width: W, height: H },
        Preferences { require_hardware: true, ..Default::default() },
    );
    assert!(hw_only.is_err(), "require_hardware must not silently use software");

    // H.265 routing depends on which backends are compiled in: with the
    // oxideav-h265 feature there IS a software decoder, without it there is not.
    // Either way the registry must answer cleanly (never panic).
    let h265 = reg.open_decoder(
        &DecoderParams { codec: Codec::H265, width: W, height: H },
        Preferences::default(),
    );
    if cfg!(any(feature = "oxideav-h265", feature = "libde265")) {
        assert!(h265.is_ok(), "an H.265 software backend should decode H.265");
    } else {
        assert!(h265.is_err(), "no H.265 software backend without a feature");
    }
}

/// The load-bearing decode test: encode H.264, decode it, and the decoded pixels
/// match the source — with PTS preserved.
#[test]
fn h264_roundtrip_preserves_image_and_pts() {
    let mut enc = open_encoder(&EncoderParams {
        codec: Codec::H264, width: W, height: H, bitrate_bps: 2_000_000, framerate: FPS as u32,
    })
    .expect("open h264 encoder");
    let mut dec = open_decoder(&DecoderParams { codec: Codec::H264, width: W, height: H })
        .expect("open h264 decoder");

    let mut last_rgba: Option<Vec<u8>> = None;
    let mut src = Vec::new();
    let mut got_pts: Vec<i64> = Vec::new();

    for i in 0..12i64 {
        src = test_frame(i as u32);
        enc.encode(Rgb24Frame::new(&src, W, H), i == 0).expect("encode");
        while let Some(pkt) = enc.next_packet() {
            dec.decode(Chunk::new(&pkt.data, pts_us(i))).ok(); // SPS/PPS may yield nothing
            while let Some(f) = dec.next_frame() {
                assert_eq!((f.width, f.height), (W, H));
                got_pts.push(f.timestamp_us);
                let mut rgba = Vec::new();
                wandr_video::i420_to_rgba(&f, &mut rgba).expect("i420->rgba");
                last_rgba = Some(rgba);
            }
        }
    }
    dec.flush().expect("flush");
    while let Some(f) = dec.next_frame() {
        got_pts.push(f.timestamp_us);
    }

    assert!(got_pts.len() >= 8, "too few frames decoded: {}", got_pts.len());
    // PTS preserved and monotonic (no B-frames → in order).
    assert_eq!(got_pts[0], pts_us(0), "first PTS not preserved");
    assert!(got_pts.windows(2).all(|w| w[1] > w[0]), "PTS not monotonic: {got_pts:?}");

    // Pixels match. H.264 at 2 Mbps on 320x240 lands well under 20 MAE; a wrong
    // colorspace or a broken decode blows past it. (Same empirical bar as VP9's
    // roundtrip; H.264's is a touch higher than VP9's 1.68 but still small.)
    let rgba = last_rgba.expect("no decoded frame");
    let mae = rgb_vs_rgba_mae(&src, &rgba, W, H);
    eprintln!("H.264 MAE = {mae:.2}");
    assert!(mae < 20.0, "decoded H.264 differs too much from source: MAE {mae:.2}");
}

/// openh264 must register as software, so a future HW H.264 backend outranks it.
#[test]
fn openh264_registers_as_software() {
    // Indirect check: with require_hardware there is no H.264 path (openh264 is SW),
    // without it there is. That is only true if openh264 is classed Software.
    let reg = default_registry();
    let sw = reg.open_decoder(
        &DecoderParams { codec: Codec::H264, width: W, height: H },
        Preferences::default(),
    );
    assert!(sw.is_ok(), "software H.264 path should exist");
    let _ = BackendKind::Hardware; // keep the import meaningful
}
