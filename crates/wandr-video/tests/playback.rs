//! Task 117 M2 step 1 — prove the PLAYBACK contract, on desktop, with the codec
//! we already ship (VP9/libvpx). No new codec, no HW backend, no network.
//!
//! What "playback shape" means, and why the call shape could not do it:
//!   * presentation timestamps survive the codec (the call path carried a 90 kHz
//!     u32 transport clock that wraps every ~13.25 h);
//!   * `reset()` gives a seek that does NOT require tearing the decoder down;
//!   * `flush()` drains at end of stream rather than losing the tail;
//!   * a player can pace frames against an external clock and measure its own
//!     drift — i.e. A/V sync is expressible at all.
//!
//! These are deliberately about the CONTRACT, not about pixels — `roundtrip.rs`
//! already proves the pixels.

use std::collections::VecDeque;

use wandr_video::{
    open_decoder, open_encoder, Chunk, Codec, DecoderParams, EncoderParams, Rgb24Frame,
};

const W: u32 = 320;
const H: u32 = 240;
const FPS: i64 = 30;
/// A real file's PTS does not start at zero and is not frame-index-shaped. Use an
/// awkward origin so a "works because everything is 0" bug cannot pass.
const PTS_ORIGIN_US: i64 = 7_123_456;

fn frame_pts_us(i: i64) -> i64 {
    PTS_ORIGIN_US + (i * 1_000_000) / FPS
}

fn test_frame(t: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (W * H * 3) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 3) as usize;
            buf[i] = ((x * 255 / W) as u8).wrapping_add(t as u8);
            buf[i + 1] = (y * 255 / H) as u8;
            buf[i + 2] = if (x / 16 + y / 16) % 2 == 0 { 220 } else { 30 };
        }
    }
    buf
}

/// An encoded "file": compressed frames each with the PTS it must be shown at.
struct Clip {
    frames: Vec<(Vec<u8>, i64, bool)>, // (data, pts_us, keyframe)
}

fn encode_clip(codec: Codec, n: i64, keyframe_every: i64) -> Clip {
    let mut enc = open_encoder(&EncoderParams {
        codec,
        width: W,
        height: H,
        bitrate_bps: 1_000_000,
        framerate: FPS as u32,
    })
    .expect("open encoder");

    let mut frames = Vec::new();
    for i in 0..n {
        let src = test_frame(i as u32);
        let force_kf = i % keyframe_every == 0;
        enc.encode(Rgb24Frame::new(&src, W, H), force_kf).expect("encode");
        while let Some(pkt) = enc.next_packet() {
            frames.push((pkt.data, frame_pts_us(i), pkt.keyframe));
        }
    }
    assert!(frames.len() as i64 >= n / 2, "encoder produced too few packets");
    Clip { frames }
}

/// THE core assertion: a presentation timestamp handed in comes back out on the
/// decoded frame, unchanged and correctly paired. Everything a player does —
/// seekbar, A/V sync, frame dropping — is downstream of this.
#[test]
fn pts_survives_the_codec_and_pairs_correctly() {
    let clip = encode_clip(Codec::Vp9, 20, 10);
    let mut dec = open_decoder(&DecoderParams { codec: Codec::Vp9, width: W, height: H })
        .expect("open decoder");

    let mut got: Vec<i64> = Vec::new();
    for (data, pts, _) in &clip.frames {
        dec.decode(Chunk::new(data, *pts)).expect("decode");
        while let Some(f) = dec.next_frame() {
            got.push(f.timestamp_us());
        }
    }
    dec.flush().expect("flush");
    while let Some(f) = dec.next_frame() {
        got.push(f.timestamp_us());
    }

    let want: Vec<i64> = clip.frames.iter().map(|(_, p, _)| *p).collect();
    assert!(!got.is_empty(), "no frames decoded");
    // Exact equality, in order. A FIFO-pairing implementation would drift here
    // the first time a packet produced zero frames.
    assert_eq!(got, want[..got.len()], "PTS did not survive the codec unchanged");
    // And the values must be the awkward real-file ones, not 0..n.
    assert_eq!(got[0], PTS_ORIGIN_US, "first PTS was rewritten");
}

/// `flush()` must surface the tail and lose nothing.
///
/// ‼️ HONEST LIMITATION — this test CANNOT currently detect a no-op `flush()`.
/// Verified by injecting one: all tests still passed. The reason is real rather
/// than a test defect: our encoder runs `g_lag_in_frames = 0` with realtime CBR,
/// so VP9 emits no alt-ref/hidden frames and the decoder never holds a tail
/// back — there is genuinely nothing for flush to drain in this stream.
///
/// So this is a SMOKE test (flush is callable, idempotent, and loses no frames),
/// not a proof. A real proof needs a stream whose decoder holds frames — a codec
/// with B-frames (H.264, M2 step 2) or an encoder configured with lag > 0.
/// Re-point this test at one of those when it exists.
#[test]
fn flush_is_safe_and_loses_no_frames() {
    let clip = encode_clip(Codec::Vp9, 12, 12);
    let mut dec = open_decoder(&DecoderParams { codec: Codec::Vp9, width: W, height: H })
        .expect("open decoder");

    let mut before = 0usize;
    for (data, pts, _) in &clip.frames {
        dec.decode(Chunk::new(data, *pts)).expect("decode");
        while dec.next_frame().is_some() {
            before += 1;
        }
    }
    dec.flush().expect("flush");
    let mut after = before;
    while dec.next_frame().is_some() {
        after += 1;
    }
    assert_eq!(after, clip.frames.len(), "flush did not account for every frame");
    // Idempotent: a player may flush on EOS and again on teardown.
    dec.flush().expect("second flush");
    assert!(dec.next_frame().is_none(), "second flush invented a frame");
}

/// A seek: `reset()` then feed from a keyframe. Proves the SEEK works — the
/// decoder resumes at the target, PTS is correct afterwards, and it never has to
/// be reopened.
///
/// ‼️ HONEST LIMITATION — like `flush`, this cannot detect a no-op `reset()`
/// on this backend. Verified by injecting one; everything still passed. That is
/// not a test defect, it is what libvpx is: a VP8/VP9 keyframe resets all
/// references by definition, so once the caller honours the contract ("feed a
/// keyframe after reset") there is no observable difference. Probing with a
/// delta frame instead does not help — measured, libvpx rejects an out-of-order
/// delta with BadFrame whether or not reset ran, for an unrelated reason.
///
/// `reset()` earns its place on backends that queue work asynchronously, where
/// it maps to a real discard (`AMediaCodec_flush` drops in-flight buffers). So
/// the verb is validated in M2 step 2+ on MediaCodec, NOT here. What this test
/// legitimately proves is that seek-by-reset is correct and cheap.
#[test]
fn reset_seeks_without_reopening_the_decoder() {
    let clip = encode_clip(Codec::Vp9, 30, 10);
    let mut dec = open_decoder(&DecoderParams { codec: Codec::Vp9, width: W, height: H })
        .expect("open decoder");

    // Play the first few frames.
    for (data, pts, _) in clip.frames.iter().take(5) {
        dec.decode(Chunk::new(data, *pts)).expect("decode");
        while dec.next_frame().is_some() {}
    }

    // Seek: reset, then resume from the next keyframe.
    dec.reset().expect("reset");
    assert!(dec.next_frame().is_none(), "reset left a frame queued");

    let seek_idx = clip
        .frames
        .iter()
        .skip(6)
        .position(|(_, _, k)| *k)
        .map(|i| i + 6)
        .expect("clip has no later keyframe to seek to");

    let mut after_seek = Vec::new();
    for (data, pts, _) in clip.frames.iter().skip(seek_idx) {
        dec.decode(Chunk::new(data, *pts)).expect("decode after seek");
        while let Some(f) = dec.next_frame() {
            after_seek.push(f.timestamp_us());
        }
    }

    assert!(!after_seek.is_empty(), "no frames decoded after seek");
    let seek_pts = clip.frames[seek_idx].1;
    assert_eq!(after_seek[0], seek_pts, "first post-seek frame is not the seek target");
    assert!(
        after_seek.iter().all(|&p| p >= seek_pts),
        "a pre-seek frame leaked through reset()"
    );
}

/// The whole point: can a player actually SCHEDULE these frames against a clock?
///
/// Simulates the guest-side sync model — an external media clock (which in the
/// real player is `wasi:audio playback.position()`), with the decoder run ahead
/// of it. Asserts every frame is presentable at its PTS with bounded error, i.e.
/// nothing about the contract makes sync impossible.
#[test]
fn frames_can_be_paced_against_an_external_clock() {
    let clip = encode_clip(Codec::Vp9, 40, 20);
    let mut dec = open_decoder(&DecoderParams { codec: Codec::Vp9, width: W, height: H })
        .expect("open decoder");

    // Decode ahead into a small queue, exactly as a player's decode thread would.
    let mut queue: VecDeque<i64> = VecDeque::new();
    for (data, pts, _) in &clip.frames {
        dec.decode(Chunk::new(data, *pts)).expect("decode");
        while let Some(f) = dec.next_frame() {
            queue.push_back(f.timestamp_us());
        }
    }
    dec.flush().expect("flush");
    while let Some(f) = dec.next_frame() {
        queue.push_back(f.timestamp_us());
    }
    let decoded = queue.len();
    assert!(decoded >= 30, "not enough frames to pace: {decoded}");

    // Walk a synthetic media clock and present whatever is due. `worst` is the
    // largest gap between when a frame was due and when the clock reached it.
    let start = queue[0];
    let mut worst = 0i64;
    let mut presented = 0usize;
    let step_us = 1_000_000 / 60; // a 60 Hz render loop
    let mut clock = start;
    while let Some(&due) = queue.front() {
        if due <= clock {
            worst = worst.max(clock - due);
            queue.pop_front();
            presented += 1;
        } else {
            clock += step_us;
        }
    }

    // Every decoded frame must actually get presented — a pacing loop that
    // silently strands frames in the queue is the failure this guards.
    assert_eq!(presented, decoded, "pacing loop did not present every frame");
    // A 30 fps clip on a 60 Hz loop: a frame can be at most one tick late.
    assert!(
        worst <= step_us,
        "frame pacing error {worst} us exceeds one 60 Hz tick ({step_us} us) — \
         the contract cannot express A/V sync"
    );
}

/// PTS must not be mangled by large/awkward values — a 3-hour offset is a normal
/// thing to see in a real file, and is where a 32-bit or 90 kHz clock would wrap.
#[test]
fn pts_survives_values_that_would_wrap_a_90khz_u32() {
    // 3 hours in µs. In 90 kHz ticks this is ~972,000,000 — under u32::MAX, but
    // 13.25 h would wrap it. Use a value past the u32 90 kHz wrap to be sure.
    const BIG_US: i64 = 14 * 3600 * 1_000_000;
    let clip = encode_clip(Codec::Vp9, 4, 4);
    let mut dec = open_decoder(&DecoderParams { codec: Codec::Vp9, width: W, height: H })
        .expect("open decoder");

    let mut got = Vec::new();
    for (i, (data, _, _)) in clip.frames.iter().enumerate() {
        let pts = BIG_US + i as i64 * 33_333;
        dec.decode(Chunk::new(data, pts)).expect("decode");
        while let Some(f) = dec.next_frame() {
            got.push(f.timestamp_us());
        }
    }
    assert!(!got.is_empty());
    assert!(got[0] >= BIG_US, "large PTS was truncated: {} < {BIG_US}", got[0]);
}
