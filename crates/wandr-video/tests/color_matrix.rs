//! Task 117 — FRAME-EXACT colour-matrix verification.
//!
//! The earlier on-screen check compared two DIFFERENT frames captured at
//! different wall-clock offsets, so the channel deltas it reported were not
//! attributable to the matrix — they were mostly "different picture". This
//! closes that: it takes ONE decoded frame and converts it three ways, so every
//! difference is the matrix and nothing else.
//!
//! It needs no GPU. The colour matrix is applied at YUV->RGB, AFTER decode, so
//! the software decoder's I420 is identical to the hardware decoder's and the
//! comparison is the same either way — which is the whole point: the matrix is a
//! property of the conversion, not of the codec.
//!
//!   WANDR_TEST_MP4=/path/to/bbb-h264.mp4 [WANDR_DUMP_DIR=dump] \
//!     cargo test --features openh264 --test color_matrix -- --nocapture

#![cfg(feature = "openh264")]

use wandr_video::{Chunk, Codec, ColorInfo, ColorMatrix, DecoderParams, Preferences};

const START: [u8; 4] = [0, 0, 0, 1];

/// MP4 -> (Annex-B access unit, PTS µs) in decode order. Same demux the pairing
/// test uses; duplicated rather than shared because tests do not share helpers.
fn demux(bytes: &[u8]) -> Vec<(Vec<u8>, i64)> {
    let size = bytes.len() as u64;
    let mut mp4 = mp4::Mp4Reader::read_header(std::io::Cursor::new(bytes), size).expect("mp4");
    let (tid, timescale, sps, pps, count) = {
        let t = mp4
            .tracks()
            .values()
            .find(|t| matches!(t.media_type(), Ok(mp4::MediaType::H264)))
            .expect("no H.264 track");
        (
            t.track_id(),
            t.timescale() as i64,
            t.sequence_parameter_set().expect("sps").to_vec(),
            t.picture_parameter_set().expect("pps").to_vec(),
            t.sample_count(),
        )
    };
    let mut out = Vec::with_capacity(count as usize);
    for sid in 1..=count {
        let s = mp4.read_sample(tid, sid).expect("read").expect("sample");
        let pts_us = (s.start_time as i64 + s.rendering_offset as i64) * 1_000_000 / timescale;
        let mut au = Vec::with_capacity(s.bytes.len() + 64);
        if s.is_sync {
            au.extend_from_slice(&START);
            au.extend_from_slice(&sps);
            au.extend_from_slice(&START);
            au.extend_from_slice(&pps);
        }
        let mut i = 0usize;
        while i + 4 <= s.bytes.len() {
            let n =
                u32::from_be_bytes([s.bytes[i], s.bytes[i + 1], s.bytes[i + 2], s.bytes[i + 3]])
                    as usize;
            i += 4;
            if n == 0 || i + n > s.bytes.len() {
                break;
            }
            au.extend_from_slice(&START);
            au.extend_from_slice(&s.bytes[i..i + n]);
            i += n;
        }
        out.push((au, pts_us));
    }
    out
}

/// Mean R/G/B over an RGBA buffer.
fn mean_rgb(rgba: &[u8]) -> (f64, f64, f64) {
    let n = (rgba.len() / 4) as f64;
    let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
    for px in rgba.chunks_exact(4) {
        r += px[0] as u64;
        g += px[1] as u64;
        b += px[2] as u64;
    }
    (r as f64 / n, g as f64 / n, b as f64 / n)
}

/// Max absolute per-channel difference between two RGBA buffers of equal size,
/// and the fraction of pixels that differ at all.
fn diff(a: &[u8], b: &[u8]) -> (u8, f64) {
    let mut max = 0u8;
    let mut changed = 0u64;
    let n = (a.len() / 4) as f64;
    for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        let mut any = false;
        for c in 0..3 {
            let d = pa[c].abs_diff(pb[c]);
            max = max.max(d);
            any |= d != 0;
        }
        if any {
            changed += 1;
        }
    }
    (max, changed as f64 / n)
}

fn dump_png(dir: &str, name: &str, rgba: &[u8], w: u32, h: u32) {
    // Minimal: re-use the `png` crate that mp4 already pulls in transitively is
    // not guaranteed, so write a PPM (P6) which any viewer + ffmpeg reads.
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in rgba.chunks_exact(4) {
        out.extend_from_slice(&px[..3]);
    }
    let path = format!("{dir}/{name}.ppm");
    if let Err(e) = std::fs::write(&path, out) {
        eprintln!("dump {path}: {e}");
    } else {
        eprintln!("wrote {path}");
    }
}

#[test]
fn matrix_choice_changes_the_picture_and_601_vs_709_is_measurable() {
    let Ok(path) = std::env::var("WANDR_TEST_MP4") else {
        eprintln!("SKIP: set WANDR_TEST_MP4 to an H.264 .mp4");
        return;
    };
    let bytes = std::fs::read(&path).expect("read WANDR_TEST_MP4");
    let aus = demux(&bytes);

    // Software decode — matrix-independent, so this frame is identical to what
    // the hardware decoder would hand us.
    let reg = wandr_video::default_registry();
    let prefs = Preferences { no_hardware: true, ..Default::default() };
    let mut dec = reg
        .open_decoder(&DecoderParams { codec: Codec::H264, width: 0, height: 0 }, prefs)
        .expect("software H.264 decoder");

    // Pull ONE frame — a mid-clip inter-predicted one, so it is a real picture,
    // not a flat IDR. Feed until we have collected frame 150.
    const TARGET: usize = 150;
    let mut got = 0usize;
    let mut frame_i420: Option<Vec<u8>> = None;
    let mut dims = (0u32, 0u32);
    'feed: for (au, pts) in &aus {
        dec.decode(Chunk::new(au, *pts)).expect("decode");
        while let Some(f) = dec.next_frame() {
            if got == TARGET {
                let i = f.as_i420().expect("software frame is CPU");
                dims = (i.width, i.height);
                // Repack tightly so we can re-wrap it against each matrix.
                let (w, h) = (i.width as usize, i.height as usize);
                let (cw, ch) = ((i.width.div_ceil(2)) as usize, (i.height.div_ceil(2)) as usize);
                let mut buf = Vec::with_capacity(w * h + 2 * cw * ch);
                for row in 0..h {
                    let o = row * i.y_stride as usize;
                    buf.extend_from_slice(&i.y[o..o + w]);
                }
                for (plane, stride) in [(i.u, i.u_stride), (i.v, i.v_stride)] {
                    for row in 0..ch {
                        let o = row * stride as usize;
                        buf.extend_from_slice(&plane[o..o + cw]);
                    }
                }
                frame_i420 = Some(buf);
                break 'feed;
            }
            got += 1;
        }
    }
    let buf = frame_i420.expect("reached frame 150");
    let (w, h) = dims;
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (y_len, c_len) = ((w * h) as usize, (cw * ch) as usize);
    let as_ref = || wandr_video::I420Ref {
        y: &buf[..y_len],
        y_stride: w,
        u: &buf[y_len..y_len + c_len],
        u_stride: cw,
        v: &buf[y_len + c_len..y_len + 2 * c_len],
        v_stride: cw,
        width: w,
        height: h,
        timestamp_us: 0,
    };

    let convert = |color: ColorInfo| -> Vec<u8> {
        let mut out = Vec::new();
        wandr_video::i420_to_rgba_with(&as_ref(), color, &mut out).expect("convert");
        out
    };

    let bt601 = convert(ColorInfo { matrix: ColorMatrix::Bt601, full_range: false });
    let bt709 = convert(ColorInfo { matrix: ColorMatrix::Bt709, full_range: false });
    // What the resolution heuristic picks for this frame — the value the pipeline
    // actually used, since this clip signals no colour description.
    let picked = ColorInfo::for_resolution(w, h);
    let auto = convert(picked);

    let m601 = mean_rgb(&bt601);
    let m709 = mean_rgb(&bt709);
    eprintln!("frame {TARGET}: {w}x{h}");
    eprintln!("  BT.601 limited mean RGB: {:.2} {:.2} {:.2}", m601.0, m601.1, m601.2);
    eprintln!("  BT.709 limited mean RGB: {:.2} {:.2} {:.2}", m709.0, m709.1, m709.2);
    eprintln!(
        "  mean delta (709-601):    {:+.2} {:+.2} {:+.2}",
        m709.0 - m601.0,
        m709.1 - m601.1,
        m709.2 - m601.2
    );
    let (max_d, frac) = diff(&bt601, &bt709);
    eprintln!("  max per-channel |709-601|: {max_d}, pixels changed: {:.1}%", frac * 100.0);
    eprintln!(
        "  heuristic picked {:?} for {h}p — auto == {}",
        picked.matrix,
        if auto == bt709 { "BT.709" } else if auto == bt601 { "BT.601" } else { "neither?!" }
    );

    if let Ok(dir) = std::env::var("WANDR_DUMP_DIR") {
        dump_png(&dir, "frame150_bt601", &bt601, w, h);
        dump_png(&dir, "frame150_bt709", &bt709, w, h);
    }

    // The claims, now attributable to the matrix alone because it is ONE frame:
    //  1. 601 and 709 genuinely differ — the selection is not a no-op.
    assert!(max_d > 2, "BT.601 and BT.709 produced near-identical output (max delta {max_d})");
    assert!(frac > 0.5, "fewer than half the pixels changed between matrices ({:.1}%)", frac * 100.0);
    //  2. This 720p clip signals nothing, so the heuristic must land on BT.709,
    //     and the pipeline's auto output must equal the BT.709 conversion.
    assert_eq!(picked.matrix, ColorMatrix::Bt709, "720p should heuristically be BT.709");
    assert_eq!(auto, bt709, "pipeline output does not match the matrix it selected");
}
