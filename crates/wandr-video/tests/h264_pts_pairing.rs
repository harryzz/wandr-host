//! Task 117 M2 — DOES THE H.264 BACKEND PAIR TIMESTAMPS TO THE RIGHT FRAMES?
//!
//! This exists because every counter we had was blind to the answer. The host
//! keeps a PTS-ordered reorder buffer (`video_desktop.rs::queue_decoded` inserts
//! by `partition_point` on `pts_us`), so whatever timestamps a codec reports, the
//! guest receives them in ascending order BY CONSTRUCTION. A backend that hands
//! frame A the timestamp of frame B therefore produces a stream that is perfectly
//! monotonic and shows the pictures in the WRONG ORDER — and the player's
//! `out-of-order` counter reads zero the whole time.
//!
//! At this level nothing sorts: `Decoder::next_frame` returns exactly what the
//! codec paired. That makes the raw PTS sequence the evidence.
//!
//! The input has to be an MP4, not an elementary stream: the question is what a
//! decoder does with REAL presentation times, and only a container has any.
//!
//!   WANDR_TEST_MP4=/path/to/bbb-h264.mp4 \
//!     cargo test --features openh264 --test h264_pts_pairing -- --nocapture

#![cfg(feature = "openh264")]

use wandr_video::{Chunk, Codec, DecoderParams, Preferences};

const START: [u8; 4] = [0, 0, 0, 1];

/// MP4 → (Annex-B access unit, true PTS in µs), in FILE (decode) order.
fn demux(bytes: &[u8]) -> (Vec<(Vec<u8>, i64)>, bool) {
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
    let mut reordered = false;
    for sid in 1..=count {
        let s = mp4.read_sample(tid, sid).expect("read").expect("sample");
        reordered |= s.rendering_offset != 0;
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
    (out, reordered)
}

#[test]
fn openh264_pairs_container_pts_to_the_right_frames() {
    let Ok(path) = std::env::var("WANDR_TEST_MP4") else {
        eprintln!("SKIP: set WANDR_TEST_MP4 to an H.264 .mp4");
        return;
    };
    let bytes = std::fs::read(&path).expect("read WANDR_TEST_MP4");
    let (aus, reordered) = demux(&bytes);
    assert!(reordered, "sample has no composition offsets — it cannot answer this question");
    eprintln!("{}: {} samples, container HAS reordering (ctts != 0)", path, aus.len());

    // Software backend explicitly: this is a question about openh264, and on a box
    // with a HW backend the registry would otherwise hand us that instead.
    let reg = wandr_video::default_registry();
    let prefs = Preferences { no_hardware: true, ..Default::default() };
    let mut dec = reg
        .open_decoder(&DecoderParams { codec: Codec::H264, width: 0, height: 0 }, prefs)
        .expect("open software H.264 decoder");

    let mut seen: Vec<i64> = Vec::new();
    for (au, pts) in &aus {
        dec.decode(Chunk::new(au, *pts)).expect("decode");
        while let Some(f) = dec.next_frame() {
            seen.push(f.timestamp_us);
        }
    }
    dec.flush().expect("flush");
    while let Some(f) = dec.next_frame() {
        seen.push(f.timestamp_us);
    }

    eprintln!("first 12 PTS out: {:?}", &seen[..seen.len().min(12)]);
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    let mut want: Vec<i64> = aus.iter().map(|(_, p)| *p).collect();
    want.sort_unstable();
    assert_eq!(sorted, want, "timestamps lost/duplicated, not merely reordered");

    // THE QUESTION. Fed real presentation times, a decoder that pairs correctly
    // and emits in display order returns them ASCENDING. Anything else means the
    // timestamp travelling with each picture is not that picture's own — which the
    // host's reorder buffer would then faithfully sort by, scrambling the video
    // while every counter reads clean.
    let ascending = seen.windows(2).all(|w| w[1] > w[0]);
    let inversions = seen.windows(2).filter(|w| w[1] <= w[0]).count();
    eprintln!(
        "PTS out is {} ({inversions} inversions of {} frames)",
        if ascending { "ASCENDING — pairing correct" } else { "NOT ascending — MISPAIRED" },
        seen.len()
    );
    assert!(
        ascending,
        "openh264 mispairs container PTS: {inversions} inversions. Its FIFO hands the i-th \
         INPUT timestamp to the i-th OUTPUT frame, which is only valid when decode order == \
         display order. See the module header in backends/openh264.rs."
    );
}
