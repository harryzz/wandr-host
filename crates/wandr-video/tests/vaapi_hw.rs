//! Task 117 M2 stage 3 — the VA-API HARDWARE decode backend, end to end.
//!
//! Unlike the other tests in this crate this one needs a MACHINE, not just a
//! build: a GPU with a VA-API H.264 VLD driver and a readable DRM render node.
//! Where that is missing it SKIPS rather than fails, because "no GPU here" is not
//! a defect — the registry falling back to software is the designed behaviour and
//! `tests/h264.rs` already covers it. It also needs a real Annex-B sample, since
//! nothing in this crate can produce H.264 that a hardware decoder will accept.
//!
//!   WANDR_TEST_H264=/path/to/bbb.h264 cargo test --features vaapi --test vaapi_hw -- --nocapture
//!
//! WHAT IT ACTUALLY CHECKS, and why each one is here:
//!   1. the registry hands out the HARDWARE backend, not a software one
//!   2. every access unit decodes — the count matches the input
//!   3. every timestamp tag comes back EXACTLY ONCE — the `I420Ref` contract's
//!      "carried through the codec unchanged". Deliberately NOT a monotonicity
//!      check, and that distinction is a finding rather than a detail: this sample
//!      is 247 B-frames deep, the tags we can synthesise from an elementary stream
//!      are indexed by BITSTREAM position (a DTS, not a PTS), and frames come out
//!      in DISPLAY order carrying the tag of the AU that decoded them. So the
//!      output is a permutation, by contract. The openh264 backend instead pairs
//!      the i-th input tag to the i-th output frame with a FIFO, which reorders
//!      the tags and looks monotonic — a different semantic for the same input.
//!      See `docs/` / the task ledger: the two backends must be reconciled, and
//!      the real fix is a container PTS (guest-side demux), not a codec change.
//!   4. the picture is not black, not uniform, and has believable chroma — the
//!      NV12→I420 de-interleave in the backend is new code, and a frame counter
//!      cannot tell a correct picture from a corrupt one. `WANDR_DUMP_DIR` writes
//!      PPMs so a human can look, which is how the M1 libvpx traps were caught.

#![cfg(feature = "vaapi")]

use wandr_video::{default_registry, Chunk, Codec, DecoderParams, Preferences};

/// Split Annex-B into ACCESS UNITS (one coded picture each): a new AU begins at a
/// VCL NAL when the current AU already holds one. Feeding NALs individually would
/// give several of them the same PTS and make the ordering check meaningless.
fn access_units(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            let sc = if i > 0 && buf[i - 1] == 0 { i - 1 } else { i };
            starts.push((sc, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let (mut aus, mut cur) = (Vec::new(), Vec::<u8>::new());
    let mut has_vcl = false;
    for (n, &(sc, payload)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map(|&(s, _)| s).unwrap_or(buf.len());
        let t = buf[payload] & 0x1f;
        let is_vcl = t == 1 || t == 5;
        if is_vcl && has_vcl {
            aus.push(std::mem::take(&mut cur));
            has_vcl = false;
        }
        cur.extend_from_slice(&buf[sc..end]);
        has_vcl |= is_vcl;
    }
    if !cur.is_empty() {
        aus.push(cur);
    }
    aus
}

fn dump_ppm(path: &str, y: &[u8], u: &[u8], v: &[u8], ys: u32, cs: u32, w: u32, h: u32) {
    // BT.601 limited range — what these VLD decoders emit for SD/HD H.264.
    let (w, h) = (w as usize, h as usize);
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for row in 0..h {
        for col in 0..w {
            let yy = y[row * ys as usize + col] as f32;
            let ci = (row / 2) * cs as usize + col / 2;
            let (cu, cv) = (u[ci] as f32 - 128.0, v[ci] as f32 - 128.0);
            let yv = (yy - 16.0) * 1.164;
            for c in [yv + 1.596 * cv, yv - 0.392 * cu - 0.813 * cv, yv + 2.017 * cu] {
                out.push(c.clamp(0.0, 255.0) as u8);
            }
        }
    }
    if let Err(e) = std::fs::write(path, out) {
        eprintln!("dump {path}: {e}");
    }
}

#[test]
fn vaapi_decodes_h264_in_hardware() {
    let Ok(sample) = std::env::var("WANDR_TEST_H264") else {
        eprintln!("SKIP: set WANDR_TEST_H264 to an Annex-B .h264 sample");
        return;
    };
    let input = std::fs::read(&sample).expect("read WANDR_TEST_H264");

    // 1. HARDWARE, or skip. `require_hardware` makes the registry refuse to fall
    //    back, so this cannot accidentally pass on a software decoder — which is
    //    the whole failure mode the fallback contract was written about.
    let reg = default_registry();
    let prefs = Preferences { require_hardware: true, ..Default::default() };
    let params = DecoderParams { codec: Codec::H264, width: 0, height: 0 };
    let Ok(mut dec) = reg.open_decoder(&params, prefs) else {
        eprintln!("SKIP: no VA-API H.264 hardware decoder on this machine");
        return;
    };
    // Reaching here IS the assertion: under `require_hardware` the registry
    // filters every `BackendKind::Software` candidate out, so a decoder came back
    // only because a hardware one opened it.

    let aus = access_units(&input);
    assert!(aus.len() > 30, "sample too short to be meaningful: {} AUs", aus.len());

    const FPS: i64 = 30;
    let pts_of = |i: usize| (i as i64) * 1_000_000 / FPS;

    let dump_dir = std::env::var("WANDR_DUMP_DIR").ok();
    let mut got = 0usize;
    let mut seen: Vec<i64> = Vec::new();
    let mut checked_picture = false;

    let drain = |dec: &mut Box<dyn wandr_video::Decoder>,
                 got: &mut usize,
                 seen: &mut Vec<i64>,
                 checked: &mut bool| {
        while let Some(f) = dec.next_frame() {
            // 3. TAG FIDELITY, not monotonicity — see the header. Record what came
            //    back; the permutation check happens once the stream is drained.
            seen.push(f.timestamp_us);
            if *got < 12 {
                eprintln!("  out[{got}] tag {}", f.timestamp_us);
            }

            // 4. THE PICTURE ITSELF. Mid-clip so it is an inter-predicted frame,
            //    not just an IDR — a decoder with broken references still produces
            //    a fine-looking first frame.
            if *got == 150 && !*checked {
                *checked = true;
                let n = (f.width * f.height) as usize;
                let mean: f64 = f.y[..n].iter().map(|&p| p as f64).sum::<f64>() / n as f64;
                let min = *f.y[..n].iter().min().unwrap();
                let max = *f.y[..n].iter().max().unwrap();
                assert!(mean > 16.0, "picture is black (mean luma {mean:.1})");
                assert!(max - min > 40, "picture is flat (luma {min}..{max}) — decode produced no detail");
                // Chroma must not be uniformly neutral: a de-interleave that put
                // everything in one plane, or dropped V, lands exactly there and
                // would still pass a luma-only check.
                let cn = (f.u_stride * f.height.div_ceil(2)) as usize;
                let uvar = f.u[..cn].iter().map(|&p| (p as i32 - 128).abs()).max().unwrap();
                let vvar = f.v[..cn].iter().map(|&p| (p as i32 - 128).abs()).max().unwrap();
                assert!(uvar > 8 && vvar > 8, "chroma is neutral (u {uvar}, v {vvar}) — NV12→I420 de-interleave suspect");
                eprintln!("frame 150: {}x{} mean luma {mean:.1} range {min}..{max} chroma u{uvar}/v{vvar}", f.width, f.height);
            }
            if let Some(d) = &dump_dir {
                if matches!(*got, 0 | 60 | 150 | 240) {
                    dump_ppm(
                        &format!("{d}/hw_frame_{got:04}.ppm"),
                        f.y, f.u, f.v, f.y_stride, f.u_stride, f.width, f.height,
                    );
                }
            }
            *got += 1;
        }
    };

    for (i, au) in aus.iter().enumerate() {
        dec.decode(Chunk::new(au, pts_of(i))).expect("decode access unit");
        drain(&mut dec, &mut got, &mut seen, &mut checked_picture);
    }
    // 2. EVERY AU. Without flush the frames the DPB holds for reordering never
    //    come out and the tail of the clip silently disappears.
    dec.flush().expect("flush");
    drain(&mut dec, &mut got, &mut seen, &mut checked_picture);

    eprintln!("HW decoded {got}/{} access units", aus.len());
    assert_eq!(got, aus.len(), "decoded {got} frames from {} access units", aus.len());

    // 3. EVERY TAG BACK, EXACTLY ONCE. `I420Ref::timestamp_us` is specified as the
    //    chunk's timestamp "carried through the codec unchanged", so the output
    //    multiset must equal the input multiset — nothing lost, nothing
    //    duplicated, nothing invented. Deliberately NOT a monotonicity check: this
    //    sample is 247 B-frames deep, so decode order != display order, and the
    //    tags we feed are indexed by BITSTREAM position (a DTS). Frames come out
    //    in display order carrying the tag of the AU that decoded them, which is
    //    correct-by-contract and NOT ascending. See the recorded finding.
    let mut want: Vec<i64> = (0..aus.len()).map(pts_of).collect();
    let mut have = seen.clone();
    want.sort_unstable();
    have.sort_unstable();
    assert_eq!(have, want, "timestamps were not carried through unchanged");
    let ascending = seen.windows(2).all(|w| w[1] > w[0]);
    eprintln!(
        "tag order out: {}  (input tags are bitstream-indexed, i.e. DTS-shaped)",
        if ascending { "ascending" } else { "PERMUTED — reordered to display order" }
    );
    assert!(checked_picture, "never reached frame 150 — picture never verified");

    // Seek: reset must drop reference state and accept a fresh keyframe.
    dec.reset().expect("reset");
    dec.decode(Chunk::new(&aus[0], 0)).expect("decode after reset");
    dec.flush().expect("flush after reset");
    assert!(dec.next_frame().is_some(), "no frame after reset + keyframe");
}
